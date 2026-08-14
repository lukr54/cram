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

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

use brotli::CompressorWriter;
use bzip2::write::BzEncoder;
use flate2::{Compress, Compression, Crc, FlushCompress};
use lz4_flex::frame::FrameEncoder;
use lzma_rust2::{XzOptions, XzWriter};
use rayon::prelude::*;
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
    /// the pieces are then concatenated. A window of `window_max` of them is deflated in parallel
    /// and written **in order**, because the concatenation *is* the DEFLATE stream.
    ///
    /// Nothing about the result is unusual to a reader: one gzip header, one DEFLATE stream, one
    /// trailer, so `gzip -d`, `zcat` and `tar -xzf` see an ordinary `.gz`. What makes the pieces
    /// separable is that each compressor starts empty, so no back-reference can point outside its
    /// own piece. That also costs a little ratio (each seam throws away up to 32 KiB of dictionary),
    /// which is why the chunk is 1 MiB rather than pigz's 128 KiB.
    Gz {
        /// Tar bytes not yet a whole chunk.
        buf: Vec<u8>,
        /// Whole chunks waiting for the window to fill.
        window: Vec<Vec<u8>>,
        /// How the work is cut: bytes per chunk, and chunks per parallel window. Fields rather than
        /// constants because the tests need a cut small enough to reach the multi-window path
        /// without deflating megabytes — dependencies are unoptimised in a debug test build, and
        /// miniz_oxide there runs about a hundred times slower than in the shipped binary.
        chunk: usize,
        window_max: usize,
        file: BufWriter<File>,
        level: u32,
        /// CRC32 and length of everything already compressed: the gzip trailer, accumulated a chunk
        /// at a time via [`Crc::combine`] so the checksum is computed in parallel with the deflate
        /// rather than as a second serial pass over the same bytes.
        crc: Crc,
    },
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
/// **lz4 is deliberately not here.** Its frames concatenate in the format, but our reader does not
/// follow them: `lz4_flex`'s `FrameDecoder` stops at the first frame, so a chunked `.tar.lz4` would
/// be written correctly and read back truncated. Measured while adding this — 49,152 of 197,385
/// bytes, exactly one chunk. That is a reader bug in its own right and is fixed separately; until
/// then lz4 stays a single streaming frame, which is what it always was.
#[derive(Clone, Copy)]
enum ChunkCodec {
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
    /// 900 KB and lz4's window 64 KB, so neither needs the room.
    ///
    /// Whatever the value, it also bounds memory. That matters historically: the first `.tar.zst`
    /// writer held the ENTIRE tar in RAM until `finish`, and archiving a 100 GiB tree OOM'd the
    /// process.
    fn chunk(self) -> usize {
        match self {
            ChunkCodec::Xz => 32 << 20,
            ChunkCodec::Bzip2 => 4 << 20,
            #[cfg(not(feature = "zstd-c"))]
            ChunkCodec::Zstd => 8 << 20,
        }
    }

    /// One chunk as a complete, standalone stream.
    fn compress(self, data: &[u8], level: Level) -> io::Result<Vec<u8>> {
        match self {
            ChunkCodec::Xz => {
                let mut w = XzWriter::new(Vec::new(), XzOptions::with_preset(preset(level)))?;
                w.write_all(data)?;
                w.finish()
            }
            ChunkCodec::Bzip2 => {
                let mut w = BzEncoder::new(Vec::new(), bzip2::Compression::new(bz_level(level)));
                w.write_all(data)?;
                w.finish()
            }
            #[cfg(not(feature = "zstd-c"))]
            ChunkCodec::Zstd => Ok(compress_to_vec(data, CompressionLevel::Fastest)),
        }
    }
}

/// Total chunk bytes allowed in flight. Bounds the pool rather than letting the core count do it:
/// 24 workers each holding a 32 MiB xz chunk plus its output is most of a gigabyte on a machine that
/// may not have one to spare, and `xz -T0` reaching 3.2 GB on this corpus is the behaviour being
/// avoided rather than copied.
const CHUNK_BUDGET: usize = 512 << 20;

/// Cut the tar into independent chunks, compress a window of them in parallel, write them in order.
struct ChunkedSink {
    buf: Vec<u8>,
    window: Vec<Vec<u8>>,
    chunk: usize,
    window_max: usize,
    file: BufWriter<File>,
    codec: ChunkCodec,
    level: Level,
}

impl ChunkedSink {
    fn new(file: BufWriter<File>, codec: ChunkCodec, level: Level) -> Self {
        let chunk = codec.chunk();
        Self {
            buf: Vec::new(),
            window: Vec::new(),
            chunk,
            window_max: rayon::current_num_threads()
                .min(CHUNK_BUDGET / chunk)
                .max(1),
            file,
            codec,
            level,
        }
    }

    /// Compress a window in parallel and write it **in order** — the concatenation is the stream.
    fn flush_window(&mut self) -> io::Result<()> {
        let (codec, level) = (self.codec, self.level);
        let done: Vec<io::Result<Vec<u8>>> = match self.window.len() {
            0 => return Ok(()),
            // One chunk is every archive below the chunk size, so it skips rayon rather than
            // starting the global pool to run a single closure on the calling thread anyway.
            1 => vec![codec.compress(&self.window[0], level)],
            _ => self
                .window
                .par_iter()
                .map(|c| codec.compress(c, level))
                .collect(),
        };
        for item in done {
            self.file.write_all(&item?)?;
        }
        self.window.clear();
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        while self.buf.len() >= self.chunk {
            let tail = self.buf.split_off(self.chunk);
            self.window.push(std::mem::replace(&mut self.buf, tail));
            if self.window.len() >= self.window_max {
                self.flush_window()?;
            }
        }
        Ok(buf.len())
    }

    /// Deliberately does not cut a chunk: a flush mid-archive would fragment the stream for nothing.
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn finish(mut self) -> io::Result<BufWriter<File>> {
        if !self.buf.is_empty() {
            let tail = std::mem::take(&mut self.buf);
            self.window.push(tail);
        }
        self.flush_window()?;
        Ok(self.file)
    }
}

/// The zstd sink. One type, two implementations, so the feature split lives here rather than in
/// every arm of `write`, `flush` and `finish`.
#[cfg(feature = "zstd-c")]
struct ZstdSink(Box<zstd::stream::write::Encoder<'static, BufWriter<File>>>);

#[cfg(feature = "zstd-c")]
impl ZstdSink {
    fn new(file: BufWriter<File>, level: Level) -> io::Result<Self> {
        Ok(Self(Box::new(zstd::stream::write::Encoder::new(
            file,
            zstd_level(level),
        )?)))
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
        self.0.finish()
    }
}

/// Map the abstract [`Level`] onto zstd's 1–22 scale. `--auto` is zstd's own default, so a
/// head-to-head against `zstd` at *its* default compares like with like, the same way `--auto` means
/// gzip 6 and xz 6 above.
#[cfg(feature = "zstd-c")]
fn zstd_level(level: Level) -> i32 {
    match level {
        Level::Auto | Level::Balanced => 3,
        Level::Fastest => 1,
        Level::Best | Level::Cold => 19,
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

/// Deflate a full window of chunks in parallel and write them out **in order**, folding each one's
/// CRC into the running trailer as it goes.
fn flush_gz_window(
    window: &mut Vec<Vec<u8>>,
    file: &mut BufWriter<File>,
    level: u32,
    crc: &mut Crc,
) -> io::Result<()> {
    // One chunk is every archive under GZ_CHUNK, so it skips rayon rather than spinning up the
    // global pool to run a single closure on the calling thread anyway.
    let done: Vec<io::Result<(Vec<u8>, Crc)>> = match window.len() {
        0 => return Ok(()),
        1 => vec![deflate_chunk(&window[0], level)],
        _ => window
            .par_iter()
            .map(|chunk| deflate_chunk(chunk, level))
            .collect(),
    };
    for item in done {
        let (bytes, chunk_crc) = item?;
        file.write_all(&bytes)?;
        crc.combine(&chunk_crc);
    }
    window.clear();
    Ok(())
}

impl TarSink {
    /// Flush/finalize the codec trailer and hand back the file (for the final-size measurement).
    fn finish(self) -> io::Result<File> {
        let buf = match self {
            TarSink::Plain(w) => w,
            TarSink::Gz {
                mut buf,
                mut window,
                mut file,
                level,
                mut crc,
                ..
            } => {
                if !buf.is_empty() {
                    window.push(std::mem::take(&mut buf));
                }
                flush_gz_window(&mut window, &mut file, level, &mut crc)?;
                // A final empty fixed-Huffman block: BFINAL=1, BTYPE=01, then the 7-bit
                // end-of-block code, LSB-first and zero-padded to a byte. Every chunk above ended
                // on a byte boundary, so these two bytes land cleanly and close the stream.
                file.write_all(&[0x03, 0x00])?;
                file.write_all(&crc.sum().to_le_bytes())?;
                // ISIZE is the uncompressed length mod 2^32, which is exactly what `amount` holds.
                file.write_all(&crc.amount().to_le_bytes())?;
                file
            }
            TarSink::Chunked(c) => c.finish()?,
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
            TarSink::Gz {
                buf: pending,
                window,
                chunk,
                window_max,
                file,
                level,
                crc,
            } => {
                pending.extend_from_slice(buf);
                while pending.len() >= *chunk {
                    let tail = pending.split_off(*chunk);
                    window.push(std::mem::replace(pending, tail));
                    if window.len() >= *window_max {
                        flush_gz_window(window, file, *level, crc)?;
                    }
                }
                Ok(buf.len())
            }
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
            TarSink::Gz { file, .. } => file.flush(),
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
        Level::Best | Level::Cold => 9,
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
        Level::Best | Level::Cold => 11,
        Level::Explicit(n) => n.clamp(0, 11),
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
                TarSink::Gz {
                    buf: Vec::with_capacity(GZ_CHUNK + 64 * 1024),
                    window: Vec::new(),
                    chunk: GZ_CHUNK,
                    window_max: rayon::current_num_threads().max(1),
                    file,
                    level: lvl,
                    crc: Crc::new(),
                }
            }
            Codec::Xz => TarSink::Chunked(ChunkedSink::new(file, ChunkCodec::Xz, opts.level)),
            Codec::Bzip2 => TarSink::Chunked(ChunkedSink::new(file, ChunkCodec::Bzip2, opts.level)),
            // lz4 stays a single streaming frame; see [`ChunkCodec`] for why it is not chunked.
            Codec::Lz4 => TarSink::Lz4(Box::new(FrameEncoder::new(file))),
            Codec::Brotli => TarSink::Br(Box::new(CompressorWriter::new(
                file,
                4096,
                br_quality(opts.level),
                22,
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
        let mut sink = TarSink::Gz {
            buf: Vec::new(),
            window: Vec::new(),
            chunk: TEST_CHUNK,
            window_max: 2,
            file,
            level: 1,
            crc: Crc::new(),
        };

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
            // Narrow the cut so a test-sized input still crosses several windows.
            sink.chunk = TEST_CHUNK;
            sink.window_max = 2;
            // `write` here is the sink's own, not the `Write` trait: it always takes the whole
            // slice, since it only appends to the pending buffer.
            for piece in data.chunks(4096) {
                assert_eq!(sink.write(piece).unwrap(), piece.len());
            }
            sink.finish().unwrap().into_inner().unwrap();

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
}
