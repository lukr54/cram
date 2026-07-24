//! ISO 9660 (`.iso`) reader, CD/DVD filesystem images. **Read-only.**
//!
//! ISO 9660 stores each file as one or more uncompressed **extents** at sector offsets, with a tree of
//! fixed-shape *directory records* describing the layout. There is no compression and no per-file
//! container framing, so the reader is small and the format is naturally **random-access** (a file is
//! `seek(extent × block_size); read(size)`), which also makes it mountable through the same
//! `RandomAccessReader` boundary as ZIP and `.cram`.
//!
//! Supported: base ISO 9660, the **Joliet** extension (UCS-2 long/Unicode names, preferred when
//! present), and **multi-extent files** (the `0x80` "not-final" flag): consecutive same-name records
//! are coalesced into a single logical entry whose bytes span every extent in order. Not interpreted:
//! Rock Ridge POSIX metadata (names fall back to their base ISO form).
//!
//! The reader is hardened against hostile images: every extent is bounds-checked against the file
//! length; directory recursion is depth-, cycle-, count-, and **cumulative-byte** guarded (so aliased
//! or overlapping directory extents can't drive read/parse amplification); the entry count is capped
//! *inside* the parse loop (no overshoot), and unsafe names are dropped, so pointing it at a crafted
//! `.iso` cannot escape the output dir, loop forever, or exhaust memory/IO.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream, RandomAccessReader};
use crate::secret::PasswordProvider;

/// The logical sector size the volume-descriptor set is addressed in (always 2048 for ISO 9660).
const SECTOR: u64 = 2048;
/// Volume descriptors begin at sector 16 (the first 16 sectors are the boot/system area).
const VD_START: u64 = 16 * SECTOR;
/// Caps that bound work on an untrusted image.
const MAX_DESCRIPTORS: usize = 64;
const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 500_000;
/// Ceiling on a single directory's extent we will buffer (real directories are tiny; this stops a
/// hostile record claiming a multi-GiB directory from forcing a huge allocation).
const MAX_DIR_BYTES: u64 = 64 * 1024 * 1024;
/// Floor for the cumulative directory-byte budget on tiny images (must be ≥ one `MAX_DIR_BYTES` read
/// so a single legitimately-large directory is never rejected). Doubles as the floor for the
/// path-byte budget.
const MIN_DIR_BUDGET: u64 = MAX_DIR_BYTES;
/// A single file's extents are coalesced across consecutive `0x80` records; cap the count so a hostile
/// run of continuation records can't grow one entry's segment list without bound. Real multi-extent
/// files have a handful (one extent per ~4 GiB), so this is very generous.
const MAX_FILE_SEGMENTS: usize = 4096;
const STREAM_CHUNK: usize = 1024 * 1024;

fn corrupt(m: &str) -> ArchiveError {
    ArchiveError::Corrupt(m.into())
}

/// Charge `add` bytes against a cumulative budget, erroring if it is exceeded. Used to bound the total
/// retained *path-string* bytes: a deep tree with long ancestor names duplicates a large prefix into
/// every descendant's path, so `entries × depth × name_len` memory can dwarf the image without a cap.
fn charge(used: &mut u64, budget: u64, add: usize) -> Result<()> {
    *used = used.saturating_add(add as u64);
    if *used > budget {
        return Err(corrupt("ISO path data exceeds image size (hostile tree)"));
    }
    Ok(())
}

/// A run of file extents: each `(absolute byte offset, byte length)`, in logical order.
type Segments = Vec<(u64, u64)>;

/// A multi-extent file being assembled from consecutive `0x80` directory records. `alive` goes false
/// once the run trips a cap (segment count / total size) or hits an invalid extent: further
/// continuation records with the same name are then swallowed and nothing is emitted, so a
/// corrupt/hostile run never yields a truncated, duplicate-path entry.
struct Pending {
    path: String,
    segments: Segments,
    size: u64,
    alive: bool,
}

/// A file's on-disk location as one or more consecutive extents. A normal file has exactly one
/// segment; an ISO multi-extent file (`0x80` flag) has several, read in order to reconstruct the whole
/// logical file. Directories carry an empty segment list.
#[derive(Clone, Default)]
struct Loc {
    /// The extent segments, in logical order.
    segments: Segments,
    /// Total logical size = Σ segment lengths.
    size: u64,
}

/// Which volume to read the tree from, and how to decode names.
struct Volume {
    block_size: u64,
    root_extent: u32,
    root_len: u32,
    joliet: bool,
}

pub struct IsoReader {
    path: PathBuf,
    file_len: u64,
    entries: Vec<Entry>,
    /// Per-entry location, aligned to `entries` (directories carry an empty `Loc`).
    locs: Vec<Loc>,
    cursor: usize,
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Read exactly `len` bytes at `off`, or a typed corruption error if the range is out of the file.
fn read_at(file: &mut File, off: u64, len: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

impl IsoReader {
    pub fn open(path: &Path, _pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < VD_START + SECTOR {
            return Err(corrupt("file too small to be an ISO 9660 image"));
        }

        let vol = read_volume(&mut file, file_len)?;
        let block = vol.block_size;

        // Walk the directory tree from the root, iteratively (explicit stack; no recursion, so a
        // deep/hostile tree can't overflow the stack). `seen` breaks extent cycles.
        let mut entries = Vec::new();
        let mut locs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![(vol.root_extent, vol.root_len, String::new(), 0usize)];
        seen.insert(vol.root_extent);

        // Cumulative anti-amplification guards: a valid image's total directory data is a small
        // fraction of the file, so cap cumulative directory bytes read at ~the image size (with a
        // floor for tiny images), and cap the number of directories descended. Distinct LBAs can alias
        // the SAME byte range, which `seen` alone does not catch; the byte budget does.
        let dir_budget = file_len.max(MIN_DIR_BUDGET);
        let mut dir_bytes_read: u64 = 0;
        let mut descended: usize = 0;
        // Bound total retained path-string bytes too (see `charge`): a valid image's paths are a small
        // fraction of its size, so the image size (with a floor) is a safe, generous ceiling.
        let path_budget = file_len.max(MIN_DIR_BUDGET);
        let mut path_bytes: u64 = 0;

        while let Some((extent, len, prefix, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                continue; // too deep, stop descending this branch
            }
            descended += 1;
            if descended > MAX_ENTRIES {
                return Err(corrupt("ISO has too many directories (hostile image)"));
            }
            let dir_off = (extent as u64)
                .checked_mul(block)
                .ok_or_else(|| corrupt("dir extent overflow"))?;
            let dir_len = len as u64;
            if dir_len == 0 || dir_len > MAX_DIR_BYTES || dir_off.saturating_add(dir_len) > file_len
            {
                // A directory whose extent runs past EOF (or is absurd) means the image is
                // truncated or corrupt. Silently skipping it would open the ISO as a SUCCESS with
                // a partial tree, extraction would then "complete" while missing entire branches.
                return Err(corrupt(
                    "ISO directory extent out of bounds (truncated or corrupt image)",
                ));
            }
            dir_bytes_read = dir_bytes_read.saturating_add(dir_len);
            if dir_bytes_read > dir_budget {
                return Err(corrupt(
                    "ISO directory data exceeds image size (aliased/hostile extents)",
                ));
            }
            let data = read_at(&mut file, dir_off, dir_len as usize)?;
            parse_dir(
                &data,
                block,
                file_len,
                vol.joliet,
                &prefix,
                depth,
                &mut seen,
                &mut stack,
                &mut entries,
                &mut locs,
                &mut path_bytes,
                path_budget,
            )?;
            if entries.len() > MAX_ENTRIES {
                return Err(corrupt("ISO directory tree too large"));
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            file_len,
            entries,
            locs,
            cursor: 0,
        })
    }
}

/// Read the volume-descriptor set at sector 16 and pick the volume to read: the Joliet supplementary
/// descriptor when present (better names), else the primary descriptor. A malformed Joliet descriptor
/// is ignored (fall back to primary) rather than failing an otherwise-valid image.
fn read_volume(file: &mut File, file_len: u64) -> Result<Volume> {
    let mut primary: Option<Volume> = None;
    let mut joliet: Option<Volume> = None;

    for i in 0..MAX_DESCRIPTORS {
        let off = VD_START + i as u64 * SECTOR;
        if off + SECTOR > file_len {
            break;
        }
        let vd = read_at(file, off, SECTOR as usize)?;
        // Every descriptor is tagged `CD001` at bytes 1..6; the standard identifier anchors the set.
        if &vd[1..6] != b"CD001" {
            if i == 0 {
                return Err(corrupt("not an ISO 9660 image (missing CD001)"));
            }
            break;
        }
        match vd[0] {
            255 => break, // terminator
            1 => primary = Some(parse_vol(&vd, false)?),
            2 => {
                // Supplementary descriptor: Joliet iff the escape sequences select UCS-2 (%/@,%/C,%/E).
                let esc = &vd[88..120];
                let is_joliet = esc.windows(3).any(|w| {
                    w == [0x25, 0x2f, 0x40] || w == [0x25, 0x2f, 0x43] || w == [0x25, 0x2f, 0x45]
                });
                if is_joliet {
                    // A broken Joliet descriptor must not sink a valid primary volume: keep the
                    // Joliet names only if they parse.
                    joliet = parse_vol(&vd, true).ok();
                }
            }
            _ => {} // boot record / partition, ignore
        }
    }

    joliet
        .or(primary)
        .ok_or_else(|| corrupt("ISO has no primary volume descriptor"))
}

/// Parse the block size + root directory record out of a primary/supplementary volume descriptor.
fn parse_vol(vd: &[u8], joliet: bool) -> Result<Volume> {
    let block_size = le_u16(&vd[128..130]) as u64;
    if !(512..=65536).contains(&block_size) || !block_size.is_power_of_two() {
        return Err(corrupt("invalid ISO logical block size"));
    }
    // Root directory record is a fixed 34-byte record at offset 156.
    let root = &vd[156..190];
    let root_extent = le_u32(&root[2..6]);
    let root_len = le_u32(&root[10..14]);
    Ok(Volume {
        block_size,
        root_extent,
        root_len,
        joliet,
    })
}

/// Sanitize `child_path` and, if safe, append a file entry spanning `segments` (total `size`). Enforces
/// the entry cap *inside* the parse loop so a single hostile directory can't overshoot it.
#[allow(clippy::too_many_arguments)]
fn emit_file(
    child_path: &str,
    segments: Segments,
    size: u64,
    entries: &mut Vec<Entry>,
    locs: &mut Vec<Loc>,
    path_bytes: &mut u64,
    path_budget: u64,
) -> Result<()> {
    if segments.is_empty() {
        return Ok(());
    }
    let Some(path) = EntryPath::from_raw(child_path) else {
        return Ok(()); // zip-slip / unsafe name → drop
    };
    if entries.len() >= MAX_ENTRIES {
        return Err(corrupt("ISO directory tree too large"));
    }
    charge(path_bytes, path_budget, path.raw().len())?;
    let index = entries.len();
    entries.push(Entry {
        index,
        path,
        kind: EntryKind::File,
        size,
        compressed_size: None,
        modified: None,
        unix_mode: None,
        crc32: None,
        encrypted: false,
    });
    locs.push(Loc { segments, size });
    Ok(())
}

/// Parse every record in one directory's extent bytes, pushing child directories to `stack` and file
/// entries to `entries`/`locs`. Records never span a logical block: a zero length byte means "skip to
/// the next block boundary". Consecutive same-name records with the `0x80` (not-final) flag form one
/// multi-extent file and are coalesced into a single entry.
#[allow(clippy::too_many_arguments)]
fn parse_dir(
    data: &[u8],
    block: u64,
    file_len: u64,
    joliet: bool,
    prefix: &str,
    depth: usize,
    seen: &mut std::collections::HashSet<u32>,
    stack: &mut Vec<(u32, u32, String, usize)>,
    entries: &mut Vec<Entry>,
    locs: &mut Vec<Loc>,
    path_bytes: &mut u64,
    path_budget: u64,
) -> Result<()> {
    // A multi-extent file in progress.
    let mut pending: Option<Pending> = None;

    let mut p = 0usize;
    while p < data.len() {
        let rec_len = data[p] as usize;
        if rec_len == 0 {
            // Advance to the next logical-block boundary; records don't cross a block. A multi-extent
            // run may straddle this padding, so `pending` is intentionally NOT flushed here.
            let next = ((p as u64 / block) + 1) * block;
            p = next as usize;
            continue;
        }
        // A record must fit within the buffer and be at least the fixed 33-byte prefix.
        if rec_len < 33 || p + rec_len > data.len() {
            break; // malformed → stop parsing this directory
        }
        let rec = &data[p..p + rec_len];
        let ear_len = rec[1] as u64; // Extended Attribute Record length, in logical blocks
        let ext_lba = le_u32(&rec[2..6]);
        let data_len = le_u32(&rec[10..14]);
        let flags = rec[25];
        let id_len = rec[32] as usize;
        if 33 + id_len > rec_len {
            break; // name runs past the record → malformed
        }
        let id = &rec[33..33 + id_len];
        p += rec_len;

        // Skip the "." (0x00) and ".." (0x01) self/parent records.
        if id_len == 1 && (id[0] == 0 || id[0] == 1) {
            continue;
        }
        let name = decode_name(id, joliet);
        if name.is_empty() {
            continue;
        }
        let is_dir = flags & 0x02 != 0;
        let child_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        // Flush a completed multi-extent run, emitting only if it stayed alive (not capped/corrupt).
        macro_rules! flush_pending {
            () => {
                if let Some(done) = pending.take() {
                    if done.alive {
                        emit_file(
                            &done.path,
                            done.segments,
                            done.size,
                            entries,
                            locs,
                            path_bytes,
                            path_budget,
                        )?;
                    }
                }
            };
        }

        if is_dir {
            // A directory record ends any multi-extent file run in progress.
            flush_pending!();
            // Descend once per unique extent (breaks cycles), within the depth and count caps.
            if depth < MAX_DEPTH && seen.len() < MAX_ENTRIES && seen.insert(ext_lba) {
                charge(path_bytes, path_budget, child_path.len())?;
                // Emit the directory itself as an entry (empty `Loc`), so listing shows the tree
                // and extraction recreates empty directories. Descending into a directory without
                // also emitting it leaves a directory that contains no files with no record of its
                // own, so an empty dir would vanish from the output.
                if let Some(path) = EntryPath::from_raw(&child_path) {
                    if entries.len() >= MAX_ENTRIES {
                        return Err(corrupt("ISO directory tree too large"));
                    }
                    let index = entries.len();
                    entries.push(Entry {
                        index,
                        path,
                        kind: EntryKind::Dir,
                        size: 0,
                        compressed_size: None,
                        modified: None,
                        unix_mode: None,
                        crc32: None,
                        encrypted: false,
                    });
                    locs.push(Loc::default());
                }
                stack.push((ext_lba, data_len, child_path, depth + 1));
            }
            continue;
        }

        // A file record. Its data starts AFTER any Extended Attribute Record (ear_len blocks). Compute
        // the extent's byte offset, or `None` if it overflows / runs past EOF.
        let start_block = (ext_lba as u64).saturating_add(ear_len);
        let off = start_block
            .checked_mul(block)
            .filter(|o| o.saturating_add(data_len as u64) <= file_len);
        let multi = flags & 0x80 != 0; // not the final record of this file

        // Is this a continuation of the current run (same logical name)?
        let continues = matches!(pending.as_ref(), Some(p) if p.path == child_path);
        if continues {
            let pend = pending.as_mut().unwrap();
            match off {
                Some(o) if pend.alive => {
                    pend.segments.push((o, data_len as u64));
                    pend.size = pend.size.saturating_add(data_len as u64);
                    // A file can't legitimately exceed the image, nor carry an unbounded segment list.
                    if pend.segments.len() > MAX_FILE_SEGMENTS || pend.size > file_len {
                        pend.alive = false;
                        pend.segments = Vec::new(); // hostile/corrupt → free memory, stop accumulating
                    }
                }
                _ => {
                    // An invalid extent mid-run makes the whole file corrupt: drop it rather than split
                    // it into a truncated, duplicate-path entry.
                    pend.alive = false;
                    pend.segments = Vec::new();
                }
            }
            if !multi {
                flush_pending!(); // terminating record → emit (only if still alive)
            }
            continue;
        }

        // Not a continuation: flush any prior run, then start (multi) or emit (single) this record.
        flush_pending!();
        let Some(o) = off else {
            continue; // a standalone invalid extent → drop
        };
        let seg = (o, data_len as u64);
        if multi {
            pending = Some(Pending {
                path: child_path,
                segments: vec![seg],
                size: data_len as u64,
                alive: true,
            });
        } else {
            emit_file(
                &child_path,
                vec![seg],
                data_len as u64,
                entries,
                locs,
                path_bytes,
                path_budget,
            )?;
        }
    }
    // A dangling multi-extent run (last record had 0x80 but no terminator) → emit best-effort if alive.
    if let Some(done) = pending.take() {
        if done.alive {
            emit_file(
                &done.path,
                done.segments,
                done.size,
                entries,
                locs,
                path_bytes,
                path_budget,
            )?;
        }
    }
    Ok(())
}

/// Decode a directory-record file identifier. Joliet names are UCS-2 big-endian; plain ISO names are
/// printable bytes. Strip the `;version` suffix either way; the trailing-`.` empty-extension marker is
/// an ISO level-1/2 convention only, so it is NOT stripped from a (real Unicode) Joliet name.
fn decode_name(id: &[u8], joliet: bool) -> String {
    let raw = if joliet {
        let mut s = String::with_capacity(id.len() / 2);
        let mut i = 0;
        while i + 1 < id.len() {
            let cp = u16::from_be_bytes([id[i], id[i + 1]]);
            s.push(char::from_u32(cp as u32).unwrap_or('\u{FFFD}'));
            i += 2;
        }
        s
    } else {
        // ISO level 1/2 identifiers are ASCII; decode leniently as latin-1 for odd media.
        id.iter().map(|&b| b as char).collect()
    };
    // Strip the ";1" version suffix (present on files in both ISO and Joliet).
    let name = raw.split(';').next().unwrap_or(&raw);
    if joliet {
        name.to_string()
    } else {
        // ISO adds a trailing '.' for an empty extension; drop it.
        name.strip_suffix('.').unwrap_or(name).to_string()
    }
}

/// Streams one file's bytes across its extent segments (in order) for the sequential path.
struct IsoBody {
    file: File,
    segments: Vec<(u64, u64)>,
    seg: usize,
    seg_pos: u64,
}
impl Read for IsoBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(&(off, len)) = self.segments.get(self.seg) else {
                return Ok(0); // all segments consumed
            };
            if self.seg_pos >= len {
                self.seg += 1;
                self.seg_pos = 0;
                continue;
            }
            self.file.seek(SeekFrom::Start(off + self.seg_pos))?;
            let want = ((len - self.seg_pos) as usize).min(buf.len());
            let n = self.file.read(&mut buf[..want])?;
            self.seg_pos += n as u64;
            return Ok(n);
        }
    }
}

impl IsoReader {
    fn body_for(&self, loc: &Loc) -> Result<IsoBody> {
        Ok(IsoBody {
            file: File::open(&self.path)?,
            segments: loc.segments.clone(),
            seg: 0,
            seg_pos: 0,
        })
    }
}

impl ArchiveReader for IsoReader {
    fn format(&self) -> Format {
        Format::iso()
    }

    fn entries(&self) -> Result<&[Entry]> {
        Ok(&self.entries)
    }

    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>> {
        if self.cursor >= self.entries.len() {
            return Ok(None);
        }
        let i = self.cursor;
        self.cursor += 1;
        let entry = self.entries[i].clone();
        let body: Box<dyn Read> = if entry.is_dir() {
            Box::new(io::empty())
        } else {
            Box::new(self.body_for(&self.locs[i])?)
        };
        Ok(Some(EntryStream {
            entry,
            body,
            meta_final: true,
        }))
    }

    fn as_random_access(&self) -> Option<&dyn RandomAccessReader> {
        Some(self)
    }
}

impl RandomAccessReader for IsoReader {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn copy_entry(&self, index: usize, out: &mut dyn Write) -> Result<u64> {
        let loc = self
            .locs
            .get(index)
            .ok_or_else(|| corrupt("bad entry index"))?;
        let mut body = self.body_for(loc)?;
        let mut buf = vec![0u8; STREAM_CHUNK];
        let mut written = 0u64;
        loop {
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            written += n as u64;
        }
        Ok(written)
    }

    fn read_range(&self, index: usize, off: u64, len: u64) -> Result<Vec<u8>> {
        let loc = self
            .locs
            .get(index)
            .ok_or_else(|| corrupt("bad entry index"))?;
        if off >= loc.size || len == 0 {
            return Ok(Vec::new());
        }
        let n = len.min(loc.size - off);
        let mut out = Vec::with_capacity(n as usize);
        let mut file = File::open(&self.path)?;
        let mut cur = 0u64; // logical offset at the start of the current segment
        let mut need = off; // next logical byte we still want
        let mut remaining = n;
        for &(seg_off, seg_len) in &loc.segments {
            if remaining == 0 {
                break;
            }
            let seg_end = cur + seg_len;
            if need < seg_end {
                let within = need - cur; // ≥ 0: `need` only ever advances forward
                let take = (seg_len - within).min(remaining);
                // Each segment was bounds-checked against file_len at open, so this range is valid.
                file.seek(SeekFrom::Start(seg_off + within))?;
                let mut buf = vec![0u8; take as usize];
                file.read_exact(&mut buf)?;
                out.extend_from_slice(&buf);
                remaining -= take;
                need += take;
            }
            cur = seg_end;
        }
        Ok(out)
    }
}

impl IsoReader {
    /// The on-disk image size (used by callers that reason about bounds; not part of the trait).
    #[allow(dead_code)]
    pub(crate) fn image_len(&self) -> u64 {
        self.file_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_name_strips_iso_version_suffix() {
        // Plain ISO 9660 identifiers carry a ";1" version suffix that must be stripped.
        assert_eq!(decode_name(b"README.TXT;1", false), "README.TXT");
        assert_eq!(decode_name(b"DATA.BIN;1", false), "DATA.BIN");
        // A trailing '.' (empty-extension marker) is dropped for ISO; a bare dir name is unchanged.
        assert_eq!(decode_name(b"SUBDIR", false), "SUBDIR");
        assert_eq!(decode_name(b"NOEXT.", false), "NOEXT");
    }

    #[test]
    fn decode_name_handles_joliet_ucs2be() {
        // Joliet names are UCS-2 big-endian. "Hi" = 0x0048 0x0069, then ";1" = 0x003B 0x0031.
        let joliet = [0x00, 0x48, 0x00, 0x69, 0x00, 0x3B, 0x00, 0x31];
        assert_eq!(decode_name(&joliet, true), "Hi");
        // A non-ASCII code point (é = U+00E9) round-trips through char::from_u32.
        let accent = [0x00, 0x65, 0x00, 0xE9]; // "eé"
        assert_eq!(decode_name(&accent, true), "eé");
    }

    #[test]
    fn decode_name_keeps_trailing_dot_for_joliet() {
        // The trailing-'.' strip is an ISO convention; a Joliet Unicode name keeps its real dot.
        // "a." = 0x0061 0x002E
        let dotted = [0x00, 0x61, 0x00, 0x2E];
        assert_eq!(decode_name(&dotted, true), "a.");
        // ...but the ISO form of the same bytes-as-ascii does strip it.
        assert_eq!(decode_name(b"a.", false), "a");
    }

    /// Build a minimal ISO 9660 directory record (both-endian fields, even-padded).
    fn make_record(name: &[u8], ext_lba: u32, data_len: u32, flags: u8) -> Vec<u8> {
        let id_len = name.len();
        let mut len = 33 + id_len;
        if len % 2 == 1 {
            len += 1; // records are padded to an even length
        }
        let mut r = vec![0u8; len];
        r[0] = len as u8;
        r[1] = 0; // no Extended Attribute Record
        r[2..6].copy_from_slice(&ext_lba.to_le_bytes());
        r[6..10].copy_from_slice(&ext_lba.to_be_bytes());
        r[10..14].copy_from_slice(&data_len.to_le_bytes());
        r[14..18].copy_from_slice(&data_len.to_be_bytes());
        r[25] = flags;
        r[32] = id_len as u8;
        r[33..33 + id_len].copy_from_slice(name);
        r
    }

    fn parse(data: &[u8]) -> (Vec<Entry>, Vec<Loc>) {
        let block = 2048u64;
        let file_len = 64 * 1024 * 1024u64; // large enough for our small extents
        let mut seen = std::collections::HashSet::new();
        let mut stack = Vec::new();
        let mut entries = Vec::new();
        let mut locs = Vec::new();
        let mut path_bytes = 0u64;
        parse_dir(
            data,
            block,
            file_len,
            false,
            "",
            0,
            &mut seen,
            &mut stack,
            &mut entries,
            &mut locs,
            &mut path_bytes,
            file_len,
        )
        .unwrap();
        (entries, locs)
    }

    #[test]
    fn single_extent_file_becomes_one_entry() {
        let data = make_record(b"A.TXT;1", 100, 5, 0x00);
        let (entries, locs) = parse(&data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.raw(), "A.TXT");
        assert_eq!(entries[0].size, 5);
        assert_eq!(locs[0].segments, vec![(100 * 2048, 5)]);
        assert_eq!(locs[0].size, 5);
    }

    #[test]
    fn multi_extent_file_is_coalesced_into_one_entry() {
        // Two consecutive same-name records: first flagged 0x80 (more to follow), second final.
        let mut data = make_record(b"BIG;1", 100, 2048, 0x80);
        data.extend_from_slice(&make_record(b"BIG;1", 200, 1000, 0x00));
        let (entries, locs) = parse(&data);
        // One logical entry, not two duplicates; size is the SUM; both extents are present in order.
        assert_eq!(
            entries.len(),
            1,
            "multi-extent must coalesce, not duplicate"
        );
        assert_eq!(entries[0].path.raw(), "BIG");
        assert_eq!(entries[0].size, 2048 + 1000);
        assert_eq!(
            locs[0].segments,
            vec![(100 * 2048, 2048), (200 * 2048, 1000)]
        );
        assert_eq!(locs[0].size, 3048);
    }

    #[test]
    fn directory_record_is_emitted_as_dir_and_pushed() {
        let data = make_record(b"SUB", 100, 34, 0x02); // 0x02 = directory
        let block = 2048u64;
        let file_len = 64 * 1024 * 1024u64;
        let mut seen = std::collections::HashSet::new();
        let mut stack = Vec::new();
        let mut entries = Vec::new();
        let mut locs = Vec::new();
        let mut path_bytes = 0u64;
        parse_dir(
            &data,
            block,
            file_len,
            false,
            "",
            0,
            &mut seen,
            &mut stack,
            &mut entries,
            &mut locs,
            &mut path_bytes,
            file_len,
        )
        .unwrap();
        // The directory IS an entry (kind Dir, empty Loc), so listing shows it and extraction
        // recreates empty dirs, and it is ALSO queued for traversal.
        assert_eq!(entries.len(), 1, "the directory is emitted as an entry");
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].path.raw(), "SUB");
        assert!(
            locs[0].segments.is_empty(),
            "a dir entry carries no extents"
        );
        assert_eq!(stack.len(), 1, "the directory is queued for traversal");
        assert_eq!(stack[0].0, 100);
    }

    #[test]
    fn extent_past_eof_is_dropped() {
        // ext_lba so large that off + data_len exceeds our file_len → the entry must be dropped.
        let data = make_record(b"BAD;1", 0x00FF_FFFF, 4096, 0x00);
        let (entries, _locs) = parse(&data);
        assert!(entries.is_empty(), "an extent past EOF is not trusted");
    }

    /// Run `parse_dir` with a caller-chosen `file_len` (to exercise the size cap on small images).
    fn parse_len(data: &[u8], file_len: u64) -> (Vec<Entry>, Vec<Loc>) {
        let mut seen = std::collections::HashSet::new();
        let mut stack = Vec::new();
        let mut entries = Vec::new();
        let mut locs = Vec::new();
        let mut path_bytes = 0u64;
        parse_dir(
            data,
            2048,
            file_len,
            false,
            "",
            0,
            &mut seen,
            &mut stack,
            &mut entries,
            &mut locs,
            &mut path_bytes,
            file_len,
        )
        .unwrap();
        (entries, locs)
    }

    #[test]
    fn multi_extent_run_with_invalid_interior_extent_is_dropped() {
        // Same-name run: valid piece (0x80), then an interior piece whose extent is past EOF (0x80),
        // then a valid terminating piece. The corrupt middle must poison the whole file so it is NOT
        // re-emitted as a truncated, duplicate-path entry.
        let mut data = make_record(b"BIG;1", 100, 2048, 0x80);
        data.extend_from_slice(&make_record(b"BIG;1", 0x00FF_FFFF, 4096, 0x80)); // past EOF
        data.extend_from_slice(&make_record(b"BIG;1", 200, 1000, 0x00));
        let (entries, _locs) = parse(&data);
        assert!(
            entries.is_empty(),
            "a multi-extent file with a corrupt interior extent is dropped, not split/duplicated"
        );
    }

    #[test]
    fn multi_extent_exceeding_image_size_is_dropped() {
        // Two aliased extents (same offset) whose coalesced size exceeds the image, the classic
        // amplification shape. Must be dropped rather than reported as a file larger than the image.
        let file_len = 307_200u64; // 300 KiB
        let mut data = make_record(b"BIG;1", 1, 160_000, 0x80);
        data.extend_from_slice(&make_record(b"BIG;1", 1, 160_000, 0x00)); // sum 320_000 > file_len
        let (entries, _locs) = parse_len(&data, file_len);
        assert!(
            entries.is_empty(),
            "a coalesced size larger than the image is refused (anti-amplification)"
        );
    }
}
