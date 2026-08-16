//! tar backend, a `.tar` (optionally wrapped in a whole-stream codec: `.tar.gz`, `.tar.xz`, …).
//!
//! tar is a pure front-to-back stream and the `tar` crate's entry iterator borrows its archive, so
//! it can't be stored across `next_entry` calls without self-reference. The clean, safe fix for a
//! one-pass source is a **worker thread**: it owns the archive+iterator entirely, reads each entry,
//! and hands `(metadata, bytes)` over a bounded channel; natural backpressure, no self-ref, no
//! unsafe. Listing (`entries`) uses a separate header-only pass so the file tree is known up front.
//!
//! Limitation (noted): a compressed tar is decoded twice for extraction, once for the metadata
//! pass, once by the worker. tar isn't the hot path (ZIP is); a single-pass optimization can come
//! later. Each entry's body is **streamed** over the channel in bounded chunks (never buffered whole
//!, a hostile header size / compression bomb would otherwise OOM the worker), with backpressure so
//! only ~1 chunk is in flight.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::codec::{decode_stream, multi};
use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream};
use crate::secret::PasswordProvider;

/// Bytes per streamed body chunk. Bounds the worker's in-flight buffer, so an entry with a huge
/// (untrusted) tar header size or a compression-bombed body is streamed to the destination rather
/// than buffered whole in RAM. Reused each read; only ~1 chunk is in flight (bounded channel).
const STREAM_CHUNK: usize = 1024 * 1024;

/// Messages the decode worker may run ahead by.
///
/// **It was 1**, described as "~1–2 entries in flight", and that is a ping-pong rather than a
/// pipeline: the producer fills one slot, blocks, the consumer takes it, and neither ever overlaps
/// the other. On a 94,778-entry archive that is ~95,000 round-trip handoffs, and it showed up as a
/// run at ~100% CPU whose two threads each accounted for only a fraction of the wall clock.
///
/// Still bounded, because the point of the bound is real — bodies stream, and an unbounded channel
/// would let the producer buffer a whole archive. The ceiling is `STREAM_DEPTH × STREAM_CHUNK`,
/// so 16 MiB.
const STREAM_DEPTH: usize = 16;

/// Read-ahead under the archive file itself. See [`open_decoded`] — without it a plain `.tar` costs
/// one `read` per 512-byte header.
const READ_BUF: usize = 256 * 1024;

/// Cap on the cumulative entry-metadata the header pass buffers into the listing `Vec`. A compressed
/// tar (`.tar.gz`/`.tar.xz`) can expand a few MB into a header stream describing tens of millions of
/// members; without a bound, `scan` grows the `Vec` until it OOMs before the caller sees a single
/// entry. 1 GiB of metadata covers any realistic listing (millions of files) while refusing the
/// hostile case. Charges each member's name length plus fixed `Entry`/`Vec` bookkeeping.
const MAX_SCAN_META: u64 = 1024 * 1024 * 1024;

/// Largest entry the worker will hand over in a single message.
///
/// A streamed file costs a `FileStart`, at least one `Chunk` and a `FileEnd` — three handoffs
/// whatever its size — and the kernel tree averages **20 KB an entry** across 100,992 of them. That
/// showed up as **591,555 `futex` calls, two thirds of the extraction's syscall time**, purely to
/// pass small buffers between two threads.
///
/// This is a ceiling on what is read before the decision, not a promise about the entry: a header
/// may declare any size it likes, and an entry that does not finish inside this many bytes falls
/// back to streaming with what has been read already as its first chunk. So a compression bomb is
/// bounded by this constant rather than by its own declaration, which is the property the streaming
/// path was protecting.
const WHOLE_MAX: usize = 256 * 1024;

/// One item streamed from the worker thread.
///
/// A large file is `FileStart` then N × `Chunk` then `FileEnd`, so its body never materializes as a
/// single Vec. A file that fits in [`WHOLE_MAX`] arrives as one `Whole` instead — same bytes, a
/// third of the handoffs. A directory is a lone `Dir`.
enum TarMsg {
    Dir(Entry),
    /// A small file, complete. See [`WHOLE_MAX`].
    Whole(Entry, Vec<u8>),
    FileStart(Entry),
    Chunk(Vec<u8>),
    FileEnd,
    Err(String),
}

/// The decoded byte stream, in parallel where the file allows it.
///
/// Both passes come through here — the header-only pass and the extraction pass — and both decode
/// every byte, so both are worth parallelising. `plan` is computed once by [`TarReader::open`] and
/// handed to each, because scanning for the seams costs a read of the file and doing it twice would
/// give a quarter of it back.
fn open_decoded(
    path: &Path,
    fmt: Format,
    plan: Option<&Arc<multi::Plan>>,
) -> Result<Box<dyn Read + Send>> {
    if let Some(p) = plan {
        return Ok(multi::open(p));
    }
    // Buffered, which matters most for the codec that does no buffering of its own: a plain `.tar`
    // handed the `tar` crate an unbuffered `File`, so every 512-byte header was its own `read`.
    // Extracting the kernel tree cost **526,810** reads for 2 GB. std's `BufReader` passes any
    // request at least as large as its capacity straight through, so an entry body still lands in
    // one read and this costs no extra copy on the bytes that matter.
    let file: Box<dyn Read + Send> =
        Box::new(BufReader::with_capacity(READ_BUF, File::open(path)?));
    decode_stream(fmt.codec, file)
}

fn cram_entry(raw: &str, is_dir: bool, size: u64, modified: Option<SystemTime>) -> Option<Entry> {
    EntryPath::from_raw(raw).map(|safe| Entry {
        index: 0,
        path: safe,
        kind: if is_dir {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size,
        compressed_size: None,
        modified,
        unix_mode: None,
        crc32: None,
        encrypted: false,
    })
}

/// A tar header's mtime as a [`SystemTime`]. A `0` mtime is tar's convention for "no timestamp" (it's
/// also what our own writer emits for timestamp-less members), so it maps to `None`; extraction then
/// leaves the file's current time rather than stamping it 1970.
///
/// The seconds field is attacker-controlled; `UNIX_EPOCH + Duration` panics on overflow, so a value
/// beyond a sane ceiling (~year 3000) is treated as "no timestamp" rather than allowed to crash the
/// reader on a crafted header.
fn header_mtime(header: &tar::Header) -> Option<SystemTime> {
    const MAX_SANE_UNIX: u64 = 32_503_680_000; // ~year 3000
    match header.mtime() {
        Ok(0) | Err(_) => None,
        Ok(secs) if secs > MAX_SANE_UNIX => None,
        Ok(secs) => Some(UNIX_EPOCH + Duration::from_secs(secs)),
    }
}

/// Is this a member Cram can materialize on disk? Regular (and contiguous) files and directories
/// only. Links (hard/sym), device nodes, FIFOs and GNU sparse members carry no extractable byte
/// stream, writing them as plain files would silently produce empty/garbage stand-ins, so they are
/// excluded from BOTH the listing and the extraction pass (the two stay consistent).
fn materializable(et: tar::EntryType) -> bool {
    et.is_dir() || matches!(et, tar::EntryType::Regular | tar::EntryType::Continuous)
}

/// Header-only pass → the entry list (data is skipped by the iterator). Also validates the archive.
fn scan(path: &Path, fmt: Format, plan: Option<&Arc<multi::Plan>>) -> Result<Vec<Entry>> {
    scan_with_cap(path, fmt, plan, MAX_SCAN_META)
}

/// [`scan`] with an explicit metadata budget (see [`MAX_SCAN_META`]); split out so the bound is
/// testable without synthesizing a multi-GB archive.
fn scan_with_cap(
    path: &Path,
    fmt: Format,
    plan: Option<&Arc<multi::Plan>>,
    cap: u64,
) -> Result<Vec<Entry>> {
    const PER_ENTRY_OVERHEAD: u64 = 256;
    let mut archive = tar::Archive::new(open_decoded(path, fmt, plan)?);
    let mut out = Vec::new();
    let mut meta: u64 = 0;
    for item in archive.entries()? {
        let entry = item?;
        if !materializable(entry.header().entry_type()) {
            continue; // link/special member, not listed, not extracted
        }
        let raw = entry.path()?.to_string_lossy().into_owned();
        // Charge this member against the budget BEFORE retaining it, so a header stream describing
        // millions of entries is refused rather than buffered into an unbounded `Vec`.
        meta = meta
            .saturating_add(raw.len() as u64)
            .saturating_add(PER_ENTRY_OVERHEAD);
        if meta > cap {
            return Err(ArchiveError::Backend(format!(
                "tar lists more than {} MiB of entry metadata, too large to buffer; extract it instead",
                cap / (1024 * 1024)
            )));
        }
        let is_dir = entry.header().entry_type().is_dir();
        let mtime = header_mtime(entry.header());
        if let Some(e) = cram_entry(&raw, is_dir, entry.size(), mtime) {
            out.push(e);
        }
        // Not reading the body → the iterator skips to the next header.
    }
    Ok(out)
}

/// The extraction pass: iterate the archive and stream each entry's body over `tx` in bounded chunks.
fn worker(reader: Box<dyn Read + Send>, tx: SyncSender<TarMsg>) {
    let mut archive = tar::Archive::new(reader);
    // **One buffer for the whole archive.** This was `vec![0u8; STREAM_CHUNK]` inside the per-entry
    // loop, so a 94,778-entry kernel tree allocated — and zeroed — 94,778 buffers of a megabyte each
    // to carry files averaging 20 KB. About 94 GB of `memset`, which is what a profile of `cram t`
    // was showing as 44.75% in `__memset_avx2_unaligned_erms` under mimalloc's `alloc_zeroed`.
    let mut buf = vec![0u8; STREAM_CHUNK];
    let entries = match archive.entries() {
        Ok(e) => e,
        Err(e) => {
            let _ = tx.send(TarMsg::Err(e.to_string()));
            return;
        }
    };
    for item in entries {
        let mut entry = match item {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(TarMsg::Err(e.to_string()));
                return;
            }
        };
        if !materializable(entry.header().entry_type()) {
            continue; // link/special member, mirror `scan`: neither listed nor written
        }
        let raw = match entry.path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        let is_dir = entry.header().entry_type().is_dir();
        let mtime = header_mtime(entry.header());
        let Some(cram) = cram_entry(&raw, is_dir, entry.size(), mtime) else {
            continue; // zip-slip name → drop
        };
        if is_dir {
            if tx.send(TarMsg::Dir(cram)).is_err() {
                return; // consumer dropped → stop
            }
            continue;
        }
        // Read up to [`WHOLE_MAX`] first. Most entries end inside it and go over as one message
        // rather than three; anything longer carries on below with these bytes as its first chunk.
        let mut head = 0usize;
        loop {
            match entry.read(&mut buf[head..WHOLE_MAX]) {
                Ok(0) => break,
                Ok(n) => head += n,
                Err(e) => {
                    let _ = tx.send(TarMsg::Err(e.to_string()));
                    return;
                }
            }
            if head == WHOLE_MAX {
                break;
            }
        }
        if head < WHOLE_MAX {
            // Short read with room to spare = the entry ended.
            if tx.send(TarMsg::Whole(cram, buf[..head].to_vec())).is_err() {
                return;
            }
            continue;
        }
        if tx.send(TarMsg::FileStart(cram)).is_err() {
            return;
        }
        if tx.send(TarMsg::Chunk(buf[..head].to_vec())).is_err() {
            return;
        }
        // Stream the rest in bounded chunks, the entry reader is capped to the (untrusted) header
        // size, so buffering it whole would let a crafted size / bomb OOM the process.
        loop {
            match entry.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(TarMsg::Chunk(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(TarMsg::Err(e.to_string()));
                    return;
                }
            }
        }
        if tx.send(TarMsg::FileEnd).is_err() {
            return;
        }
    }
    // tx dropped here → the channel closes → the consumer's recv() returns Err → end of stream.
}

/// Streams one file entry's body from the worker channel, one chunk at a time. On drop it drains any
/// unread chunks up to `FileEnd`, so an entry the engine abandons early (e.g. on a write error, where
/// the sequential path does *not* drain) still leaves the channel aligned to the next entry, the
/// "drain before the next `next_entry`" invariant stays local to this backend.
struct TarBody<'a> {
    rx: &'a Receiver<TarMsg>,
    cur: io::Cursor<Vec<u8>>,
    done: bool,
}

impl Read for TarBody<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.cur.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(TarMsg::Chunk(bytes)) => self.cur = io::Cursor::new(bytes),
                Ok(TarMsg::FileEnd) => {
                    self.done = true;
                    return Ok(0);
                }
                Ok(TarMsg::Err(e)) => {
                    self.done = true;
                    return Err(io::Error::other(e));
                }
                Ok(_) => {
                    self.done = true;
                    return Err(io::Error::other("tar stream desync"));
                }
                Err(_) => {
                    self.done = true;
                    return Err(io::Error::other("tar worker ended mid-entry"));
                }
            }
        }
    }
}

impl Drop for TarBody<'_> {
    fn drop(&mut self) {
        // Discard any remaining chunks up to the entry boundary so the next entry starts clean.
        if self.done {
            return;
        }
        loop {
            match self.rx.recv() {
                Ok(TarMsg::FileEnd) | Ok(TarMsg::Err(_)) | Err(_) => break,
                Ok(_) => {} // leftover chunk → drop
            }
        }
    }
}

/// A tar archive. `entries` come from the header pass; extraction streams from a worker thread.
pub struct TarReader {
    path: PathBuf,
    fmt: Format,
    /// Built on demand. For a **compressed** tar this costs a full decode — the headers are
    /// interleaved with the bodies, so there is no way to read them without decompressing
    /// everything — and an extraction that then streams every entry would pay for the archive
    /// twice. See [`ArchiveReader::entries_are_cheap`].
    listing: std::cell::OnceCell<Vec<Entry>>,
    rx: Option<Receiver<TarMsg>>,
    started: bool,
    /// Where the archive's independent streams begin, when it has more than one. `None` means the
    /// sequential decoder: a codec written as a single stream, a foreign archive, or the override.
    plan: Option<Arc<multi::Plan>>,
}

impl TarReader {
    pub fn open(path: &Path, fmt: Format, _pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        let plan = multi::plan(path, fmt.codec);
        Ok(Self {
            path: path.to_path_buf(),
            fmt,
            listing: std::cell::OnceCell::new(),
            rx: None,
            started: false,
            plan,
        })
    }

    /// Spawn the extraction worker on first use.
    fn ensure_started(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let (tx, rx) = sync_channel::<TarMsg>(STREAM_DEPTH);
        let reader = open_decoded(&self.path, self.fmt, self.plan.as_ref())?;
        thread::spawn(move || worker(reader, tx));
        self.rx = Some(rx);
        Ok(())
    }
}

impl ArchiveReader for TarReader {
    fn format(&self) -> Format {
        self.fmt
    }

    fn entries(&self) -> Result<&[Entry]> {
        if let Some(v) = self.listing.get() {
            return Ok(v);
        }
        let v = scan(&self.path, self.fmt, self.plan.as_ref())?;
        Ok(self.listing.get_or_init(|| v))
    }

    /// False for anything compressed: see the field doc on `listing`. A plain `.tar` still has to
    /// be walked, but walking one is reading its headers and seeking past the bodies, which is not
    /// the same order of cost as decompressing them.
    fn entries_are_cheap(&self) -> bool {
        self.fmt.codec == crate::format::Codec::None
    }

    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>> {
        self.ensure_started()?;
        let rx = self.rx.as_ref().unwrap();
        match rx.recv() {
            Ok(TarMsg::FileStart(entry)) => Ok(Some(EntryStream {
                entry,
                body: Box::new(TarBody {
                    rx,
                    cur: io::Cursor::new(Vec::new()),
                    done: false,
                }),
                meta_final: true,
            })),
            Ok(TarMsg::Whole(entry, bytes)) => Ok(Some(EntryStream {
                entry,
                body: Box::new(io::Cursor::new(bytes)),
                meta_final: true,
            })),
            Ok(TarMsg::Dir(entry)) => Ok(Some(EntryStream {
                entry,
                body: Box::new(io::empty()),
                meta_final: true,
            })),
            Ok(TarMsg::Err(e)) => Err(ArchiveError::Backend(e)),
            // A stray body message with no active entry means the stream is out of sync.
            Ok(TarMsg::Chunk(_)) | Ok(TarMsg::FileEnd) => {
                Err(ArchiveError::Backend("tar stream desync".into()))
            }
            Err(_) => Ok(None), // channel closed → done
        }
    }
}

#[cfg(test)]
mod link_entry_tests {
    use super::*;
    use crate::secret::NoPassword;

    /// A tar holding a regular file, a symlink and a hardlink must list and extract ONLY the
    /// regular file, link entries have no byte stream, and materializing them as empty files was
    /// silent data corruption (an "extracted" file with none of its content).
    #[test]
    fn link_entries_are_neither_listed_nor_written() {
        let mut builder = tar::Builder::new(Vec::new());

        let body = b"real file content";
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut h, "real.txt", &body[..]).unwrap();

        let mut sl = tar::Header::new_gnu();
        sl.set_size(0);
        sl.set_entry_type(tar::EntryType::Symlink);
        sl.set_link_name("real.txt").unwrap();
        builder
            .append_data(&mut sl, "sym.txt", io::empty())
            .unwrap();

        let mut hl = tar::Header::new_gnu();
        hl.set_size(0);
        hl.set_entry_type(tar::EntryType::Link);
        hl.set_link_name("real.txt").unwrap();
        builder
            .append_data(&mut hl, "hard.txt", io::empty())
            .unwrap();

        let bytes = builder.into_inner().unwrap();
        let dir = std::env::temp_dir().join(format!("cram-tar-links-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("links.tar");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = TarReader::open(
            &path,
            Format::tar(crate::format::Codec::None),
            Arc::new(NoPassword),
        )
        .unwrap();
        let names: Vec<_> = ArchiveReader::entries(&reader)
            .unwrap()
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        assert_eq!(names, vec!["real.txt"], "links must not be listed");

        // Extraction pass yields only the regular file, with its full body.
        let mut seen = Vec::new();
        while let Some(mut es) = reader.next_entry().unwrap() {
            let mut got = Vec::new();
            es.body.read_to_end(&mut got).unwrap();
            seen.push((es.entry.name().to_string(), got));
        }
        assert_eq!(seen.len(), 1, "links must not be extracted: {seen:?}");
        assert_eq!(seen[0].0, "real.txt");
        assert_eq!(seen[0].1, body);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod mtime_guard_tests {
    use super::*;

    #[test]
    fn header_mtime_rejects_hostile_and_zero_accepts_real() {
        let mut h = tar::Header::new_gnu();
        // tar's "no timestamp" convention.
        h.set_mtime(0);
        assert!(header_mtime(&h).is_none());
        // A crafted enormous mtime must NOT panic (`UNIX_EPOCH + Duration` overflows) → None.
        h.set_mtime(u64::MAX);
        assert!(header_mtime(&h).is_none());
        // A real 2020 timestamp survives.
        h.set_mtime(1_577_934_246);
        assert_eq!(
            header_mtime(&h),
            Some(UNIX_EPOCH + Duration::from_secs(1_577_934_246))
        );
    }
}

#[cfg(test)]
mod scan_cap_tests {
    use super::*;
    use crate::format::Codec;
    use crate::progress::NullSink;
    use crate::writer::CreateOptions;

    /// A header stream whose metadata exceeds the budget must be refused, not buffered whole.
    #[test]
    fn scan_refuses_metadata_flood_but_allows_normal_listing() {
        let dir = std::env::temp_dir().join(format!("cram-tarscan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d")).unwrap();
        for i in 0..64 {
            std::fs::write(dir.join(format!("d/f-{i:03}.txt")), b"x").unwrap();
        }
        let tar = dir.join("many.tar");
        crate::engine::create::create(
            &tar,
            Format::tar(Codec::None),
            &[dir.join("d")],
            CreateOptions::default(),
            &NullSink,
        )
        .unwrap();
        let fmt = Format::tar(Codec::None);
        // A tiny budget (smaller than 64 names + overhead) must trip the guard.
        assert!(
            scan_with_cap(&tar, fmt, None, 256).is_err(),
            "a metadata flood must be refused before the Vec grows unbounded"
        );
        // The production budget lists every member (64 files + their parent dir).
        assert!(scan_with_cap(&tar, fmt, None, MAX_SCAN_META).unwrap().len() >= 64);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
