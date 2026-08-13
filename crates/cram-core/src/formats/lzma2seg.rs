//! Cutting an LZMA2 stream into independently-decodable segments.
//!
//! 7-Zip's default writes an entire archive as ONE solid folder, so block-level parallelism has
//! nothing to fan out over and cram decoded a 2.8 GB folder on one thread. But 7-Zip's *multi-
//! threaded* encoder resets the LZMA2 dictionary at each thread-block boundary, and a chunk with a
//! dictionary reset can be decoded cold — nothing before it is needed. Those resets are the seams
//! along which one folder splits into work.
//!
//! Measured on the benchmark corpus: 47,011 chunks, 21 of them dictionary resets, giving segments
//! of 110.9-128.0 MiB. Decoding those 21 concurrently took 1.61 s against 17.37 s for the same
//! stream on one thread, byte-identical.
//!
//! **This is a property of the archive, not of the format.** An archive written single-threaded, or
//! one smaller than a single thread-block, has exactly one segment and gains nothing. The walk says
//! which it is; nothing assumes.
//!
//! `lzma-rust2` ships an `Lzma2ReaderMt` that finds the same seams and does not exploit them: it
//! refills its work queue only when empty and spawns a worker only when the queue is non-empty, so
//! at most one unit decodes at a time. Measured at 1, 4, 12 and 24 threads it gave 1.01 effective
//! cores every time, 28% slower than single-threaded and 528 MB heavier. Hence cram cutting the
//! seams itself and feeding them to the scheduler it already has.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// Refuse to walk a stream claiming more chunks than this. A chunk carries at most 2 MiB, so this
/// covers a 4 TiB folder and bounds the work a crafted header can ask for.
const MAX_CHUNKS: usize = 4 << 20;

/// One run of LZMA2 chunks that can be decoded without anything before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    /// Absolute file offset of the segment's first chunk header.
    pub comp_off: u64,
    /// Uncompressed offset of this segment within the folder's decoded stream.
    pub unpacked_start: u64,
    /// Uncompressed bytes this segment produces.
    pub unpacked: u64,
}

/// Walk the chunk framing of the LZMA2 stream at `[off, off + len)`, cutting at every dictionary
/// reset.
///
/// Headers only: the packed payload of each chunk is seeked over, never read, so this costs two
/// syscalls per chunk and no memory whatever the archive's size. Returns `None` for anything that
/// does not parse as LZMA2 framing, which is the honest answer for a stream this has no business
/// splitting — the caller then decodes it whole, exactly as before.
pub(crate) fn walk(file: &mut File, off: u64, len: u64) -> io::Result<Option<Vec<Segment>>> {
    let mut segs: Vec<Segment> = Vec::new();
    let mut pos = off;
    let end = off.saturating_add(len);
    let mut unpacked_total = 0u64;
    let mut hdr = [0u8; 5];

    for _ in 0..MAX_CHUNKS {
        if pos >= end {
            // Ran to the end of the pack stream without an end marker. Well-formed streams have
            // one; treat its absence as "not something to split" rather than guessing.
            return Ok(None);
        }
        file.seek(SeekFrom::Start(pos))?;
        if file.read(&mut hdr[..1])? == 0 {
            return Ok(None);
        }
        let control = hdr[0];
        if control == 0x00 {
            // End of stream. A stream with one segment is not worth reporting as splittable.
            return Ok(if segs.len() > 1 { Some(segs) } else { None });
        }

        // Control >= 0xE0 is an LZMA chunk resetting state, properties AND dictionary; 0x01 is an
        // uncompressed chunk with a dictionary reset. Either can start a segment.
        if control >= 0xE0 || control == 0x01 {
            segs.push(Segment {
                comp_off: pos,
                unpacked_start: unpacked_total,
                unpacked: 0,
            });
        } else if segs.is_empty() {
            // The stream does not open on a dictionary reset, so it is not LZMA2 framing (or not
            // one we understand). Refuse rather than mis-cut it.
            return Ok(None);
        }

        let (hlen, packed, unpacked) = if control >= 0x80 {
            file.read_exact(&mut hdr[1..5])?;
            let un =
                ((((control & 0x1F) as u64) << 16) | ((hdr[1] as u64) << 8) | hdr[2] as u64) + 1;
            let pk = (((hdr[3] as u64) << 8) | hdr[4] as u64) + 1;
            // Reset mode 2 and 3 carry an LZMA properties byte after the two size fields.
            (if control >= 0xC0 { 6u64 } else { 5 }, pk, un)
        } else if control == 0x01 || control == 0x02 {
            file.read_exact(&mut hdr[1..3])?;
            let n = (((hdr[1] as u64) << 8) | hdr[2] as u64) + 1;
            (3u64, n, n)
        } else {
            // 0x03..0x7F is not a valid control byte.
            return Ok(None);
        };

        let seg = segs.last_mut().expect("a segment was opened above");
        seg.unpacked += unpacked;
        unpacked_total += unpacked;
        pos = pos.saturating_add(hlen).saturating_add(packed);
    }
    Ok(None)
}

/// The dictionary window a decoder needs to serve `seg`, having also to read `spill` bytes past its
/// end for an entry that straddles the boundary.
///
/// The archive's declared dictionary size would be the exact answer, but `sevenz-rust2` keeps coder
/// properties private, so it is not reachable from here. The segment's own length is an upper bound
/// that is always safe: the segment begins at a dictionary reset, so no match inside it can reach
/// further back than its start. On 7-Zip's output the real dictionary is a quarter of the thread
/// block, so this over-allocates about 4x — worth recovering if a `Coder::properties()` accessor
/// ever lands upstream, and correct in the meantime.
///
/// `spill` is covered because reading past the boundary crosses another reset, after which match
/// distances are bounded by the bytes produced since it.
pub(crate) fn dict_window(seg: &Segment, spill: u64) -> u32 {
    seg.unpacked.max(spill).min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a synthetic LZMA2 chunk header. Not decodable — the walk reads framing only, never
    /// payload, which is exactly the property being pinned here.
    fn chunk(out: &mut Vec<u8>, control: u8, unpacked: u32, packed: u16) {
        if control >= 0x80 {
            let u = unpacked - 1;
            out.push(control | ((u >> 16) as u8 & 0x1F));
            out.push((u >> 8) as u8);
            out.push(u as u8);
            out.push((packed.wrapping_sub(1) >> 8) as u8);
            out.push(packed.wrapping_sub(1) as u8);
            if control >= 0xC0 {
                out.push(0x5D); // properties byte
            }
        } else {
            let n = packed.wrapping_sub(1);
            out.push(control);
            out.push((n >> 8) as u8);
            out.push(n as u8);
        }
        out.extend(std::iter::repeat_n(0u8, packed as usize));
    }

    fn scratch(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cram-lzma2seg-{}-{tag}", std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(body).unwrap();
        p
    }

    #[test]
    fn cuts_at_every_dictionary_reset_and_nowhere_else() {
        let mut b = Vec::new();
        chunk(&mut b, 0xE0, 1000, 400); // reset: opens segment 0
        chunk(&mut b, 0x80, 2000, 500); // continues it
        chunk(&mut b, 0xA0, 3000, 600); // state reset only: still segment 0
        chunk(&mut b, 0xE0, 4000, 700); // reset: opens segment 1
        chunk(&mut b, 0x80, 5000, 800); // continues it
        b.push(0x00);
        let p = scratch("cuts", &b);
        let mut f = File::open(&p).unwrap();
        let segs = walk(&mut f, 0, b.len() as u64).unwrap().unwrap();

        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].unpacked_start, 0);
        assert_eq!(segs[0].unpacked, 1000 + 2000 + 3000);
        assert_eq!(segs[1].unpacked_start, 6000);
        assert_eq!(segs[1].unpacked, 4000 + 5000);
        // 0xE0 carries a properties byte (6-byte header); 0x80 and 0xA0 do not (5 bytes).
        assert_eq!(segs[1].comp_off, (6 + 400) + (5 + 500) + (5 + 600));
        let _ = std::fs::remove_file(&p);
    }

    /// A single-threaded 7-Zip archive resets the dictionary once, at the start. There is nothing to
    /// split and saying so is the whole point — the caller must not be handed a one-item fan-out.
    #[test]
    fn one_segment_is_reported_as_not_splittable() {
        let mut b = Vec::new();
        chunk(&mut b, 0xE0, 1000, 400);
        chunk(&mut b, 0x80, 2000, 500);
        b.push(0x00);
        let p = scratch("single", &b);
        let mut f = File::open(&p).unwrap();
        assert!(walk(&mut f, 0, b.len() as u64).unwrap().is_none());
        let _ = std::fs::remove_file(&p);
    }

    /// Anything that is not LZMA2 framing must be refused, not mis-cut. A wrong cut would hand a
    /// worker a byte range that decodes to plausible garbage.
    #[test]
    fn refuses_what_is_not_lzma2_framing() {
        for body in [
            vec![0x42, 0x00, 0x01, 0x02],       // invalid control byte
            vec![0x80, 0x00, 0x10, 0x00, 0x10], // opens without a dictionary reset
            vec![0xE0, 0x00, 0x10, 0x00],       // truncated mid-header
        ] {
            let p = scratch("bad", &body);
            let mut f = File::open(&p).unwrap();
            let got = walk(&mut f, 0, body.len() as u64);
            assert!(
                matches!(got, Ok(None) | Err(_)),
                "must refuse, got {got:?} for {body:?}"
            );
            let _ = std::fs::remove_file(&p);
        }
    }

    /// Running off the end of the pack stream without an end marker is refusal, not a partial split.
    #[test]
    fn refuses_a_stream_with_no_end_marker() {
        let mut b = Vec::new();
        chunk(&mut b, 0xE0, 1000, 400);
        chunk(&mut b, 0xE0, 1000, 400);
        let p = scratch("noend", &b);
        let mut f = File::open(&p).unwrap();
        assert!(walk(&mut f, 0, b.len() as u64).unwrap().is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_window_covers_the_segment_and_any_spill_past_it() {
        let s = Segment {
            comp_off: 0,
            unpacked_start: 0,
            unpacked: 1000,
        };
        assert_eq!(dict_window(&s, 0), 1000);
        assert_eq!(dict_window(&s, 4000), 4000);
    }
}
