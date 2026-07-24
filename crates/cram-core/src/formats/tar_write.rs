//! tar writer backend, creates `.tar` and, by wrapping the output sink in a whole-stream encoder,
//! the full tar-family: `.tar.gz` `.tar.xz` `.tar.bz2` `.tar.lz4` `.tar.br` `.tar.zst`. Built on the
//! `tar` crate's `Builder` (`append_data` per entry, trailer on `into_inner`); the codec wrapper is
//! chosen at construction and finalized explicitly in `finish` (each encoder needs its trailer
//! flushed that dropping alone wouldn't guarantee cleanly).
//!
//! All encoders are pure-Rust and stream, except **zstd**: `ruzstd`'s encoder is pull-model with no
//! `Write` sink, so `.tar.zst` accumulates a bounded chunk (8 MiB) and emits it as an independent
//! zstd *frame*, concatenated frames are spec-legal (`cat`/pzstd produce them; our reader and
//! `zstd -d` decode them all), so memory stays bounded instead of buffering the entire tar. Since
//! ruzstd only implements its `Fastest` level, `.tar.zst` is a fast-tier archive regardless of
//! `--best` (use `.tar.xz` for maximum ratio). A future `zstd-c` feature can swap in the C zstd.
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
use flate2::write::GzEncoder;
use flate2::Compression;
use lz4_flex::frame::FrameEncoder;
use lzma_rust2::{XzOptions, XzWriter};
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
    Gz(Box<GzEncoder<BufWriter<File>>>),
    Xz(Box<XzWriter<BufWriter<File>>>),
    Bz2(Box<BzEncoder<BufWriter<File>>>),
    Lz4(Box<FrameEncoder<BufWriter<File>>>),
    Br(Box<CompressorWriter<BufWriter<File>>>),
    /// zstd accumulates at most [`ZSTD_FRAME_CHUNK`] bytes (ruzstd has no `Write` sink), emitting
    /// each full chunk as its own zstd frame, bounded memory instead of buffering the whole tar.
    Zstd {
        buf: Vec<u8>,
        file: BufWriter<File>,
        level: CompressionLevel,
    },
}

/// Bytes of tar accumulated before being compressed out as one independent zstd frame. Bounds the
/// `.tar.zst` writer's memory (the old design held the ENTIRE tar in RAM until `finish`, archiving
/// a 100 GiB tree OOM'd the process). Per-frame compression costs a little ratio; at ruzstd's
/// Fastest level the difference is small.
const ZSTD_FRAME_CHUNK: usize = 8 * 1024 * 1024;

impl TarSink {
    /// Flush/finalize the codec trailer and hand back the file (for the final-size measurement).
    fn finish(self) -> io::Result<File> {
        let buf = match self {
            TarSink::Plain(w) => w,
            TarSink::Gz(e) => e.finish()?,
            TarSink::Xz(e) => e.finish()?,
            TarSink::Bz2(e) => e.finish()?,
            TarSink::Lz4(e) => e.finish().map_err(io::Error::other)?,
            TarSink::Br(e) => e.into_inner(),
            TarSink::Zstd {
                buf,
                mut file,
                level,
            } => {
                // Compress the final partial chunk as the last frame (in memory, then write out;
                // ruzstd's streaming `compress` swallows target write errors).
                if !buf.is_empty() {
                    file.write_all(&compress_to_vec(&buf[..], level))?;
                }
                file
            }
        };
        buf.into_inner().map_err(|e| e.into_error())
    }
}

impl Write for TarSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TarSink::Plain(w) => w.write(buf),
            TarSink::Gz(w) => w.write(buf),
            TarSink::Xz(w) => w.write(buf),
            TarSink::Bz2(w) => w.write(buf),
            TarSink::Lz4(w) => w.write(buf),
            TarSink::Br(w) => w.write(buf),
            TarSink::Zstd {
                buf: b,
                file,
                level,
            } => {
                b.extend_from_slice(buf);
                // A full chunk becomes one independent zstd frame, memory stays ≤ ~1 chunk.
                if b.len() >= ZSTD_FRAME_CHUNK {
                    file.write_all(&compress_to_vec(&b[..], *level))?;
                    b.clear();
                }
                Ok(buf.len())
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            TarSink::Plain(w) => w.flush(),
            TarSink::Gz(w) => w.flush(),
            TarSink::Xz(w) => w.flush(),
            TarSink::Bz2(w) => w.flush(),
            TarSink::Lz4(w) => w.flush(),
            TarSink::Br(w) => w.flush(),
            TarSink::Zstd { buf: b, .. } => b.flush(),
        }
    }
}

/// Map the abstract [`Level`] onto the 0–9 preset gzip/xz share (`None`-equivalent default is 6).
fn preset(level: Level) -> u32 {
    match level {
        Level::Auto | Level::Balanced => 6,
        Level::Fastest => 1,
        Level::Best => 9,
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
        Level::Best => 11,
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
        let file = BufWriter::new(File::create(path)?);
        let lvl = preset(opts.level);
        let sink = match fmt.codec {
            Codec::None => TarSink::Plain(file),
            Codec::Gzip => TarSink::Gz(Box::new(GzEncoder::new(file, Compression::new(lvl)))),
            Codec::Xz => TarSink::Xz(Box::new(XzWriter::new(file, XzOptions::with_preset(lvl))?)),
            Codec::Bzip2 => TarSink::Bz2(Box::new(BzEncoder::new(
                file,
                bzip2::Compression::new(bz_level(opts.level)),
            ))),
            Codec::Lz4 => TarSink::Lz4(Box::new(FrameEncoder::new(file))),
            Codec::Brotli => TarSink::Br(Box::new(CompressorWriter::new(
                file,
                4096,
                br_quality(opts.level),
                22,
            ))),
            // ruzstd only implements its `Fastest` level (see the module docs).
            Codec::Zstd => TarSink::Zstd {
                buf: Vec::new(),
                file,
                level: CompressionLevel::Fastest,
            },
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

    /// An input larger than one `ZSTD_FRAME_CHUNK` forces the bounded writer to emit several
    /// concatenated zstd frames, the decode side must reassemble the tar byte-for-byte.
    #[test]
    fn tar_zst_multi_frame_round_trips() {
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
