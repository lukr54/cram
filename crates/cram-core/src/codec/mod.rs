//! The codec layer, the byte transform for whole-stream codecs, plus the [`plan`] glue.
//!
//! Cram has THREE distinct "codec" notions that must not be conflated:
//! - [`crate::format::Codec`], the whole-stream *wrapper* (`.tar.gz`, `foo.xz`). Describes layout.
//! - [`crate::hw::Codec`], the *planning* cost class `derive_plan` reasons about (worker counts).
//! - this module, the concrete decompression ([`decode_stream`]), and [`plan`] mapping between the
//!   first two.
//!
//! ZIP and 7z do **not** go through [`decode_stream`], they decode per-entry via their own backend
//! (`zip` / `sevenz-rust2`) from the method stored in each entry's metadata. This path serves the
//! whole-stream `Raw`/`Tar` containers (`.gz`, `.xz`, `.zst`, `.bz2`, `.lz4`, `.br`). All pure-Rust.

use std::io::Read;

use crate::error::{ArchiveError, Result};
use crate::format::Codec as StreamCodec;

pub mod plan;

/// Frame walking for the pure-Rust zstd decoder, and nothing else. libzstd does all of this itself,
/// so with `zstd-c` none of it is reachable — it lives behind one module boundary rather than a
/// `cfg` per item, because dead code under `-D warnings` is a CI failure on a build nobody runs
/// locally.
#[cfg(not(feature = "zstd-c"))]
mod ruzstd_frames {
    use std::io::{self, Read};

    // zstd *skippable*-frame magic range (little-endian first 4 bytes): 0x184D2A50 ..= 0x184D2A5F.
    // Such frames carry out-of-band metadata (e.g. the seekable-format seek table); `zstd -d` skips
    // them and keeps decoding. Layout: magic(4) | frame_size(4, LE) | frame_size bytes.
    const ZSTD_SKIP_MIN: u32 = 0x184D_2A50;
    const ZSTD_SKIP_MAX: u32 = 0x184D_2A5F;

    type ZstdDec = ruzstd::decoding::StreamingDecoder<FramedSource, ruzstd::decoding::FrameDecoder>;

    /// Open a whole zstd stream: every concatenated data frame, skippable frames stepped over.
    pub(super) fn open(inner: Box<dyn Read + Send>) -> io::Result<MultiFrameZstd> {
        Ok(MultiFrameZstd {
            dec: next_zstd_frame(FramedSource::new(inner))?,
        })
    }

    /// Read exactly `buf.len()` bytes, or return `Ok(0)` if the reader is cleanly at EOF *before any
    /// byte*. Returns the count read: `0` (clean EOF), `buf.len()` (full), or a smaller value (a partial
    /// read the caller treats as truncation).
    fn read_full_or_eof(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match r.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }

    /// The byte source under a run of zstd frames, with a tiny (≤4 byte) push-back so a frame magic we
    /// read to inspect can be handed back to `ruzstd` intact. **One** instance is threaded through every
    /// frame (see [`next_zstd_frame`]), so a many-frame stream stays O(1) in reader-wrapper depth; a
    /// per-frame `Chain`/wrapper would nest one level per frame and blow the stack on a hostile stream.
    struct FramedSource {
        inner: Box<dyn Read + Send>,
        prefix: [u8; 4],
        prefix_len: usize,
        prefix_pos: usize,
    }

    impl FramedSource {
        fn new(inner: Box<dyn Read + Send>) -> Self {
            Self {
                inner,
                prefix: [0; 4],
                prefix_len: 0,
                prefix_pos: 0,
            }
        }
        /// Push 4 magic bytes back so the following reads re-emit them before continuing from `inner`.
        fn push_magic(&mut self, magic: [u8; 4]) {
            self.prefix = magic;
            self.prefix_len = 4;
            self.prefix_pos = 0;
        }
        /// Read the next 4-byte frame magic (prefix is empty here). `Ok(None)` at a clean EOF.
        fn read_magic(&mut self) -> io::Result<Option<[u8; 4]>> {
            let mut magic = [0u8; 4];
            match read_full_or_eof(self, &mut magic)? {
                0 => Ok(None),
                4 => Ok(Some(magic)),
                _ => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated zstd frame magic",
                )),
            }
        }
    }

    impl Read for FramedSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.prefix_pos < self.prefix_len {
                let n = (self.prefix_len - self.prefix_pos).min(buf.len());
                buf[..n].copy_from_slice(&self.prefix[self.prefix_pos..self.prefix_pos + n]);
                self.prefix_pos += n;
                return Ok(n);
            }
            self.inner.read(buf)
        }
    }

    /// Advance `src` past any *skippable* frames and build a decoder for the next data frame, or return
    /// `Ok(None)` at clean EOF. `ruzstd`'s `StreamingDecoder::new` *errors* on a skippable magic (and
    /// consumes the reader on error), so we parse frame magics ourselves and only hand real data frames
    /// to it, otherwise a leading skippable frame would fail a valid stream and an interleaved one would
    /// be mistaken for EOF, silently dropping every following data frame.
    fn next_zstd_frame(mut src: FramedSource) -> io::Result<Option<ZstdDec>> {
        loop {
            let Some(magic) = src.read_magic()? else {
                return Ok(None); // clean end of stream
            };
            let m = u32::from_le_bytes(magic);
            if (ZSTD_SKIP_MIN..=ZSTD_SKIP_MAX).contains(&m) {
                // Skippable frame: a 4-byte LE size, then that many bytes to discard. `io::copy`
                // returns Ok on a short source, so the count must be checked; otherwise a stream
                // truncated inside a skippable frame reads as a clean EOF and silently drops every
                // byte that should have followed the frame.
                let mut size = [0u8; 4];
                src.read_exact(&mut size)?;
                let skip = u32::from_le_bytes(size) as u64;
                let skipped = io::copy(&mut src.by_ref().take(skip), &mut io::sink())?;
                if skipped != skip {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated zstd skippable frame",
                    ));
                }
                continue;
            }
            // A real frame: push its magic back so ruzstd reads the full header, then build the decoder.
            src.push_magic(magic);
            let dec = ruzstd::decoding::StreamingDecoder::new(src)
                .map_err(|e| io::Error::other(format!("zstd: {e}")))?;
            return Ok(Some(dec));
        }
    }

    /// zstd multi-frame reader. `ruzstd`'s `StreamingDecoder` decodes only ONE frame, but the format;
    /// and `cat a.zst b.zst`, and `pzstd`; allows concatenated frames, optionally interleaved with
    /// skippable frames. When the current frame is exhausted we advance to the next data frame via
    /// [`next_zstd_frame`] (skipping skippable frames); when none follows, we're at EOF. Safe because
    /// zstd frames are self-delimiting: the block decoder consumes exactly the frame's bytes, leaving the
    /// reader positioned at the next frame's magic.
    pub(super) struct MultiFrameZstd {
        dec: Option<ZstdDec>,
    }

    impl Read for MultiFrameZstd {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Guard the empty-buffer case: a decoder returning 0 is our "frame drained" signal, but an
            // empty `buf` also yields 0, without this we'd spuriously advance/consume frames.
            if buf.is_empty() {
                return Ok(0);
            }
            loop {
                match self.dec.as_mut() {
                    Some(d) => {
                        let n = d.read(buf)?;
                        if n > 0 {
                            return Ok(n);
                        }
                    }
                    None => return Ok(0),
                }
                // Current frame drained → advance to the next data frame (skipping skippable), or EOF.
                let src = self.dec.take().unwrap().into_inner();
                self.dec = next_zstd_frame(src)?;
            }
        }
    }
} // mod ruzstd_frames

/// Wrap a byte source in the decoder for a whole-stream codec, returning a **`Send`** reader; used
/// by the tar/raw sequential paths, which stream the decoded bytes (and, for tar, hand the reader to
/// a worker thread). All backends are pure-Rust, and each decodes **every** concatenated member/
/// stream/frame (not just the first) so `cat`-joined and parallel-tool outputs never truncate.
pub fn decode_stream(
    codec: StreamCodec,
    inner: Box<dyn Read + Send>,
) -> Result<Box<dyn Read + Send>> {
    Ok(match codec {
        StreamCodec::None => inner,
        // MultiGzDecoder (not GzDecoder) so concatenated gzip *members* are all decoded, a gzip
        // file is a series of members (RFC 1952), and `cat a.gz b.gz`, bgzip/BGZF, and rotated
        // logs all produce them; the single-member decoder silently stops after the first.
        StreamCodec::Gzip => Box::new(flate2::read::MultiGzDecoder::new(inner)),
        // allow_multiple_streams=true so concatenated .xz *streams* are all decoded (`cat a.xz
        // b.xz`, which `xz -d` handles); false silently truncates to the first stream.
        StreamCodec::Xz => Box::new(lzma_rust2::XzReader::new(inner, true)),
        // libzstd walks concatenated frames and skips skippable ones itself, which is exactly what
        // `MultiFrameZstd` below exists to do for ruzstd. It is also several times faster: decoding
        // the kernel tree from a `.tar.zst` took 16.18 s through ruzstd against GNU tar's 2.12 s.
        #[cfg(feature = "zstd-c")]
        StreamCodec::Zstd => Box::new(
            zstd::stream::read::Decoder::new(inner)
                .map_err(|e| ArchiveError::Backend(format!("zstd: {e}")))?,
        ),
        // Walks concatenated zstd *frames* (`cat`, pzstd), not just the first, and steps over
        // skippable ones.
        #[cfg(not(feature = "zstd-c"))]
        StreamCodec::Zstd => Box::new(
            ruzstd_frames::open(inner).map_err(|e| ArchiveError::Backend(format!("zstd: {e}")))?,
        ),
        // MultiBzDecoder (not BzDecoder) so concatenated bzip2 *streams* (pbzip2/lbzip2, Wikipedia
        // multistream dumps) are all decoded, not just the first.
        StreamCodec::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(inner)),
        // lz4_flex's FrameDecoder already advances through concatenated frames; brotli has no
        // multi-stream concept.
        StreamCodec::Lz4 => Box::new(lz4_flex::frame::FrameDecoder::new(inner)),
        StreamCodec::Brotli => Box::new(brotli::Decompressor::new(inner, 4096)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// A payload with a compressible run plus varied bytes (so codecs actually do work).
    fn payload() -> Vec<u8> {
        let mut v = b"cram niche codec round-trip payload ".repeat(300);
        v.extend((0..5_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 15) as u8));
        v
    }

    /// Decode `compressed` through the live `decode_stream` path and assert it equals `expected`.
    fn assert_decode(codec: StreamCodec, compressed: Vec<u8>, expected: &[u8]) {
        let mut out = Vec::new();
        decode_stream(codec, Box::new(Cursor::new(compressed)))
            .expect("decoder builds")
            .read_to_end(&mut out)
            .expect("decode");
        assert_eq!(out, expected, "{codec:?} round-trip mismatch");
    }

    #[test]
    fn bzip2_stream_round_trips() {
        let p = payload();
        let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
        enc.write_all(&p).unwrap();
        assert_decode(StreamCodec::Bzip2, enc.finish().unwrap(), &p);
    }

    #[test]
    fn zstd_stream_round_trips() {
        let p = payload();
        let compressed =
            ruzstd::encoding::compress_to_vec(&p[..], ruzstd::encoding::CompressionLevel::Fastest);
        assert_decode(StreamCodec::Zstd, compressed, &p);
    }

    #[test]
    fn lz4_stream_round_trips() {
        let p = payload();
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        enc.write_all(&p).unwrap();
        assert_decode(StreamCodec::Lz4, enc.finish().unwrap(), &p);
    }

    #[test]
    fn brotli_stream_round_trips() {
        let p = payload();
        let mut enc = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        enc.write_all(&p).unwrap();
        assert_decode(StreamCodec::Brotli, enc.into_inner(), &p);
    }

    #[test]
    fn bzip2_multistream_decodes_all_members() {
        // pbzip2/lbzip2 + Wikipedia dumps concatenate independent bzip2 streams, all must decode.
        let bz = |d: &[u8]| {
            let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
            e.write_all(d).unwrap();
            e.finish().unwrap()
        };
        let (a, b) = (
            b"first bzip2 member ".repeat(80),
            b"second bzip2 member ".repeat(80),
        );
        let mut cat = bz(&a);
        cat.extend(bz(&b));
        let mut expected = a.clone();
        expected.extend_from_slice(&b);
        assert_decode(StreamCodec::Bzip2, cat, &expected);
    }

    #[test]
    fn zstd_multiframe_decodes_all_frames() {
        // `cat a.zst b.zst` / pzstd produce concatenated frames, all must decode.
        use ruzstd::encoding::{compress_to_vec, CompressionLevel};
        let (a, b) = (
            b"first zstd frame ".repeat(80),
            b"second zstd frame ".repeat(80),
        );
        let mut cat = compress_to_vec(&a[..], CompressionLevel::Fastest);
        cat.extend(compress_to_vec(&b[..], CompressionLevel::Fastest));
        let mut expected = a.clone();
        expected.extend_from_slice(&b);
        assert_decode(StreamCodec::Zstd, cat, &expected);
    }

    #[test]
    fn zstd_truncated_skippable_frame_errors_not_silent_eof() {
        // A stream that ends INSIDE a skippable frame is truncated: decoding must fail, not stop
        // early with only the data decoded so far. Guards against: an unchecked `io::copy` over the
        // skippable payload, which returns Ok on a short source and so reports a truncated stream as
        // a clean success.
        use ruzstd::encoding::{compress_to_vec, CompressionLevel};
        let a = b"data before the truncation ".repeat(40);
        let mut stream = compress_to_vec(&a[..], CompressionLevel::Fastest);
        stream.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
        stream.extend_from_slice(&100u32.to_le_bytes()); // claims 100 payload bytes...
        stream.extend_from_slice(b"only5"); // ...but the stream ends after 5

        let mut out = Vec::new();
        let err = decode_stream(StreamCodec::Zstd, Box::new(Cursor::new(stream)))
            .expect("decoder builds")
            .read_to_end(&mut out)
            .expect_err("a truncated skippable frame must surface as an error");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn zstd_skips_skippable_frames() {
        // Spec-valid skippable frames (magic 0x184D2A50..=0x184D2A5F) may lead or interleave data
        // frames; `zstd -d` skips them. Both a LEADING skippable frame (which `ruzstd`'s own
        // `StreamingDecoder::new` rejects as a hard error) and an INTERLEAVED one (which a decoder
        // that stops at an unrecognized magic reads as EOF, dropping every data frame that follows)
        // must decode.
        use ruzstd::encoding::{compress_to_vec, CompressionLevel};
        let skippable = |payload: &[u8]| {
            let mut f = Vec::new();
            f.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
            f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            f.extend_from_slice(payload);
            f
        };
        let (a, b) = (
            b"first data frame ".repeat(80),
            b"second data frame ".repeat(80),
        );
        // [skippable][data A][skippable][data B]
        let mut stream = skippable(b"leading-sidecar");
        stream.extend(compress_to_vec(&a[..], CompressionLevel::Fastest));
        stream.extend(skippable(b"interleaved-metadata"));
        stream.extend(compress_to_vec(&b[..], CompressionLevel::Fastest));

        let mut expected = a.clone();
        expected.extend_from_slice(&b);
        assert_decode(StreamCodec::Zstd, stream, &expected);
    }
}
