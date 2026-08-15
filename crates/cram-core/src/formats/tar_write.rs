//! tar writer backend, creates `.tar` and, by wrapping the output sink in a whole-stream encoder,
//! the full tar-family: `.tar.gz` `.tar.xz` `.tar.bz2` `.tar.lz4` `.tar.br` `.tar.zst`. Built on the
//! `tar` crate's `Builder` (`append_data` per entry, trailer on `into_inner`); the codec wrapper is
//! chosen at construction and finalized explicitly in `finish` (each encoder needs its trailer
//! flushed that dropping alone wouldn't guarantee cleanly).
//!
//! Two encoders are not a plain `Write` wrapper:
//!
//! **zstd** depends on the `zstd-c` feature, and the difference is large enough to measure. With it,
//! the C library's streaming encoder is used: a real `Write` sink at any level, writing what the
//! `zstd` CLI writes. Without it, `ruzstd` has no `Write` sink and encodes only at its `Fastest`
//! level, so the archive is built as a run of independent 8 MiB *frames* instead — spec-legal
//! (`cat`/pzstd produce exactly that, and both our reader and `zstd -d` decode them all) and bounded
//! in memory, but slower and considerably larger.
//!
//! That gap was measured on 2026-08-14 and it is why the feature now reaches here: writing the
//! kernel tree took **18.78 s for 742,491,196 bytes** through ruzstd against `zstd -T0 -3`'s 1.73 s
//! for 540,088,970 — 10.9× slower and 37% larger — in a shipping build that already linked the C
//! library for `.cram` packs and simply never used it here.
//!
//! **gzip**: written pigz-style so create uses every core, see [`TarSink::Gz`]. Only *create* is
//! parallel — a standard `.gz` cannot be extracted in parallel by anyone, ours included, because a
//! decoder cannot find the block boundaries without first inflating everything before them.
//!
//! tar cannot encrypt in-format (there's no per-entry or whole-archive password slot); an encrypted
//! tar means wrapping it in a `.cram`/`.zip`, so a create request carrying an `EncryptSpec` here
//! returns [`ArchiveError::UnsupportedEncryption`] rather than silently producing a plaintext archive.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Instant, UNIX_EPOCH};

use brotli::enc::BrotliEncoderParams;
use brotli::CompressorWriter;
use bzip2::write::BzEncoder;
use flate2::{Compress, Compression, Crc, FlushCompress};
use lz4_flex::frame::{BlockSize, FrameEncoder, FrameInfo};
use lzma_rust2::{XzOptions, XzWriter};
#[cfg(not(feature = "zstd-c"))]
use ruzstd::encoding::{compress_to_vec, CompressionLevel};
use tar::{Builder, EntryType, Header};

use crate::error::{ArchiveError, Result};
use crate::format::{Codec, Format};
use crate::model::Entry;
use crate::writer::{ArchiveWriter, CreateOptions, CreateReport, Level, WriteHint};

/// Streams **exactly** `remaining` bytes from `inner`: truncates if the source now yields more than
/// the recorded size (it grew after being sized) and zero-pads if it yields fewer (it shrank/was
/// truncated). tar writes a fixed `size` into each entry header and then pads the body to whatever
/// the reader produced, if those disagree, the archive tail desyncs. A source file mutated between
/// the create pre-pass and this streaming write is the realistic trigger; forcing the body length to
/// equal the header `size` keeps the archive structurally valid regardless (GNU tar's behavior).
///
/// Padding keeps the *container* valid but the entry's CONTENT is no longer what the user asked to
/// archive, `padded` records that so the caller can surface it as an error instead of reporting a
/// successful create over silently-corrupted data (GNU tar likewise exits non-zero with "file
/// changed as we read it").
struct ExactReader<'a> {
    inner: &'a mut dyn io::Read,
    remaining: u64,
    /// Set when the source ended early and the tail was zero-padded (it shrank mid-archive).
    padded: bool,
}

impl io::Read for ExactReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = self.remaining.min(buf.len() as u64) as usize;
        let dst = &mut buf[..cap];
        let n = self.inner.read(dst)?;
        if n == 0 {
            dst.fill(0); // source ended early → zero-pad the rest of the declared size
            self.remaining -= cap as u64;
            self.padded = true;
            Ok(cap)
        } else {
            self.remaining -= n as u64;
            Ok(n)
        }
    }
}

/// The output sink: a plain file, or the file behind a whole-stream encoder. Implements `Write` (the
/// tar `Builder` writes through it) and finalizes the codec + returns the underlying `File` in
/// `finish`. Encoder variants are boxed (their structs dwarf a plain file, clippy `large_enum_variant`).
enum TarSink {
    Plain(BufWriter<File>),
    /// gzip, written the way pigz writes it. The tar stream is cut into [`GZ_CHUNK`] pieces, each
    /// deflated by its own compressor and ended with a sync flush so it stops on a byte boundary;
    /// the pieces are then concatenated, because the concatenation *is* the DEFLATE stream.
    ///
    /// Nothing about the result is unusual to a reader: one gzip header, one DEFLATE stream, one
    /// trailer, so `gzip -d`, `zcat` and `tar -xzf` see an ordinary `.gz`. What makes the pieces
    /// separable is that each compressor starts empty, so no back-reference can point outside its
    /// own piece. That also costs a little ratio (each seam throws away up to 32 KiB of dictionary),
    /// which is why the chunk is 1 MiB rather than pigz's 128 KiB.
    ///
    /// The header is written before the sink is built and the trailer in `finish`; everything
    /// between is [`ChunkedSink`], the same pool the other chunked codecs use. It had its own copy
    /// of that machinery — a window, a parallel map, a serial write — until the pipeline replaced
    /// the window, at which point keeping a second implementation of it only meant gzip would not
    /// get the fix.
    Gz(ChunkedSink),
    /// xz and bzip2 — the codecs whose *whole streams* concatenate and whose reader follows them.
    /// See [`ChunkedSink`] and [`ChunkCodec`].
    Chunked(ChunkedSink),
    Lz4(Box<FrameEncoder<BufWriter<File>>>),
    /// brotli is the one codec that cannot be chunked: it has no multi-stream concept, so two
    /// brotli streams laid end to end are not a brotli stream. It stays a plain streaming writer.
    Br(Box<CompressorWriter<BufWriter<File>>>),
    /// zstd, whose implementation depends on the `zstd-c` feature. See [`ZstdSink`].
    Zstd(ZstdSink),
}

/// A codec whose complete streams concatenate into a valid stream of the same kind. `cat a.xz b.xz`
/// is a `.xz` that `xz -d` reads whole, and the same holds for bzip2 and lz4 — our own reader
/// already relies on it (`XzReader` with `allow_multiple_streams`, `MultiBzDecoder`, lz4's
/// `FrameDecoder`), and so do pbzip2, lbzip2 and `xz -T0`.
///
/// That makes parallel create nearly free: compress N chunks independently and write them in order.
/// It is the same trade R4 took for gzip — each chunk starts with an empty dictionary, so the
/// archive grows slightly — and it is what `xz -T0` itself does, which is why its output is *larger*
/// than a single-threaded `xz` too.
///
/// **lz4 is deliberately not here, and the reason changed on 2026-08-15.** It used to be that our
/// *reader* stopped at the first frame; that was fixed (`codec::frames::MultiFrameLz4`) and the
/// exclusion outlived it, so chunked lz4 create was built and measured. It is a **loss**, and the
/// numbers are worth keeping so nobody builds it a third time. Kernel tree, tmpfs, one binary
/// reading both archives back to back:
///
/// | | one frame | 239 frames |
/// |---|---|---|
/// | `cram a` | 3.72 s, 100% CPU | **2.06 s**, 252% |
/// | `cram x` | **3.73 s** | 4.34 s |
/// | `lz4 -dc \| tar` | **2.09 s** | 2.59 s |
///
/// Chunking buys 1.7 s of create and costs 0.6 s of our extraction and **0.5 s of everyone else's**
/// — a penalty on every third-party tool that reads what we write, for a codec chosen because it is
/// fast to read.
///
/// It also *looked* like it gained 3% of ratio, which is impossible for chunking and was the clue
/// worth following: the gain belonged to the block size, arriving by accident because a worker's
/// first write is a whole 8 MiB chunk rather than a 512-byte tar header. That is [`LZ4_BLOCK`], and
/// measured on its own it is the wrong trade too.
#[derive(Clone, Copy)]
enum ChunkCodec {
    /// Not a whole stream like the others: a run of raw DEFLATE blocks ended with a sync flush, to
    /// sit inside the single gzip member whose header and trailer [`TarSink`] writes around it. It
    /// belongs here anyway — the pieces are independent and concatenate in order, which is the only
    /// property the pipeline needs — and putting it here is what gets gzip the same pool as the rest
    /// instead of a second copy of the same machinery.
    Gzip,
    Xz,
    Bzip2,
    /// Only reachable without `zstd-c`; the C encoder streams and needs no help from here.
    #[cfg(not(feature = "zstd-c"))]
    Zstd,
}

impl ChunkCodec {
    /// Bytes per chunk. Chosen per codec against its window: too small and every seam costs ratio,
    /// too large and the pool runs dry on a small archive. xz's dictionary is 8 MiB at preset 6, so
    /// 32 MiB keeps the loss to a quarter of the seam it would otherwise be; bzip2's block is
    /// 900 KB and lz4's window 64 KB, so neither needs the room. gzip's is [`GZ_CHUNK`].
    ///
    /// Whatever the value, it also bounds memory. That matters historically: the first `.tar.zst`
    /// writer held the ENTIRE tar in RAM until `finish`, and archiving a 100 GiB tree OOM'd the
    /// process.
    fn chunk(self) -> usize {
        match self {
            ChunkCodec::Gzip => GZ_CHUNK,
            ChunkCodec::Xz => 32 << 20,
            ChunkCodec::Bzip2 => 4 << 20,
            #[cfg(not(feature = "zstd-c"))]
            ChunkCodec::Zstd => 8 << 20,
        }
    }

    /// One chunk as a complete, standalone stream, plus whatever the container's trailer needs from
    /// it. Only gzip needs anything: a CRC32 and a length, computed on the worker so the trailer
    /// costs no second pass over the same bytes.
    fn compress(self, data: &[u8], level: Level) -> io::Result<(Vec<u8>, Option<Crc>)> {
        match self {
            ChunkCodec::Gzip => {
                let (bytes, crc) = deflate_chunk(data, preset(level))?;
                Ok((bytes, Some(crc)))
            }
            ChunkCodec::Xz => {
                let mut w = XzWriter::new(Vec::new(), XzOptions::with_preset(preset(level)))?;
                w.write_all(data)?;
                Ok((w.finish()?, None))
            }
            ChunkCodec::Bzip2 => {
                let mut w = BzEncoder::new(Vec::new(), bzip2::Compression::new(bz_level(level)));
                w.write_all(data)?;
                Ok((w.finish()?, None))
            }
            #[cfg(not(feature = "zstd-c"))]
            ChunkCodec::Zstd => Ok((compress_to_vec(data, CompressionLevel::Fastest), None)),
        }
    }
}

/// Total chunk bytes allowed in flight, used **only when the machine's free RAM cannot be read**.
///
/// This was the sole bound until 2026-08-14 and it bounded the wrong quantity. It counts chunk bytes,
/// and chunk bytes are the small part: an xz worker's encoder outweighs its 32 MiB chunk three to
/// one, so a "512 MiB" budget was really 2.7 GB of resident memory at the 16 workers it permitted.
/// It was also fixed, so it gave 16 on a 24-thread box with 23 GiB free — a third of the throughput
/// for no memory saved that anyone asked for. See [`chunk_width`].
const CHUNK_BUDGET: usize = 512 << 20;

/// Chunks allowed in flight at once — queued, compressing, or compressed and waiting their turn to
/// be written. A permit is taken when a chunk is cut and given back by the **writer**, so the bound
/// covers the reorder buffer as well as the pool: a slow chunk cannot let its faster successors pile
/// up in memory without limit while they wait for it.
struct Permits {
    left: Mutex<usize>,
    cv: Condvar,
}

impl Permits {
    fn new(n: usize) -> Self {
        Self {
            left: Mutex::new(n),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) {
        // A poisoned lock means a worker panicked. That is reported through the error slot; blocking
        // the producer for ever on top of it would only turn a failure into a hang.
        let mut left = self.left.lock().unwrap_or_else(|e| e.into_inner());
        while *left == 0 {
            left = self.cv.wait(left).unwrap_or_else(|e| e.into_inner());
        }
        *left -= 1;
    }

    fn release(&self) {
        let mut left = self.left.lock().unwrap_or_else(|e| e.into_inner());
        *left += 1;
        self.cv.notify_one();
    }
}

/// First error from any worker or from the writer. Kept beside the channels rather than returned
/// through them so `write` can fail fast instead of only finding out at `finish`.
type ErrSlot = Arc<Mutex<Option<io::Error>>>;

fn set_err(slot: &ErrSlot, e: io::Error) {
    let mut g = slot.lock().unwrap_or_else(|x| x.into_inner());
    if g.is_none() {
        *g = Some(e);
    }
}

fn take_err(slot: &ErrSlot) -> Option<io::Error> {
    slot.lock().unwrap_or_else(|x| x.into_inner()).take()
}

/// A compressed chunk on its way to the writer: the bytes, and the CRC gzip's trailer needs.
type ChunkOut = (Vec<u8>, Option<Crc>);

/// What the writer thread hands back once every chunk is on disk: the file, and the CRC gzip's
/// trailer needs. Empty for every other codec.
type WrittenOut = io::Result<(BufWriter<File>, Crc)>;

/// Workers compressing chunks, and one thread writing them out in index order.
struct Pipeline {
    tx: Option<SyncSender<(usize, Vec<u8>)>>,
    workers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<WrittenOut>>,
    permits: Arc<Permits>,
    err: ErrSlot,
}

impl Pipeline {
    fn start(file: BufWriter<File>, codec: ChunkCodec, level: Level, width: usize) -> Self {
        let (tx, rx) = sync_channel::<(usize, Vec<u8>)>(width);
        let (done_tx, done_rx) = sync_channel::<(usize, io::Result<ChunkOut>)>(width);
        let rx = Arc::new(Mutex::new(rx));
        let permits = Arc::new(Permits::new(width));
        let err: ErrSlot = Arc::new(Mutex::new(None));

        let workers = (0..width)
            .map(|_| {
                let rx = Arc::clone(&rx);
                let done_tx = done_tx.clone();
                std::thread::spawn(move || {
                    loop {
                        // Held only across `recv`, never across the compression itself.
                        let next = {
                            let g = rx.lock().unwrap_or_else(|e| e.into_inner());
                            g.recv()
                        };
                        let Ok((idx, data)) = next else { return };
                        // A panicking codec would otherwise strand this chunk's permit and hang the
                        // producer, turning a crash into a deadlock.
                        let out = std::panic::catch_unwind(AssertUnwindSafe(|| {
                            codec.compress(&data, level)
                        }))
                        .unwrap_or_else(|_| Err(io::Error::other("chunk compressor panicked")));
                        if done_tx.send((idx, out)).is_err() {
                            return;
                        }
                    }
                })
            })
            .collect();
        drop(done_tx);

        let writer = {
            let permits = Arc::clone(&permits);
            let err = Arc::clone(&err);
            std::thread::spawn(move || writer_loop(file, done_rx, permits, err))
        };

        Self {
            tx: Some(tx),
            workers,
            writer: Some(writer),
            permits,
            err,
        }
    }

    /// Hand a cut chunk to the pool, blocking only when the in-flight bound is reached.
    fn submit(&self, idx: usize, data: Vec<u8>) -> io::Result<()> {
        if let Some(e) = take_err(&self.err) {
            return Err(e);
        }
        self.permits.acquire();
        if let Some(tx) = &self.tx {
            // The workers only vanish after a failure, which the error slot already carries.
            let _ = tx.send((idx, data));
        }
        Ok(())
    }

    fn finish(mut self) -> io::Result<(BufWriter<File>, Crc)> {
        // Closing the input ends the workers, which drops their senders, which ends the writer.
        self.tx = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        let out = match self.writer.take().map(|w| w.join()) {
            Some(Ok(r)) => r,
            _ => Err(io::Error::other("chunk writer thread died")),
        };
        match take_err(&self.err) {
            Some(e) => Err(e),
            None => out,
        }
    }
}

/// Receive compressed chunks in whatever order they finish and write them in index order, folding
/// each one's CRC into the running trailer as it goes.
///
/// The CRC has to be combined here rather than by the workers, and in this order: `Crc::combine` is
/// associative but not commutative, so folding chunks as they *finished* would give a checksum that
/// depended on scheduling and disagreed with the bytes actually written.
fn writer_loop(
    mut file: BufWriter<File>,
    rx: Receiver<(usize, io::Result<ChunkOut>)>,
    permits: Arc<Permits>,
    err: ErrSlot,
) -> WrittenOut {
    let mut pending: BTreeMap<usize, ChunkOut> = BTreeMap::new();
    let mut next = 0usize;
    let mut failed = false;
    let mut crc = Crc::new();

    for (idx, res) in rx {
        if failed {
            permits.release();
            continue;
        }
        match res {
            Err(e) => {
                set_err(&err, e);
                failed = true;
                // Everything already queued behind the failure is never going to be written, so give
                // its permits back now rather than leaving the producer blocked until the channel
                // happens to close.
                for _ in 0..pending.len() + 1 {
                    permits.release();
                }
                pending.clear();
            }
            Ok(out) => {
                pending.insert(idx, out);
                while let Some((data, chunk_crc)) = pending.remove(&next) {
                    if let Err(e) = file.write_all(&data) {
                        set_err(&err, e);
                        failed = true;
                    }
                    if let Some(c) = chunk_crc {
                        crc.combine(&c);
                    }
                    next += 1;
                    permits.release();
                }
            }
        }
    }
    Ok((file, crc))
}

/// Peak RSS per worker, MiB — **measured on the kernel tree**, not modelled, by running the same
/// create at several widths and taking the slope.
///
/// The chunk itself is the small half of it. An xz worker at preset 6 holds a 32 MiB chunk and about
/// 100 MiB of encoder: LZMA's BT4 match finder runs to roughly 11.5× the dictionary, and the
/// dictionary is 8 MiB. That is why bounding *chunk bytes* bounded almost nothing.
///
/// | codec | 8 workers | 16 | 24 | slope |
/// |---|---|---|---|---|
/// | xz | 1646 MB | 2695 | 3912 | ~136 MiB |
/// | bzip2 | 322 MB | 535 | 810 | ~34 MiB |
///
/// gzip is not in that table because it never binds: a 1 MiB chunk and a deflate state under a
/// megabyte means the core count decides long before the RAM does, on anything that can run cram.
///
/// `hw::codec_mem_per_thread_mib` is not reusable here: its 2400 MiB for LZMA is the **level 9**
/// figure, and applying it at preset 6 would cap this machine at five workers — slower than the
/// fixed bound it replaced.
fn chunk_mem_per_worker_mib(codec: ChunkCodec) -> u64 {
    match codec {
        ChunkCodec::Gzip => 2,
        ChunkCodec::Xz => 136,
        ChunkCodec::Bzip2 => 34,
        // Not measured: the pure-Rust fallback encoder is far simpler than either of the above, so
        // this is the 8 MiB chunk plus room, and it is deliberately on the generous side.
        #[cfg(not(feature = "zstd-c"))]
        ChunkCodec::Zstd => 24,
    }
}

/// How many chunks may be in flight — queued, compressing, or awaiting their turn to be written.
///
/// This is the width of the pool, and it is what actually decides create speed: on the kernel tree
/// xz runs at 7.1 effective cores at width 8, 12.8 at 16 and 18.9 at 24, and saturates there (width
/// 32 buys 0.3 s for another gigabyte). The old fixed `CHUNK_BUDGET / chunk` gave 16 whatever the
/// machine had, which is both too many for a small VM and a third of the throughput on a 24-thread
/// box with 23 GiB spare.
///
/// So take it from the RAM that is actually free, the same 60% fraction `hw::derive_plan` uses.
/// `CRAM_WORKERS` forces it, as it does elsewhere in the engine — including past this bound, since an
/// explicit override is the caller saying they know what their machine has.
///
/// **On any machine with headroom the RAM term is inert, and the core count is the whole answer.**
/// Worth stating because it is easy to read the fraction as the thing setting the width: on the dev
/// box, 60% of 21 GiB free against 136 MiB per xz worker permits **95**, so `threads` — 24 — is what
/// binds. The fraction earns its place only on a small machine, which is what it was added for.
///
/// Measured 2026-08-15, kernel tree (2.1 GB) to `/dev/shm`, `.tar.xz`, median of two, peak RSS from
/// `/usr/bin/time`:
///
/// | width | wall | peak RSS |
/// |---|---|---|
/// | 4 | 127.7 s | 1035 MB |
/// | 8 | 72.7 s | 1739 MB |
/// | 12 | 52.7 s | 2312 MB |
/// | 16 | 49.0 s | 2797 MB |
/// | 20 | 43.3 s | 3397 MB |
/// | 24 | 40.8 s | 3940 MB |
/// | `tar \| xz -T0` | 34.3 s | 3195 MB |
///
/// **The excess over `xz -T0` is the in-flight chunk budget, not the encoders.** Per worker the two
/// are comparable; what cram holds on top is `width × chunk` = 24 × 32 MiB = 768 MB, against a
/// measured gap of 745 MB. That budget is already at its floor — permits equal the width, and fewer
/// would leave workers idle — so **there is no width at 24 that fits under `xz -T0`'s memory**. Going
/// under it means dropping to about 16 workers and giving back 8 s of the 12 that A2 won. That is a
/// real trade rather than a defect, and it is recorded here so it is not re-derived.
fn chunk_width(codec: ChunkCodec) -> usize {
    if let Some(n) = std::env::var("CRAM_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    let threads = rayon::current_num_threads().max(1);
    let avail = crate::hw::HwProfile::detect().ram_avail;
    if avail == 0 {
        // No usable reading: fall back to the old fixed budget rather than guessing from core count
        // alone, which is how a 64-thread machine would try to hold 8 GiB of xz encoders.
        return threads.min(CHUNK_BUDGET / codec.chunk()).max(1);
    }
    let per = chunk_mem_per_worker_mib(codec) << 20;
    let cap = (((avail as f64) * 0.6) as u64 / per.max(1)) as usize;
    threads.min(cap).max(1)
}

/// Cut the tar into independent chunks, compress them on every core, write them in order.
///
/// This used to be a window: fill N chunks, `par_iter` the lot, write them, repeat. That is a barrier
/// twice over, and sampling CPU width every 100 ms during an xz create showed both halves of the cost
/// as a sawtooth whose period matched the window size — roughly 9 s at 18–20 cores, then a 3–4 s
/// decay while the window's slowest chunk finished **alone**, then ~1.5 s under two cores writing the
/// output and accumulating the next 512 MiB with the pool completely idle.
///
/// A pipeline removes both. Workers take the next chunk the moment they finish one, so a straggler
/// delays only the writer's cursor and not the pool, and cutting continues while compression runs.
struct ChunkedSink {
    buf: Vec<u8>,
    chunk: usize,
    /// Index of the next chunk to be cut. The writer reassembles on this.
    next: usize,
    width: usize,
    codec: ChunkCodec,
    level: Level,
    /// Holds the file until the first chunk is cut, then the pipeline owns it. An archive smaller
    /// than one chunk — which is most of them — never starts a thread.
    file: Option<BufWriter<File>>,
    pipe: Option<Pipeline>,
}

impl ChunkedSink {
    fn new(file: BufWriter<File>, codec: ChunkCodec, level: Level) -> Self {
        let chunk = codec.chunk();
        Self {
            buf: Vec::new(),
            chunk,
            next: 0,
            width: chunk_width(codec),
            codec,
            level,
            file: Some(file),
            pipe: None,
        }
    }

    fn cut(&mut self, data: Vec<u8>) -> io::Result<()> {
        if self.pipe.is_none() {
            let file = self
                .file
                .take()
                .ok_or_else(|| io::Error::other("chunked sink already finished"))?;
            self.pipe = Some(Pipeline::start(file, self.codec, self.level, self.width));
        }
        let idx = self.next;
        self.next += 1;
        self.pipe.as_ref().expect("just started").submit(idx, data)
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        while self.buf.len() >= self.chunk {
            let tail = self.buf.split_off(self.chunk);
            let full = std::mem::replace(&mut self.buf, tail);
            self.cut(full)?;
        }
        Ok(buf.len())
    }

    /// Deliberately does not cut a chunk: a flush mid-archive would fragment the stream for nothing.
    /// With the write now behind a thread there is also nothing here to push — the file is only
    /// reachable from the writer until `finish` gives it back.
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }

    /// Returns the file and the accumulated CRC. Only gzip's trailer wants the latter; for every
    /// other codec it is an empty `Crc` the caller drops.
    fn finish(mut self) -> io::Result<(BufWriter<File>, Crc)> {
        let tail = std::mem::take(&mut self.buf);
        match self.pipe.take() {
            // The archive outgrew one chunk, so the pool is already running.
            Some(pipe) => {
                if !tail.is_empty() {
                    let idx = self.next;
                    self.next += 1;
                    pipe.submit(idx, tail)?;
                }
                pipe.finish()
            }
            // It never did: compress the whole thing here rather than start threads to do it.
            None => {
                let mut file = self
                    .file
                    .take()
                    .ok_or_else(|| io::Error::other("chunked sink already finished"))?;
                let mut crc = Crc::new();
                if !tail.is_empty() {
                    let (bytes, chunk_crc) = self.codec.compress(&tail, self.level)?;
                    file.write_all(&bytes)?;
                    if let Some(c) = chunk_crc {
                        crc.combine(&c);
                    }
                }
                Ok((file, crc))
            }
        }
    }
}

/// The zstd sink. One type, two implementations, so the feature split lives here rather than in
/// every arm of `write`, `flush` and `finish`.
#[cfg(feature = "zstd-c")]
struct ZstdSink(Box<zstd::stream::write::Encoder<'static, BufWriter<File>>>);

#[cfg(feature = "zstd-c")]
impl ZstdSink {
    fn new(file: BufWriter<File>, level: Level) -> io::Result<Self> {
        let mut enc = zstd::stream::write::Encoder::new(file, zstd_level(level))?;
        // libzstd's own workers, which is the whole reason this arm exists. They share one context
        // and one window, so unlike [`ChunkedSink`] the output is the stream a single-threaded run
        // would have produced — no seams, no ratio given away — and `zstd -T0` works the same way.
        enc.multithread(zstd_workers() as u32)?;
        Ok(Self(Box::new(enc)))
    }
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
    fn finish(self) -> io::Result<BufWriter<File>> {
        self.0.finish()
    }
}

/// Pure-Rust fallback: `ruzstd` has no `Write` sink, so the tar is cut into bounded independent
/// *frames*, which is exactly the shape [`ChunkedSink`] exists for — so it gets parallel create too,
/// rather than compressing those frames one after another as it used to.
#[cfg(not(feature = "zstd-c"))]
struct ZstdSink(ChunkedSink);

#[cfg(not(feature = "zstd-c"))]
impl ZstdSink {
    fn new(file: BufWriter<File>, level: Level) -> io::Result<Self> {
        Ok(Self(ChunkedSink::new(file, ChunkCodec::Zstd, level)))
    }
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
    fn finish(self) -> io::Result<BufWriter<File>> {
        // The CRC rides along for gzip's benefit; zstd frames carry their own checksums.
        Ok(self.0.finish()?.0)
    }
}

/// Workers for libzstd's own thread pool.
///
/// Not routed through [`chunk_width`]: that bounds a pool of *independent* encoders each holding its
/// own chunk and dictionary, and libzstd's workers share one context, so the memory shape is a
/// different thing entirely — `zstd -T0` peaks at 275 MB on the kernel tree where our 16 xz encoders
/// take 2.7 GB. `CRAM_WORKERS` still forces it, as it does everywhere else.
#[cfg(feature = "zstd-c")]
fn zstd_workers() -> usize {
    std::env::var("CRAM_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| rayon::current_num_threads().max(1))
}

/// Map the abstract [`Level`] onto zstd's 1–22 scale. `--auto` is zstd's own default, so a
/// head-to-head against `zstd` at *its* default compares like with like, the same way `--auto` means
/// gzip 6 and xz 6 above.
#[cfg(feature = "zstd-c")]
fn zstd_level(level: Level) -> i32 {
    match level {
        Level::Auto | Level::Balanced => 3,
        Level::Fastest => 1,
        Level::Best | Level::Cold | Level::Tiny => 19,
        Level::Explicit(n) => (n as i32 * 2).clamp(1, 19),
    }
}

/// Bytes of tar per independently-deflated gzip chunk (see [`TarSink::Gz`]). pigz uses 128 KiB;
/// 1 MiB throws away eight times less dictionary at the seams for the same parallelism, and is still
/// only ~20 ms of level-6 deflate, fine enough granularity for a work queue.
const GZ_CHUNK: usize = 1024 * 1024;

/// The 10-byte gzip header, byte-identical to the one flate2's own `GzEncoder` writes at this level.
/// mtime stays zero deliberately: an identical input tree must produce an identical archive, and the
/// tar members already carry their own timestamps.
fn gzip_header(level: u32) -> [u8; 10] {
    let xfl = if level >= 9 {
        2 // best compression
    } else if level <= 1 {
        4 // fastest
    } else {
        0
    };
    [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 255]
}

/// One chunk as a standalone run of raw DEFLATE blocks, ended with a sync flush so it finishes on a
/// byte boundary and the next chunk can simply follow it. Also returns the chunk's CRC32, computed
/// here so it rides along on the worker instead of costing a serial pass over the same bytes.
///
/// The fresh compressor is the point rather than an accident: with no preset dictionary, every
/// back-reference it emits points inside `data`, which is what lets these runs be produced out of
/// order and concatenated.
fn deflate_chunk(data: &[u8], level: u32) -> io::Result<(Vec<u8>, Crc)> {
    let mut z = Compress::new(Compression::new(level), false); // false → raw DEFLATE, no zlib wrapper
    let mut out = Vec::with_capacity(data.len() / 2 + 512);
    let mut fed = 0usize;
    // Feed the whole chunk in first. `compress_vec` fills the Vec's spare capacity and never grows
    // the Vec itself, so the room has to be made before every call — which is also what guarantees
    // each call makes progress.
    while fed < data.len() {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(4096));
        }
        let was_in = z.total_in();
        z.compress_vec(&data[fed..], &mut out, FlushCompress::None)
            .map_err(io::Error::other)?;
        fed += (z.total_in() - was_in) as usize;
    }
    // Then flush, which ends the run on a byte boundary so the next chunk can follow it. A flush is
    // complete once a call leaves output room unused — **not** once it stops producing, because a
    // sync flush asked for again always emits another empty stored block, so that test never
    // terminates and the encoder writes markers until it runs out of memory.
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(4096));
        }
        let room = (out.capacity() - out.len()) as u64;
        let was_out = z.total_out();
        z.compress_vec(&[], &mut out, FlushCompress::Sync)
            .map_err(io::Error::other)?;
        if z.total_out() - was_out < room {
            break;
        }
    }
    let mut crc = Crc::new();
    crc.update(data);
    Ok((out, crc))
}

impl TarSink {
    /// Flush/finalize the codec trailer and hand back the file (for the final-size measurement).
    fn finish(self) -> io::Result<File> {
        let buf = match self {
            TarSink::Plain(w) => w,
            TarSink::Gz(c) => {
                let (mut file, crc) = c.finish()?;
                // A final empty fixed-Huffman block: BFINAL=1, BTYPE=01, then the 7-bit
                // end-of-block code, LSB-first and zero-padded to a byte. Every chunk above ended
                // on a byte boundary, so these two bytes land cleanly and close the stream.
                file.write_all(&[0x03, 0x00])?;
                file.write_all(&crc.sum().to_le_bytes())?;
                // ISIZE is the uncompressed length mod 2^32, which is exactly what `amount` holds.
                file.write_all(&crc.amount().to_le_bytes())?;
                file
            }
            TarSink::Chunked(c) => c.finish()?.0,
            TarSink::Lz4(e) => e.finish().map_err(io::Error::other)?,
            TarSink::Br(e) => e.into_inner(),
            TarSink::Zstd(z) => z.finish()?,
        };
        buf.into_inner().map_err(|e| e.into_error())
    }
}

impl Write for TarSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TarSink::Plain(w) => w.write(buf),
            TarSink::Gz(c) => c.write(buf),
            TarSink::Chunked(c) => c.write(buf),
            TarSink::Lz4(w) => w.write(buf),
            TarSink::Br(w) => w.write(buf),
            TarSink::Zstd(z) => z.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            TarSink::Plain(w) => w.flush(),
            // Deliberately does NOT cut a chunk: a flush mid-archive would fragment the stream for
            // no gain. Only the file behind it is flushed.
            TarSink::Gz(c) => c.flush(),
            TarSink::Chunked(c) => c.flush(),
            TarSink::Lz4(w) => w.flush(),
            TarSink::Br(w) => w.flush(),
            TarSink::Zstd(z) => z.flush(),
        }
    }
}

/// Map the abstract [`Level`] onto the 0–9 preset gzip/xz share (`None`-equivalent default is 6).
fn preset(level: Level) -> u32 {
    match level {
        Level::Auto | Level::Balanced => 6,
        Level::Fastest => 1,
        // No slower DEFLATE/xz encoder to reach for here: zopfli is wired into the zip writer, and a
        // `.tar.gz` is chunked, which zopfli's whole-stream search does not fit.
        Level::Best | Level::Cold | Level::Tiny => 9,
        Level::Explicit(n) => n.clamp(0, 9),
    }
}

/// bzip2's valid block-size level is 1–9.
fn bz_level(level: Level) -> u32 {
    preset(level).clamp(1, 9)
}

/// brotli's quality scale is 0–11.
fn br_quality(level: Level) -> u32 {
    match level {
        Level::Fastest => 2,
        Level::Auto | Level::Balanced => 6,
        Level::Best | Level::Cold | Level::Tiny => 11,
        Level::Explicit(n) => n.clamp(0, 11),
    }
}

/// Output staging buffer for the brotli encoder. Purely an I/O granularity knob — the encoder holds
/// its own input ring buffer sized by `lgwin` — so it costs nothing but syscalls, and 4 KiB against a
/// 4 MiB window was a lot of syscalls.
/// The lz4 frame's block size. **Set to the value we already had, so the output does not change** —
/// the point is that we had it by accident.
///
/// `lz4_flex`'s default is `BlockSize::Auto`, documented as detecting the size **from the first
/// write call**. A tar's first write is a 512-byte header, so every `.tar.lz4` cram has ever written
/// used 64 KB blocks without anyone choosing that. It would move on its own if the crate retuned
/// `Auto`, or if anything upstream ever buffered the first write.
///
/// **4 MB, which is what the `lz4` CLI uses, is 3.03% smaller and the wrong trade.** Measured on the
/// kernel tree, one binary, three alternating rounds:
///
/// | | 64 KB | 4 MB |
/// |---|---|---|
/// | archive | 763,711,608 | **740,584,433** |
/// | `cram x` | **3.72 s** | 4.16 s |
/// | `cram t` (decode only) | 1.51 s | 1.48 s |
/// | `lz4 -dc \| tar` | **2.07 s** | 2.59 s |
///
/// Decode is *not* what gets slower — `cram t` is flat, so the cost lands in extraction, and in the
/// pipe for anyone using the native tool. Paying 12% of our read speed and 24% of theirs to save 3%
/// is backwards for the one codec chosen because it is fast to read; a caller who wants 3% has zstd
/// beside it at 30% smaller. Recorded here so the 3.03% is not rediscovered and taken.
const LZ4_BLOCK: BlockSize = BlockSize::Max64KB;

const BR_BUF: usize = 256 << 10;

/// The window brotli's own CLI uses by default, and what we have always asked for.
const BR_LGWIN: i32 = 22;

/// Encoder settings for a `.tar.br`, **including the size hint**, which is the whole point of this
/// function existing rather than a bare `CompressorWriter::new`.
///
/// brotli picks its hash table from `size_hint` alone: `ChooseHasher` reaches H6 — 15 bucket bits, a
/// 5-byte hash — only when the hint is over 4 MiB and `lgwin` is at least 19, and otherwise a
/// quality-6 stream falls to H5, whose bucket count drops to **14 bits** when the hint is under
/// 1 MiB. `CompressorWriter::new` leaves the hint at 0.
///
/// A caller that never sets it does not get 0, which is the part that makes this easy to miss.
/// `update_size_hint` fills it in from `available_in` **on the first write**, so whoever hands the
/// encoder its whole input in one `write_all` is silently given a correct hint and a correct hasher.
/// tar does the opposite: it streams through `io::copy` in ~8 KiB writes, so the inferred hint was
/// ~8 KiB and every `.tar.br` cram ever wrote used a 16,384-bucket table however large the archive
/// was. The table size is fixed, so the more data pushed through it the more matches it misses —
/// which is why the loss against `brotli -q 6` *grew* with the corpus, 16.1% on 203 MiB against
/// 23.55% on 2.1 GB. Setting it explicitly took silesia from 69,147,136 to 58,497,939 and the kernel
/// tree from 608,920,976 to 487,982,888, both now slightly under what `brotli -q 6` writes.
///
/// The hint only has to be the right order of magnitude — it selects a hasher, it does not bound
/// anything — so an absent count is not a reason to fall back to 0. An archive whose size we could
/// not count beforehand is far more likely to be large than to be under 4 MiB, and guessing high
/// costs a few MiB of hash table on a small one.
fn br_params(level: Level, total_bytes: Option<u64>) -> BrotliEncoderParams {
    BrotliEncoderParams {
        quality: br_quality(level) as i32,
        lgwin: BR_LGWIN,
        size_hint: total_bytes
            .unwrap_or(u64::from(u32::MAX))
            .min(usize::MAX as u64) as usize,
        ..Default::default()
    }
}

/// The tar archive name for an entry: normalized-safe relative path, forward slashes.
fn tar_name(entry: &Entry) -> String {
    entry.path.safe().to_string_lossy().replace('\\', "/")
}

pub struct TarArchiveWriter {
    /// `Option` so `finish` can move the builder out.
    builder: Option<Builder<TarSink>>,
    entries: u64,
    in_bytes: u64,
    start: Instant,
}

impl TarArchiveWriter {
    pub fn create(path: &Path, fmt: Format, opts: &CreateOptions) -> Result<Self> {
        if opts.encrypt.is_some() {
            // tar has no encryption slot, wrap it in .zip/.cram to encrypt.
            return Err(ArchiveError::UnsupportedEncryption);
        }
        let mut file = BufWriter::new(File::create(path)?);
        let lvl = preset(opts.level);
        let sink = match fmt.codec {
            Codec::None => TarSink::Plain(file),
            Codec::Gzip => {
                file.write_all(&gzip_header(lvl))?;
                TarSink::Gz(ChunkedSink::new(file, ChunkCodec::Gzip, opts.level))
            }
            Codec::Xz => TarSink::Chunked(ChunkedSink::new(file, ChunkCodec::Xz, opts.level)),
            Codec::Bzip2 => TarSink::Chunked(ChunkedSink::new(file, ChunkCodec::Bzip2, opts.level)),
            // lz4 stays a single streaming frame; see [`ChunkCodec`] for why it is not chunked, and
            // [`LZ4_BLOCK`] for why the frame info is set rather than left to default.
            Codec::Lz4 => TarSink::Lz4(Box::new(FrameEncoder::with_frame_info(
                FrameInfo::new().block_size(LZ4_BLOCK),
                file,
            ))),
            Codec::Brotli => TarSink::Br(Box::new(CompressorWriter::with_params(
                file,
                BR_BUF,
                &br_params(opts.level, opts.total_bytes),
            ))),
            Codec::Zstd => TarSink::Zstd(ZstdSink::new(file, opts.level)?),
        };
        Ok(Self {
            builder: Some(Builder::new(sink)),
            entries: 0,
            in_bytes: 0,
            start: Instant::now(),
        })
    }

    fn builder(&mut self) -> Result<&mut Builder<TarSink>> {
        self.builder
            .as_mut()
            .ok_or_else(|| ArchiveError::Backend("tar writer already finished".into()))
    }
}

/// The entry's mtime as tar's unix-seconds field. `0` when the source carried no timestamp (or a
/// pre-1970 one), matching tar's own behavior for timestamp-less members. mtime comes from the
/// source file, not the wall clock, so an identical input tree still produces an identical tar.
fn mtime_secs(entry: &Entry) -> u64 {
    entry
        .modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl ArchiveWriter for TarArchiveWriter {
    fn add_file(&mut self, entry: &Entry, body: &mut dyn io::Read, _hint: WriteHint) -> Result<()> {
        // tar is a whole-stream codec (the whole concatenation is compressed as one), so there is
        // no per-entry method to switch, the incompressibility hint is handled upstream by the
        // engine dropping an Auto level to Fastest for a mostly-incompressible input.
        let mut header = Header::new_gnu();
        header.set_size(entry.size);
        header.set_mode(entry.unix_mode.unwrap_or(0o644));
        header.set_mtime(mtime_secs(entry));
        header.set_entry_type(EntryType::Regular);
        let name = tar_name(entry);
        // `append_data` writes the header (with `size`) then pads the body to however many bytes the
        // reader yields, so the body MUST be exactly `entry.size` long or the archive desyncs. The
        // live file may have changed size since it was recorded; `ExactReader` forces the exact
        // length (truncate-if-grew / zero-pad-if-shrank) so the archive is always valid.
        let mut exact = ExactReader {
            inner: body,
            remaining: entry.size,
            padded: false,
        };
        self.builder()?.append_data(&mut header, name, &mut exact)?;
        // The pad/truncate above keeps the archive structurally valid, but the entry's bytes then
        // differ from the source, that must surface as an error, not a silent success.
        if exact.padded {
            return Err(ArchiveError::Backend(format!(
                "{}: file shrank while being archived (tail zero-padded)",
                entry.path.raw()
            )));
        }
        let mut probe = [0u8; 1];
        if body.read(&mut probe)? != 0 {
            return Err(ArchiveError::Backend(format!(
                "{}: file grew while being archived (content truncated to the recorded size)",
                entry.path.raw()
            )));
        }
        self.entries += 1;
        self.in_bytes += entry.size;
        Ok(())
    }

    fn add_dir(&mut self, entry: &Entry) -> Result<()> {
        let mut header = Header::new_gnu();
        header.set_size(0);
        header.set_mode(entry.unix_mode.unwrap_or(0o755));
        header.set_mtime(mtime_secs(entry));
        header.set_entry_type(EntryType::Directory);
        let mut name = tar_name(entry);
        if !name.ends_with('/') {
            name.push('/'); // tar dir convention
        }
        self.builder()?
            .append_data(&mut header, name, io::empty())?;
        self.entries += 1;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<CreateReport> {
        let builder = self
            .builder
            .take()
            .ok_or_else(|| ArchiveError::Backend("tar writer already finished".into()))?;
        // `into_inner` writes the tar end-of-archive trailer, then we finalize the codec stream.
        let sink = builder.into_inner()?;
        let file = sink.finish()?;
        let out_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(CreateReport {
            entries: self.entries,
            in_bytes: self.in_bytes,
            out_bytes,
            stored: 0,
            dedup_saved: 0,
            elapsed: self.start.elapsed(),
            // Filled in by the engine walk, which is the only thing that sees the source tree.
            skipped_links: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Read an `ExactReader` fully into a Vec (small buffer to exercise the multi-read path),
    /// returning the bytes and whether the tail was zero-padded (source shrank).
    fn drain_flag(mut src: &[u8], declared: u64) -> (Vec<u8>, bool) {
        let mut r = ExactReader {
            inner: &mut src,
            remaining: declared,
            padded: false,
        };
        let mut out = Vec::new();
        let mut buf = [0u8; 7];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        (out, r.padded)
    }

    fn drain(src: &[u8], declared: u64) -> Vec<u8> {
        drain_flag(src, declared).0
    }

    #[test]
    fn exact_when_source_matches() {
        let data = b"hello world";
        assert_eq!(drain(data, data.len() as u64), data);
    }

    #[test]
    fn truncates_when_source_grew() {
        // Recorded size 5, but the source now has 11 bytes → only the first 5 are emitted.
        assert_eq!(drain(b"hello world", 5), b"hello");
    }

    #[test]
    fn zero_pads_when_source_shrank_and_flags_it() {
        // Recorded size 8, source only has 3 bytes → padded with zeros to exactly 8, and the
        // padding is FLAGGED so `add_file` can fail the create instead of silently archiving
        // zero-filled content.
        let (bytes, padded) = drain_flag(b"abc", 8);
        assert_eq!(bytes, b"abc\0\0\0\0\0");
        assert!(
            padded,
            "a shrunken source must be flagged, not silently padded"
        );
        // The exact-match case must NOT flag.
        let (_, ok_padded) = drain_flag(b"hello", 5);
        assert!(!ok_padded);
    }

    #[test]
    fn empty_declared_reads_nothing() {
        assert_eq!(drain(b"anything", 0), b"");
    }

    /// Decode a `.gz` with a decoder that is not ours: flate2's `GzDecoder` parses the header,
    /// inflates the concatenated chunks as one stream, and **verifies the CRC32 and ISIZE trailer**,
    /// so a full read that succeeds is proof of the framing as much as of the bytes.
    fn gunzip(path: &Path) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(File::open(path).unwrap())
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    /// The header written by hand must be the one flate2 writes, or a `.gz` from cram differs from
    /// every other `.gz` in a way nobody would think to look for.
    #[test]
    fn gzip_header_matches_flate2() {
        for lvl in [0u32, 1, 6, 9] {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), Compression::new(lvl));
            e.write_all(b"x").unwrap();
            let theirs = e.finish().unwrap();
            assert_eq!(
                &theirs[..10],
                &gzip_header(lvl)[..],
                "gzip header must match flate2's at level {lvl}"
            );
        }
    }

    /// The parallel gzip path with a deliberately narrow cut, so a full window flushes mid-stream
    /// and a partial tail closes the archive. In the shipped binary the window is the core count and
    /// the chunk is a megabyte, which no test-sized input would ever reach.
    #[test]
    fn gz_chunks_concatenate_across_windows() {
        const TEST_CHUNK: usize = 64 * 1024;

        let dir = std::env::temp_dir().join(format!("cram-gzwin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("w.gz");

        let mut file = BufWriter::new(File::create(&path).unwrap());
        file.write_all(&gzip_header(1)).unwrap();
        let mut inner = ChunkedSink::new(file, ChunkCodec::Gzip, Level::Fastest);
        // A cut small enough to reach the multi-chunk path without deflating megabytes:
        // dependencies are unoptimised in a debug test build, and miniz_oxide there runs about a
        // hundred times slower than in the shipped binary.
        inner.chunk = TEST_CHUNK;
        inner.width = 2;
        let mut sink = TarSink::Gz(inner);

        // Three whole chunks and a tail: one full window flushes mid-stream, then a chunk plus the
        // tail flush at finish. The chunk index is mixed into the bytes so that emitting the pieces
        // out of order changes the output — on uniform data a reordering bug would round-trip
        // perfectly and the test would pass.
        let data: Vec<u8> = (0..TEST_CHUNK * 3 + 1234)
            .map(|i| ((i / 97) as u8).wrapping_add(((i / TEST_CHUNK) as u8).wrapping_mul(61)))
            .collect();
        // Fed in small pieces, the way tar's `append_data` feeds it.
        for piece in data.chunks(8192) {
            sink.write_all(piece).unwrap();
        }
        sink.finish().unwrap();

        assert_eq!(
            gunzip(&path),
            data,
            "concatenated deflate chunks must inflate to the original, in order"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The premise of [`ChunkedSink`] is that complete streams of these codecs concatenate into one
    /// valid stream. That is a claim about each format, and it is asserted here per codec rather
    /// than assumed: several chunks in, one continuous byte sequence out.
    ///
    /// The chunk is set small by hand — the shipped ones are 4 to 32 MiB, which no test should
    /// deflate in a debug build where dependencies are unoptimised.
    #[test]
    fn chunked_codecs_concatenate_into_one_stream() {
        use crate::codec::decode_stream;

        const TEST_CHUNK: usize = 48 * 1024;

        let dir = std::env::temp_dir().join(format!("cram-chunkcat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Four whole chunks and a tail, with the chunk index mixed into the bytes so that emitting
        // the pieces out of order changes the output rather than round-tripping regardless.
        let data: Vec<u8> = (0..TEST_CHUNK * 4 + 777)
            .map(|i| ((i / 61) as u8).wrapping_add(((i / TEST_CHUNK) as u8).wrapping_mul(53)))
            .collect();

        // lz4 is absent on purpose: its frames concatenate but our reader stops at the first one,
        // so it is not chunked. See [`ChunkCodec`].
        let cases: [(ChunkCodec, Codec, &str); 2] = [
            (ChunkCodec::Xz, Codec::Xz, "xz"),
            (ChunkCodec::Bzip2, Codec::Bzip2, "bz2"),
        ];

        for (chunk_codec, stream_codec, name) in cases {
            let path = dir.join(format!("c.{name}"));
            let mut sink = ChunkedSink::new(
                BufWriter::new(File::create(&path).unwrap()),
                chunk_codec,
                Level::Fastest,
            );
            // Narrow the cut so a test-sized input still crosses several chunks, and keep the pool
            // narrower than the chunk count so the writer really has to reorder rather than
            // receiving everything in the order it was submitted.
            sink.chunk = TEST_CHUNK;
            sink.width = 2;
            // `write` here is the sink's own, not the `Write` trait: it always takes the whole
            // slice, since it only appends to the pending buffer.
            for piece in data.chunks(4096) {
                assert_eq!(sink.write(piece).unwrap(), piece.len());
            }
            sink.finish().unwrap().0.into_inner().unwrap();

            let src: Box<dyn io::Read + Send> = Box::new(File::open(&path).unwrap());
            let mut out = Vec::new();
            decode_stream(stream_codec, src)
                .unwrap()
                .read_to_end(&mut out)
                .unwrap();
            // Length first: a truncation at the first stream boundary is the failure this is
            // looking for, and comparing megabytes of bytes to say so prints a megabyte.
            assert_eq!(
                out.len(),
                data.len(),
                "{name}: decoded {} of {} bytes — a concatenated stream was truncated, \
                 probably at the first boundary ({} = one chunk)",
                out.len(),
                data.len(),
                TEST_CHUNK
            );
            assert!(
                out == data,
                "{name}: concatenated streams decoded to the right length but the wrong bytes"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real archive, built the way `create`/`finish` build one, read back by a decoder that is not
    /// ours. The point is interoperability: header, stream and trailer assembled by hand have to
    /// satisfy something that has never heard of cram.
    #[test]
    fn tar_gz_reads_with_a_standard_gzip_decoder() {
        use crate::model::{EntryKind, EntryPath};

        let dir = std::env::temp_dir().join(format!("cram-tgz-std-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("small.tar.gz");

        let body: Vec<u8> = (0..40_000u32).map(|i| (i / 131) as u8).collect();
        let entry = Entry {
            index: 0,
            path: EntryPath::from_raw("big.bin").unwrap(),
            kind: EntryKind::File,
            size: body.len() as u64,
            compressed_size: None,
            modified: None,
            unix_mode: None,
            crc32: None,
            encrypted: false,
        };
        let mut w = Box::new(
            TarArchiveWriter::create(
                &archive,
                Format::tar(Codec::Gzip),
                &CreateOptions::default(),
            )
            .unwrap(),
        );
        w.add_file(&entry, &mut &body[..], WriteHint::default())
            .unwrap();
        w.finish().unwrap();

        let mut tar = tar::Archive::new(io::Cursor::new(gunzip(&archive)));
        let mut found = false;
        for item in tar.entries().unwrap() {
            let mut e = item.unwrap();
            let mut got = Vec::new();
            e.read_to_end(&mut got).unwrap();
            assert_eq!(
                got, body,
                "a cram .tar.gz must decode with a stock gzip reader"
            );
            found = true;
        }
        assert!(found, "the entry must be present");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An input larger than one `ZSTD_FRAME_CHUNK`, which is the size that matters for the pure-Rust
    /// writer: it forces several concatenated frames and the decode side has to reassemble the tar
    /// byte-for-byte. With `zstd-c` the same input is one streamed frame, so this covers whichever
    /// writer the build actually has, and asserts the same thing of both.
    #[test]
    fn tar_zst_round_trips_whichever_writer_is_built() {
        use crate::codec::decode_stream;
        use crate::model::{EntryKind, EntryPath};

        let dir = std::env::temp_dir().join(format!("cram-tzst-mf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("big.tar.zst");

        // ~20 MiB of varied-but-compressible bytes: > 2 chunks, quick to compress at Fastest.
        let body: Vec<u8> = (0..20 * 1024 * 1024u32).map(|i| (i / 4096) as u8).collect();
        let entry = Entry {
            index: 0,
            path: EntryPath::from_raw("big.bin").unwrap(),
            kind: EntryKind::File,
            size: body.len() as u64,
            compressed_size: None,
            modified: None,
            unix_mode: None,
            crc32: None,
            encrypted: false,
        };
        let mut w = Box::new(
            TarArchiveWriter::create(
                &archive,
                Format::tar(Codec::Zstd),
                &CreateOptions::default(),
            )
            .unwrap(),
        );
        w.add_file(&entry, &mut &body[..], WriteHint::default())
            .unwrap();
        w.finish().unwrap();

        // Decode through the live multi-frame zstd path and re-read the tar.
        let file: Box<dyn io::Read + Send> = Box::new(std::fs::File::open(&archive).unwrap());
        let decoded = decode_stream(crate::format::Codec::Zstd, file).unwrap();
        let mut tar = tar::Archive::new(decoded);
        let mut found = false;
        for item in tar.entries().unwrap() {
            let mut e = item.unwrap();
            let mut got = Vec::new();
            io::Read::read_to_end(&mut e, &mut got).unwrap();
            assert_eq!(got, body, "multi-frame tar.zst must round-trip exactly");
            found = true;
        }
        assert!(found, "the entry must be present");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// gzip's trailer is a CRC32 of the whole stream, assembled from per-chunk CRCs by
    /// [`Crc::combine`]. That is associative but **not** commutative, so folding chunks in the order
    /// they happened to finish gives a checksum that depends on thread scheduling and disagrees with
    /// the bytes on disk. `gunzip` verifies the trailer, so this catches it — and it must be checked
    /// at more than one width, because at width 1 completion order and index order are the same and
    /// a broken fold would pass.
    #[test]
    fn gz_trailer_does_not_depend_on_pool_width() {
        const TEST_CHUNK: usize = 64 * 1024;

        let dir = std::env::temp_dir().join(format!("cram-gzcrc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Cheap and expensive chunks alternating, so completion order will not be index order.
        let mut data = Vec::with_capacity(TEST_CHUNK * 7);
        let mut seed = 0xDEAD_BEEF_CAFE_F00Du64;
        for c in 0..7 {
            if c % 2 == 0 {
                data.resize(data.len() + TEST_CHUNK, 0u8);
            } else {
                for _ in 0..TEST_CHUNK {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    data.push(seed as u8);
                }
            }
        }

        let mut first: Option<Vec<u8>> = None;
        for width in [1usize, 2, 4, 8] {
            let path = dir.join(format!("g{width}.gz"));
            let mut file = BufWriter::new(File::create(&path).unwrap());
            file.write_all(&gzip_header(1)).unwrap();
            let mut inner = ChunkedSink::new(file, ChunkCodec::Gzip, Level::Fastest);
            inner.chunk = TEST_CHUNK;
            inner.width = width;
            let mut sink = TarSink::Gz(inner);
            for piece in data.chunks(8192) {
                sink.write_all(piece).unwrap();
            }
            sink.finish().unwrap();

            // gunzip checks the CRC and the length, so this fails loudly on a mis-ordered fold.
            assert_eq!(
                gunzip(&path),
                data,
                "width {width}: the gzip trailer disagrees with the stream"
            );
            let bytes = std::fs::read(&path).unwrap();
            match &first {
                None => first = Some(bytes),
                Some(f) => assert_eq!(&bytes, f, "width {width} produced a different .gz"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pipeline writes chunks in index order, not completion order, so the archive must not
    /// depend on how many workers happened to be running. Chunks finish at genuinely different times
    /// — an xz chunk of zeros is far quicker than one of noise — so with a wide pool the writer is
    /// reordering for real, and a bug there would show up as a width-dependent file.
    ///
    /// This is the property the old barrier got for free by never letting two windows overlap, and
    /// the one thing a pipeline can plausibly break.
    #[test]
    fn chunk_order_does_not_depend_on_pool_width() {
        const TEST_CHUNK: usize = 96 * 1024;

        let dir = std::env::temp_dir().join(format!("cram-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Alternating cheap and expensive chunks: runs of zeros compress almost instantly, xorshift
        // noise does not, so completion order will not match submission order.
        let mut data = Vec::with_capacity(TEST_CHUNK * 9);
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for c in 0..9 {
            if c % 2 == 0 {
                data.resize(data.len() + TEST_CHUNK, 0u8);
            } else {
                for _ in 0..TEST_CHUNK {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    data.push(seed as u8);
                }
            }
        }

        let run = |width: usize| -> Vec<u8> {
            let path = dir.join(format!("w{width}.xz"));
            let mut sink = ChunkedSink::new(
                BufWriter::new(File::create(&path).unwrap()),
                ChunkCodec::Xz,
                Level::Fastest,
            );
            sink.chunk = TEST_CHUNK;
            sink.width = width;
            for piece in data.chunks(4096) {
                sink.write(piece).unwrap();
            }
            sink.finish().unwrap().0.into_inner().unwrap();
            std::fs::read(&path).unwrap()
        };

        let one = run(1);
        for width in [2usize, 3, 8] {
            assert_eq!(
                run(width),
                one,
                "pool width {width} produced a different archive: the writer is emitting chunks in \
                 completion order rather than index order"
            );
        }

        // And it must still decode back to exactly what went in.
        use crate::codec::decode_stream;
        let src: Box<dyn io::Read + Send> = Box::new(io::Cursor::new(one));
        let mut out = Vec::new();
        decode_stream(Codec::Xz, src)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out.len(), data.len(), "decoded length differs");
        assert!(out == data, "decoded bytes differ");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// brotli's `ChooseHasher` reaches H6 only when `size_hint > 4 MiB` **and** `lgwin >= 19`, and
    /// H5 drops to 14 bucket bits when the hint is under 1 MiB. Those two thresholds are the entire
    /// reason [`br_params`] exists, so assert against the numbers rather than against "it is set".
    ///
    /// An **unknown** total must clear the bar, since a stream we could not count is far likelier to
    /// be large than tiny. A *known* small total is a different thing and is left alone: H5 is the
    /// right hasher for a small archive, and the bug was never about those.
    #[test]
    fn br_params_clear_brotlis_h6_threshold() {
        const H6_MIN_HINT: usize = 1 << 22;
        const H6_MIN_LGWIN: i32 = 19;

        for level in [
            Level::Fastest,
            Level::Auto,
            Level::Balanced,
            Level::Best,
            Level::Cold,
            Level::Explicit(6),
        ] {
            for total in [None, Some(203 << 20), Some(2_100_000_000)] {
                let p = br_params(level, total);
                assert!(
                    p.lgwin >= H6_MIN_LGWIN,
                    "lgwin {} would force H5 whatever the hint",
                    p.lgwin
                );
                assert!(
                    p.size_hint > H6_MIN_HINT,
                    "size_hint {} at level {level:?}, total {total:?} falls back to H5's small table",
                    p.size_hint
                );
            }
            // A counted-and-genuinely-small archive keeps its honest figure.
            assert_eq!(br_params(level, Some(1234)).size_hint, 1234);
        }
    }

    /// The hint is worth ratio, not just a different code path. Two encoders, same quality and
    /// window, differing only in `size_hint` — the hinted one must not lose, and on data large
    /// enough to saturate a 16K-bucket table it should win outright.
    ///
    /// **The input must be fed in small writes, the way tar feeds it.** A single `write_all` of the
    /// whole buffer makes this test pass no matter what the code does: brotli's `update_size_hint`
    /// infers a hint from `available_in` when none was set, so one big write hands the unhinted
    /// encoder the right answer for free and both sides come out byte-identical. That is precisely
    /// how the bug hid for as long as it did, and a probe written that way reported "the hint makes
    /// no difference" on the very corpus where it was worth 15%.
    #[test]
    fn the_size_hint_is_worth_ratio() {
        // ~6 MiB drawn from a large vocabulary. A handful of repeated words is the wrong shape for
        // this test and was tried first: with only twelve tokens the two encoders produced byte-
        // identical output, because 16K buckets hold twelve strings comfortably. The table size can
        // only matter when the number of distinct contexts exceeds it, so the vocabulary here is
        // deliberately bigger than H5's 16,384 buckets.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let vocab: Vec<String> = (0..40_000)
            .map(|_| {
                let n = 4 + (rng() % 9) as usize;
                (0..n)
                    .map(|_| (b'a' + (rng() % 26) as u8) as char)
                    .collect()
            })
            .collect();
        let mut corpus = Vec::with_capacity(6 << 20);
        while corpus.len() < (6 << 20) {
            let w = &vocab[(rng() % vocab.len() as u64) as usize];
            corpus.extend_from_slice(w.as_bytes());
            corpus.push(if rng() & 0xF == 0 { b'\n' } else { b' ' });
        }

        let encode = |params: &BrotliEncoderParams| {
            let mut w = CompressorWriter::with_params(Vec::new(), BR_BUF, params);
            // 8 KiB at a time: `io::copy`'s buffer, which is how the tar builder reaches the sink.
            for c in corpus.chunks(8 << 10) {
                w.write_all(c).unwrap();
            }
            w.into_inner().len()
        };

        let hinted = encode(&br_params(Level::Balanced, Some(corpus.len() as u64)));
        let unhinted = encode(&BrotliEncoderParams {
            size_hint: 0,
            ..br_params(Level::Balanced, None)
        });

        assert!(
            hinted <= unhinted,
            "hinting the size must never cost ratio: {hinted} hinted vs {unhinted} unhinted"
        );
        assert!(
            hinted < unhinted,
            "on {} MiB the hint should buy a real win, got {hinted} against {unhinted}",
            corpus.len() >> 20
        );
    }
}
