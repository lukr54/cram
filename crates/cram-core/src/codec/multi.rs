//! Parallel decode of a file that is a **run of independent compressed streams**.
//!
//! Our chunked tar writer ([`crate::formats::tar_write`]) compresses on every core by cutting the
//! tar into chunks, compressing each into a complete standalone stream, and concatenating them in
//! order. Reading that back walked the whole run through one decoder on one thread, so a `.tar.bz2`
//! that took 40 s to write took 61 s to read while 23 cores sat idle.
//!
//! The seams are still there and they are findable. This module locates them, decodes the spans
//! between them on a pool, and yields the bytes in order through a [`Read`], which is what the tar
//! worker already consumes. Measured on the dev box against a cram-written kernel tree `.tar.bz2`
//! (497 MB, 477 streams): `bunzip2` serial 33.4 s, the same file split at its boundaries and decoded
//! 24 at a time 2.15 s.
//!
//! **This is a reader improvement and not a format change.** The scan looks for what the *format*
//! says a stream start is, so it works on any archive of concatenated streams — `pbzip2`, `lbzip2`,
//! `cat a.xz b.xz`, a Wikipedia multistream dump — and an archive it cannot split simply falls back
//! to the sequential decoder. Nothing about what we *write* changes.
//!
//! **Safety of the split.** A false boundary cannot corrupt an extraction silently: the span before
//! it is a truncated stream and the span after it starts mid-data, so both fail to decode and the
//! error surfaces. A missed boundary costs parallelism and nothing else. The checks below are
//! nonetheless strong enough that neither is expected — a bzip2 candidate must carry 72 bits of
//! header *and* be preceded by the previous stream's end-of-stream magic, and an xz candidate must
//! carry a header whose CRC32 checks out *and* be preceded by a stream footer.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::codec::decode_stream;
use crate::format::Codec as StreamCodec;

/// bzip2's start-of-block magic (the first 48 bits of pi), which follows `BZh` + a level digit at
/// the head of every stream. Byte-aligned there, unlike the block magics inside a stream.
const BZ_BLOCK_MAGIC: [u8; 6] = [0x31, 0x41, 0x59, 0x26, 0x53, 0x59];

/// bzip2's end-of-stream magic (sqrt(pi)). Bit-aligned — the preceding blocks decide its offset —
/// so it is searched at all eight alignments rather than compared as bytes.
const BZ_EOS_MAGIC: u64 = 0x1772_4538_5090;

/// xz's stream header magic.
const XZ_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];

/// Bytes before a candidate that the preceding-stream check may look at. bzip2 needs 11 (48 bits of
/// magic, 32 of CRC, up to 7 of padding); xz needs 2 for the `YZ` that ends a footer plus whatever
/// stream padding follows it, which is a multiple of four zero bytes and in practice none.
const LOOKBACK: usize = 20;

/// Bytes read per scan window.
const SCAN_WIN: usize = 4 << 20;

/// How far to read before giving up on a file that shows no second stream.
///
/// Without this the scan reads the **whole file** to discover it cannot be split, and that is a
/// straight loss on every single-stream archive — measured at 0.47 s on a 763 MB `.tar.lz4`, which
/// is a 13% regression on an extraction that gains nothing in return. Our own chunks compress to at
/// most ~8 MB (xz's 32 MiB), and `pbzip2`/`lbzip2` are far finer, so a second boundary inside 64 MiB
/// is a very safe bet. A foreign archive with streams larger than that reads sequentially, which is
/// what it did before this module existed.
const PROBE: u64 = 64 << 20;

/// Decoded bytes per message. Matches the tar backend's own chunk, and bounds nothing on its own —
/// the slot count and the piece window do that. See [`shape`].
const CHUNK: usize = 1024 * 1024;

/// Bounds on the decoded chunks a worker may run ahead by *within its piece*. See [`shape`] — this
/// is the number that decides whether the pool works at all, not the worker count.
const MIN_SLOTS: usize = 4;
const MAX_SLOTS: usize = 64;

/// Decoded bytes the pool may hold at once: `width × slots × CHUNK` is held to this.
///
/// 256 MiB rather than something proportional to the machine. This is a memory bound, and [`shape`]
/// spends it on buffer depth before width, because depth is what decides whether the pool works at
/// all.
///
/// **The arithmetic that used to justify the number is withdrawn.** It rested on the tar consumer
/// writing ~95,000 files in about 5.5 s whatever fed it, and therefore on three workers being enough
/// to hide a 15 CPU-second decode behind it. That floor is 1.19 s: the 16 August 2026 run extracts
/// the 94,778-file kernel tree from a plain `.tar` in 1.19 s, on one machine, medians of 3, to
/// `/dev/shm` with the archive read into page cache first. A floor that low asks for more workers
/// than three, not fewer. The budget has not been re-derived against it. The shipping end-to-end
/// figures from the same run are 3.14 s for `.tar.bz2` and 2.85 s for `.tar.xz`.
const BUDGET: u64 = 256 << 20;

/// Assumed decompression ratio, used only to guess how many chunks a piece will decode to.
///
/// Guessing is enough because both errors are cheap: too high wastes some of the budget on slots a
/// piece never fills, too low costs parallelism on the tail of each piece and nothing else. The
/// kernel tree lands at 4.2 for bzip2 and 4.6 for xz, so this is those with headroom.
const TYPICAL_RATIO: u64 = 6;

/// Read-ahead buffer under each worker's slice of the file.
const READ_BUF: usize = 256 << 10;

/// Smallest span worth handing a worker. Adjacent streams are merged up to this, because a run of
/// tiny streams costs more in handoffs than it buys in parallelism — and because a file with
/// millions of them would otherwise want millions of channels.
///
/// 256 KiB compressed is on the order of a megabyte decoded, which is milliseconds of work against
/// microseconds of handoff. Our own bzip2 chunks land at ~650 KB and xz's at several MB, so this
/// merges nothing we write and exists for foreign archives with unusually fine seams.
const MIN_PIECE: u64 = 256 << 10;

/// Ceiling on the number of pieces, enforced by merging rather than by refusing: a 250 GB archive
/// still parallelises, its pieces are just larger.
const MAX_PIECES: usize = 1 << 16;

/// A codec whose members are complete, byte-aligned, self-contained streams that concatenate into a
/// valid file of the same codec — so a span covering whole members decodes on its own, and a span
/// covering several decodes as one.
///
/// The other four are not here, and each for its own reason. **zstd** and **lz4** we write as one
/// frame (`zstdmt` does its own threading inside it), so there is nothing to find. **gzip** we write
/// as one member with the deflate blocks chunked inside it, which is what makes it a legal `.gz` for
/// every other tool; splitting that needs an index rather than a scan (T3). **brotli** has no
/// multi-stream concept at all: two brotli streams end to end are not a brotli stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Separable {
    Bzip2,
    Xz,
}

impl Separable {
    fn of(codec: StreamCodec) -> Option<Self> {
        match codec {
            StreamCodec::Bzip2 => Some(Separable::Bzip2),
            StreamCodec::Xz => Some(Separable::Xz),
            _ => None,
        }
    }

    /// Bytes a candidate needs in front of it before it can be tested.
    fn probe(self) -> usize {
        match self {
            // "BZh" + level digit + the 48-bit block magic.
            Separable::Bzip2 => 10,
            // 6-byte magic + 2 flag bytes + the CRC32 of those flags.
            Separable::Xz => 12,
        }
    }

    /// First byte of a stream, the one the scan searches for.
    fn lead(self) -> u8 {
        match self {
            Separable::Bzip2 => b'B',
            Separable::Xz => 0xFD,
        }
    }

    /// Does a stream begin at `buf[at]`? `at + self.probe() <= buf.len()` is the caller's job.
    ///
    /// Two independent tests, and the second is what makes a false positive unreachable in practice:
    /// the bytes *before* a stream start are the end of the previous stream, and both formats end
    /// with something recognisable.
    fn starts_at(self, buf: &[u8], at: usize) -> bool {
        let head = &buf[at..at + self.probe()];
        let tail = &buf[at.saturating_sub(LOOKBACK)..at];
        match self {
            Separable::Bzip2 => {
                head[..3] == *b"BZh"
                    && (b'1'..=b'9').contains(&head[3])
                    && head[4..10] == BZ_BLOCK_MAGIC
                    && (at == 0 || has_bz_eos(tail))
            }
            Separable::Xz => {
                head[..6] == XZ_MAGIC
                    && crc32fast::hash(&head[6..8])
                        == u32::from_le_bytes([head[8], head[9], head[10], head[11]])
                    && (at == 0 || ends_xz_stream(tail))
            }
        }
    }
}

/// Is bzip2's 48-bit end-of-stream magic present anywhere in `tail`, at any bit alignment? A stream
/// ends with that magic, a 32-bit CRC and zero padding to a byte boundary, so in the 11 bytes before
/// the next stream it sits at an offset the preceding blocks' bit lengths decide.
fn has_bz_eos(tail: &[u8]) -> bool {
    let bits = tail.len() * 8;
    if bits < 48 {
        return false;
    }
    for start in 0..=(bits - 48) {
        let mut v = 0u64;
        for i in 0..48 {
            let b = start + i;
            v = (v << 1) | u64::from((tail[b / 8] >> (7 - (b % 8))) & 1);
        }
        if v == BZ_EOS_MAGIC {
            return true;
        }
    }
    false
}

/// Do these bytes end an xz stream? A footer's last two bytes are always `YZ`, and streams may be
/// separated by *stream padding*, which the spec defines as a multiple of four zero bytes.
fn ends_xz_stream(tail: &[u8]) -> bool {
    let pad = tail.len() - tail.iter().rev().take_while(|&&b| b == 0).count();
    (tail.len() - pad).is_multiple_of(4) && tail[..pad].ends_with(b"YZ")
}

/// One independently-decodable span of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
    start: u64,
    len: u64,
}

/// Where a file's streams begin, computed once and reused. A compressed tar is opened **twice** for
/// an extraction — a header-only pass to build the listing, then the extraction pass — and both
/// decode every byte, so scanning once and sharing the answer halves the scan.
pub(crate) struct Plan {
    path: PathBuf,
    codec: StreamCodec,
    pieces: Vec<Piece>,
}

/// Scan `path` for stream boundaries, or `None` if it is not a run of streams we can split.
///
/// Every failure here is a fallback and not an error: an unreadable file, a codec that is one
/// stream, a file that does not begin with a stream header. The sequential decoder runs next and
/// reports whatever the real problem is, so this must not be the thing that names it.
pub(crate) fn plan(path: &Path, codec: StreamCodec) -> Option<Arc<Plan>> {
    plan_with(path, codec, MIN_PIECE)
}

/// [`plan`] with an explicit minimum piece, so the end-to-end tests can exercise a real pool without
/// synthesizing the tens of megabytes of incompressible data the shipping value would need.
fn plan_with(path: &Path, codec: StreamCodec, min_piece: u64) -> Option<Arc<Plan>> {
    if !enabled() {
        return None;
    }
    let kind = Separable::of(codec)?;
    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let bounds = boundaries(BufReader::with_capacity(READ_BUF, file), kind).ok()?;
    let pieces = merge(&bounds, len, min_piece);
    if pieces.len() < 2 {
        return None;
    }
    Some(Arc::new(Plan {
        path: path.to_path_buf(),
        codec,
        pieces,
    }))
}

/// `CRAM_PARALLEL_DECODE=0` forces the sequential decoder, for A/B measurement and as an escape
/// hatch if a stream layout we have not seen ever confuses the scan.
fn enabled() -> bool {
    !matches!(
        std::env::var("CRAM_PARALLEL_DECODE").as_deref(),
        Ok("0") | Ok("off") | Ok("false")
    )
}

/// Offsets at which a stream begins. Empty if the file does not start with one, which is the signal
/// that this is not a layout we handle rather than that the file is bad.
fn boundaries<R: Read>(mut src: R, kind: Separable) -> io::Result<Vec<u64>> {
    let probe = kind.probe();
    let carry = LOOKBACK + probe;
    let mut buf = vec![0u8; SCAN_WIN];
    let mut out: Vec<u64> = Vec::new();
    let mut base: u64 = 0; // file offset of buf[0]
    let mut have: usize = 0;
    let mut from: usize = 0;
    let mut first = true;

    loop {
        while have < buf.len() {
            match src.read(&mut buf[have..]) {
                Ok(0) => break,
                Ok(n) => have += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let eof = have < buf.len();
        if first {
            first = false;
            if have < probe || !kind.starts_at(&buf[..have], 0) {
                return Ok(Vec::new()); // not a run of streams; fall back
            }
            out.push(0);
            from = 1;
        }
        if have >= probe {
            // A candidate at `p` is testable while `p + probe <= have`.
            let stop = have + 1 - probe;
            let mut p = from;
            while p < stop {
                let Some(off) = buf[p..stop].iter().position(|&b| b == kind.lead()) else {
                    break;
                };
                let q = p + off;
                if kind.starts_at(&buf[..have], q) {
                    out.push(base + q as u64);
                }
                p = q + 1;
            }
        }
        if eof {
            return Ok(out);
        }
        // Nothing but the file's own start in the first [`PROBE`] bytes → not a run of streams.
        // Stopping here is what keeps a single-stream archive from paying for a whole extra read.
        if out.len() < 2 && base + have as u64 >= PROBE {
            return Ok(out);
        }
        // Carry enough that neither a header nor the lookback before it straddles two reads, and
        // resume the scan exactly where it stopped so nothing is tested twice.
        let keep = have - carry;
        buf.copy_within(keep..have, 0);
        base += keep as u64;
        from = carry + 1 - probe;
        have = carry;
    }
}

/// Turn boundaries into work items, fusing adjacent streams until each piece is worth a handoff.
///
/// Fusing is free because a span of *several* whole streams is itself a valid stream for both
/// codecs — the same property that lets the writer concatenate them in the first place.
fn merge(bounds: &[u64], len: u64, min_piece: u64) -> Vec<Piece> {
    if bounds.is_empty() {
        return Vec::new();
    }
    let want = (len / MAX_PIECES as u64).max(min_piece);
    let mut out: Vec<Piece> = Vec::new();
    let mut start = bounds[0];
    for &b in &bounds[1..] {
        if b - start >= want {
            out.push(Piece {
                start,
                len: b - start,
            });
            start = b;
        }
    }
    if len > start {
        out.push(Piece {
            start,
            len: len - start,
        });
    }
    out
}

/// How wide the pool runs, and how much of its piece each worker may buffer.
///
/// **The second number is the one that decides whether this works.** A worker that cannot buffer a
/// whole piece decodes the part that fits, blocks, and then feeds the rest to the consumer at
/// one-worker speed when its turn comes — so the speedup is capped at `piece / buffer` however many
/// workers are running. Measured on the kernel tree with a fixed four-chunk buffer: bzip2, whose
/// pieces are 4 MiB, went 61.60 s → 8.85 s at 995% CPU, while xz, whose pieces are 32 MiB, went
/// 20.68 s → 17.86 s at 149%. 32/4 predicts 1.14× and 1.16× is what it did.
///
/// **Those four numbers are the rejected configuration, not the shipping one.** The fixed four-chunk
/// buffer is exactly what this function exists to replace, so 8.85 and 17.86 are the cost of the
/// mistake rather than current figures. What ships measures 3.14 s for `.tar.bz2` and 2.85 s for
/// `.tar.xz` on the same tree: 16 August 2026, one machine, medians of 3, `/dev/shm` destination
/// with the archive read into page cache first.
///
/// So size the buffer against the piece first and let the width take what is left of the budget.
/// The two trade directly. Which one to give up is no longer settled by the argument that used to
/// close this paragraph — "three or four workers already hide a decode behind the tar consumer" —
/// because that consumer was taken to cost ~5.5 s and costs 1.19 s. See [`BUDGET`].
///
/// `CRAM_WORKERS` caps the width, as everywhere else in the engine. The RAM fraction is the same
/// 60% [`crate::formats::tar_write`] uses, and binds only on a machine with under ~430 MiB free.
fn shape(pieces: &[Piece]) -> (usize, usize) {
    let threads = if let Some(n) = std::env::var("CRAM_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        n
    } else {
        rayon::current_num_threads().max(1)
    };
    let total: u64 = pieces.iter().map(|p| p.len).sum();
    let mean = total / pieces.len().max(1) as u64;
    let slots =
        ((mean.saturating_mul(TYPICAL_RATIO) / CHUNK as u64) as usize).clamp(MIN_SLOTS, MAX_SLOTS);

    let avail = crate::hw::HwProfile::detect().ram_avail;
    let budget = if avail == 0 {
        BUDGET
    } else {
        BUDGET.min(((avail as f64) * 0.6) as u64)
    };
    let per_worker = (slots as u64).saturating_mul(CHUNK as u64);
    let width = ((budget / per_worker.max(1)) as usize)
        .clamp(1, threads)
        .min(pieces.len());
    (width, slots)
}

/// Which pieces may be in flight, and the stop flag, in one lock.
///
/// The window is what bounds memory, and it has to be released by the **consumer** rather than by a
/// worker finishing: a worker that finished a piece and moved on leaves that piece's decoded bytes
/// sitting in its channel, so bounding the workers alone bounds nothing. Claims are handed out in
/// increasing order, which is also what makes the bound deadlock-free — the piece the consumer is
/// waiting on was claimed before any piece ahead of it, so it is always inside the window.
struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
}

struct GateState {
    /// Piece the consumer is currently reading.
    cur: usize,
    /// Next unclaimed piece.
    next: usize,
    stop: bool,
}

impl Gate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                cur: 0,
                next: 0,
                stop: false,
            }),
            cv: Condvar::new(),
        }
    }

    /// Take the next piece, waiting while it would run further than `window` ahead of the consumer.
    fn claim(&self, window: usize, total: usize) -> Option<usize> {
        let mut s = self.state.lock().ok()?;
        loop {
            if s.stop || s.next >= total {
                return None;
            }
            if s.next < s.cur + window {
                let i = s.next;
                s.next += 1;
                return Some(i);
            }
            s = self.cv.wait(s).ok()?;
        }
    }

    fn advance(&self, cur: usize) {
        if let Ok(mut s) = self.state.lock() {
            s.cur = cur;
            self.cv.notify_all();
        }
    }

    fn stop(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.stop = true;
            self.cv.notify_all();
        }
    }
}

enum Msg {
    Chunk(Vec<u8>),
    End,
    Err(String),
}

/// The pieces of a file, decoded on a pool and yielded in order.
struct ParallelStreams {
    rx: Vec<Receiver<Msg>>,
    cur: usize,
    cursor: io::Cursor<Vec<u8>>,
    gate: Arc<Gate>,
    workers: Vec<thread::JoinHandle<()>>,
    done: bool,
}

/// Open a plan's file as one decoded byte stream, decoded in parallel.
pub(crate) fn open(plan: &Arc<Plan>) -> Box<dyn Read + Send> {
    let total = plan.pieces.len();
    let (width, slots) = shape(&plan.pieces);
    let gate = Arc::new(Gate::new());

    let mut rx = Vec::with_capacity(total);
    let mut tx = Vec::with_capacity(total);
    for _ in 0..total {
        let (s, r) = sync_channel::<Msg>(slots);
        tx.push(s);
        rx.push(r);
    }
    let tx = Arc::new(tx);
    let pieces = Arc::new(plan.pieces.clone());

    let mut workers = Vec::with_capacity(width);
    for _ in 0..width {
        let (path, codec) = (plan.path.clone(), plan.codec);
        let (tx, pieces, gate) = (Arc::clone(&tx), Arc::clone(&pieces), Arc::clone(&gate));
        workers.push(thread::spawn(move || {
            while let Some(i) = gate.claim(width, total) {
                let msg = match decode_piece(&path, codec, pieces[i], &tx[i]) {
                    Ok(()) => Msg::End,
                    Err(e) => Msg::Err(e),
                };
                if tx[i].send(msg).is_err() {
                    return; // consumer gone
                }
            }
        }));
    }

    Box::new(ParallelStreams {
        rx,
        cur: 0,
        cursor: io::Cursor::new(Vec::new()),
        gate,
        workers,
        done: false,
    })
}

/// Decode one span and send it on, a chunk at a time. Sending blocks once the piece's slots are
/// full, which is the backpressure: a worker whose piece is far ahead of the consumer stops rather
/// than buffering, whatever that piece expands to. That is also why an entry bomb inside a span
/// costs nothing here — it never materializes.
fn decode_piece(
    path: &Path,
    codec: StreamCodec,
    piece: Piece,
    tx: &SyncSender<Msg>,
) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(piece.start))
        .map_err(|e| e.to_string())?;
    let src: Box<dyn Read + Send> =
        Box::new(BufReader::with_capacity(READ_BUF, file.take(piece.len)));
    let mut dec = decode_stream(codec, src).map_err(|e| e.to_string())?;
    // One buffer for the whole piece, copied out at its filled length. `vec![0u8; CHUNK]` per
    // message would zero a megabyte per megabyte decoded, which is the cost `formats::tar`'s worker
    // was paying before it hoisted its own.
    let mut buf = vec![0u8; CHUNK];
    loop {
        let mut filled = 0;
        while filled < CHUNK {
            match dec.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        if filled == 0 {
            return Ok(());
        }
        if tx.send(Msg::Chunk(buf[..filled].to_vec())).is_err() {
            return Ok(()); // consumer gone
        }
    }
}

impl Read for ParallelStreams {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // A zero-length request also yields 0 from the cursor, which is our "chunk drained" signal.
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.cursor.read(out)?;
            if n > 0 {
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            match self.rx[self.cur].recv() {
                Ok(Msg::Chunk(v)) => self.cursor = io::Cursor::new(v),
                Ok(Msg::End) => {
                    self.cur += 1;
                    if self.cur == self.rx.len() {
                        self.done = true;
                        return Ok(0);
                    }
                    self.gate.advance(self.cur);
                }
                Ok(Msg::Err(e)) => {
                    self.done = true;
                    return Err(io::Error::other(e));
                }
                Err(_) => {
                    self.done = true;
                    return Err(io::Error::other("parallel decode worker ended mid-stream"));
                }
            }
        }
    }
}

impl Drop for ParallelStreams {
    fn drop(&mut self) {
        // Order matters. `stop` releases anyone waiting on the window; dropping the receivers makes
        // every blocked `send` fail, which is how a worker part-way through a piece learns to quit.
        // Without both, abandoning an extraction early would leave the pool decoding to nobody.
        self.gate.stop();
        self.rx.clear();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::write::BzEncoder;
    use std::io::Write;

    fn bz(data: &[u8]) -> Vec<u8> {
        let mut e = BzEncoder::new(Vec::new(), bzip2::Compression::new(1));
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut w =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap()
    }

    /// Concatenate `n` streams of distinguishable content, the way the chunked writer does.
    fn run(n: usize, enc: fn(&[u8]) -> Vec<u8>) -> (Vec<u8>, Vec<u8>) {
        let mut file = Vec::new();
        let mut plain = Vec::new();
        for i in 0..n {
            let body: Vec<u8> = (0..40_000u32).map(|k| (k as u8) ^ (i as u8)).collect();
            file.extend_from_slice(&enc(&body));
            plain.extend_from_slice(&body);
        }
        (file, plain)
    }

    #[test]
    fn finds_every_bzip2_stream_start() {
        let (file, _) = run(6, bz);
        let b = boundaries(io::Cursor::new(&file), Separable::Bzip2).unwrap();
        assert_eq!(b.len(), 6, "expected one boundary per stream, got {b:?}");
        assert_eq!(b[0], 0);
        // Every offset really is a stream: decoding from it must yield whole streams.
        for &off in &b {
            let mut dec = decode_stream(
                StreamCodec::Bzip2,
                Box::new(io::Cursor::new(file[off as usize..].to_vec())),
            )
            .unwrap();
            let mut v = Vec::new();
            dec.read_to_end(&mut v).unwrap();
            assert_eq!(v.len() % 40_000, 0);
        }
    }

    #[test]
    fn finds_every_xz_stream_start() {
        let (file, _) = run(6, xz);
        let b = boundaries(io::Cursor::new(&file), Separable::Xz).unwrap();
        assert_eq!(b.len(), 6, "expected one boundary per stream, got {b:?}");
        assert_eq!(b[0], 0);
    }

    /// A boundary that straddles a scan window must still be found. The window is 4 MiB, so this
    /// drives enough data through to cross several.
    #[test]
    fn boundaries_survive_the_window_seam() {
        let mut file = Vec::new();
        let mut want = Vec::new();
        // Streams of ~1 MiB compressed of incompressible data, so seams land at arbitrary offsets.
        let mut seed = 0x243F_6A88_85A3_08D3u64;
        for _ in 0..14 {
            let body: Vec<u8> = (0..1_100_000)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect();
            want.push(file.len() as u64);
            file.extend_from_slice(&bz(&body));
        }
        assert!(
            file.len() > 3 * SCAN_WIN,
            "test data too small to cross a window: {}",
            file.len()
        );
        let got = boundaries(io::Cursor::new(&file), Separable::Bzip2).unwrap();
        assert_eq!(got, want);
    }

    /// A file that is a single stream is not something to split, and must say so rather than
    /// returning one piece and a pool.
    #[test]
    fn a_lone_stream_is_not_a_plan() {
        let (file, _) = run(1, bz);
        let b = boundaries(io::Cursor::new(&file), Separable::Bzip2).unwrap();
        assert_eq!(b, vec![0]);
        assert_eq!(merge(&b, file.len() as u64, MIN_PIECE).len(), 1);
    }

    /// A file whose second stream is beyond [`PROBE`] must be given up on rather than read whole:
    /// scanning a single-stream archive to its end is a straight loss, since there is nothing to
    /// find and the sequential decoder is about to read every byte again.
    #[test]
    fn a_file_with_no_second_stream_is_abandoned_early() {
        // Only the scan is under test and it decodes nothing, so the body is filler rather than a
        // real stream — compressing 96 MB of noise to prove a `find` returns None is 90 seconds of
        // CI for no extra coverage. A byte that is not `B` cannot begin a bzip2 candidate.
        let mut file = Vec::with_capacity(PROBE as usize + 2 * SCAN_WIN);
        file.extend_from_slice(b"BZh9");
        file.extend_from_slice(&BZ_BLOCK_MAGIC);
        file.resize(PROBE as usize + 2 * SCAN_WIN, 0x5A);
        assert!(file.len() as u64 > PROBE, "fixture must exceed the probe");
        // A reader that refuses to serve past the probe plus one window: if the scan read further,
        // it errors instead of quietly succeeding.
        struct Capped(io::Cursor<Vec<u8>>, u64);
        impl Read for Capped {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.0.position() >= self.1 {
                    return Err(io::Error::other("scan read past the probe"));
                }
                self.0.read(b)
            }
        }
        let cap = PROBE + SCAN_WIN as u64;
        let got = boundaries(Capped(io::Cursor::new(file), cap), Separable::Bzip2).unwrap();
        assert_eq!(got, vec![0], "a lone stream must yield only its own start");
    }

    /// Anything that does not begin with a stream header is not this layout at all.
    #[test]
    fn a_foreign_head_is_refused() {
        let mut file = b"not an archive".to_vec();
        file.extend_from_slice(&bz(b"hello"));
        assert!(boundaries(io::Cursor::new(&file), Separable::Bzip2)
            .unwrap()
            .is_empty());
    }

    /// The end-of-stream check is what keeps a chance `BZh9` + pi in compressed data from being read
    /// as a boundary. Plant exactly that byte pattern inside a stream's body.
    #[test]
    fn a_planted_header_without_a_footer_is_not_a_boundary() {
        let mut body = vec![0u8; 200_000];
        body[100_000..100_003].copy_from_slice(b"BZh");
        body[100_003] = b'9';
        body[100_004..100_010].copy_from_slice(&BZ_BLOCK_MAGIC);
        let (mut file, _) = (bz(&body), ());
        file.extend_from_slice(&bz(b"second stream"));
        let b = boundaries(io::Cursor::new(&file), Separable::Bzip2).unwrap();
        assert_eq!(b.len(), 2, "planted header was taken for a stream start");
    }

    /// Merging must cover the file exactly once, in order, with no gap and no overlap.
    #[test]
    fn merged_pieces_tile_the_file() {
        let bounds: Vec<u64> = (0..40).map(|i| i * 300_000).collect();
        let len = 40 * 300_000;
        let p = merge(&bounds, len, MIN_PIECE);
        assert!(p.len() > 1);
        assert_eq!(p[0].start, 0);
        for w in p.windows(2) {
            assert_eq!(w[0].start + w[0].len, w[1].start);
            assert!(w[0].len >= MIN_PIECE);
        }
        let last = p.last().unwrap();
        assert_eq!(last.start + last.len, len);
    }

    /// The buffer must cover a piece, and the pool must stay inside its budget. This is the pair of
    /// numbers that turned xz from 1.16× into a real win, so it gets a regression rather than a
    /// comment: a 32 MiB xz piece compressed to ~7.5 MB must get enough slots to hold itself.
    #[test]
    fn a_worker_can_buffer_a_whole_piece_within_the_budget() {
        let of = |len: u64, n: usize| vec![Piece { start: 0, len }; n];
        for (compressed, decoded) in [
            (7_500_000u64, 32u64 << 20), // xz: the writer's 32 MiB chunk
            (1_040_000, 4 << 20),        // bzip2: the writer's 4 MiB chunk
        ] {
            let (width, slots) = shape(&of(compressed, 200));
            let held = (slots as u64) * CHUNK as u64;
            assert!(
                held >= decoded,
                "a worker can hold {held} bytes of a {decoded}-byte piece; \
                 the tail of every piece would decode one-worker-wide"
            );
            assert!(
                (width as u64) * held <= BUDGET,
                "width {width} x {held} bytes exceeds the budget"
            );
            assert!(width >= 1);
        }
    }

    /// The whole point, end to end: the parallel reader must yield exactly what the sequential one
    /// does, byte for byte, for both codecs.
    #[test]
    fn parallel_output_equals_sequential_output() {
        for (codec, enc) in [
            (StreamCodec::Bzip2, bz as fn(&[u8]) -> Vec<u8>),
            (StreamCodec::Xz, xz as fn(&[u8]) -> Vec<u8>),
        ] {
            let (file, plain) = run(40, enc);
            let dir =
                std::env::temp_dir().join(format!("cram-multi-{:?}-{}", codec, std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("a.bin");
            std::fs::write(&path, &file).unwrap();

            let bounds = boundaries(io::Cursor::new(&file), Separable::of(codec).unwrap()).unwrap();
            assert_eq!(bounds.len(), 40);

            // A minimum of one byte keeps all 40 pieces, so the pool, the window and the reorder
            // are all genuinely exercised on a fixture measured in kilobytes.
            let p = plan_with(&path, codec, 1).expect("40 streams should plan");
            assert!(p.pieces.len() > 1);
            let mut got = Vec::new();
            open(&p).read_to_end(&mut got).unwrap();
            assert_eq!(
                got, plain,
                "{codec:?} parallel decode differs from the input"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// Dropping the reader part-way must not leave the pool running. If `Drop` did not both stop the
    /// gate and drop the receivers, this would hang.
    #[test]
    fn abandoning_the_reader_stops_the_pool() {
        let (file, _) = run(40, bz);
        let dir = std::env::temp_dir().join(format!("cram-multi-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.bin");
        std::fs::write(&path, &file).unwrap();
        let p = plan_with(&path, StreamCodec::Bzip2, 1).unwrap();
        let mut r = open(&p);
        let mut head = [0u8; 64];
        r.read_exact(&mut head).unwrap();
        drop(r); // joins every worker; a leak shows up as a hang
        std::fs::remove_dir_all(&dir).ok();
    }
}
