//! 7z backend, **read-only** for now (create lands with the writer phase), via the pure-Rust
//! `sevenz-rust2` decoder (LZMA/LZMA2 always; BZip2/PPMd/Deflate/LZ4/AES-256 behind features).
//!
//! 7z is solid/blocked: entries in a block share one decode stream, so there's no cheap per-entry
//! random access → this is a sequential [`ArchiveReader`], routed to the sequential engine. The
//! crate's extraction API is a **push** callback (`for_each_entries(|entry, &mut Read|)`), the same
//! shape as tar, so the same fix applies: a **worker thread** owns the reader and pushes
//! `(metadata, bytes)` over a bounded channel; `next_entry` pulls. Listing (`entries`) is a cheap
//! header pass off `archive().files` (no block decode).
//!
//! Passwords: 7z uses ONE archive-wide password. If the *header* is encrypted we resolve it at
//! `open()` (needed even to list). If only *content* is encrypted (header plain, listing browsable),
//! metadata reads with an empty password and the worker resolves the password lazily on the first
//! block-decode failure, retrying from the start (safe: the failure precedes any emitted entry).

use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;

use sevenz_rust2::{ArchiveEntry, ArchiveReader as SzReader, Error as SzError, Password};

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::formats::lzma2seg;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream};
use crate::secret::{PasswordProvider, PasswordRequest, Secret};

/// Bytes per streamed body chunk, bounds the worker's in-flight buffer so a huge or compression-
/// bombed entry is streamed to the destination, never buffered whole in RAM (a crafted metadata size
/// would otherwise force an allocation that aborts the process). Reused each read.
const STREAM_CHUNK: usize = 1024 * 1024;

/// One item streamed from the worker thread. A file is `FileStart` then N × `Chunk` then `FileEnd`;
/// a directory is a lone `Dir`.
enum SzMsg {
    Dir(Entry),
    FileStart(Entry),
    Chunk(Vec<u8>),
    FileEnd,
    Err(ArchiveError),
}

fn is_password_error(e: &SzError) -> bool {
    matches!(e, SzError::PasswordRequired | SzError::MaybeBadPassword(_))
}

fn map_sevenz(e: SzError) -> ArchiveError {
    match e {
        SzError::PasswordRequired => ArchiveError::PasswordRequired,
        SzError::MaybeBadPassword(_) => ArchiveError::WrongPassword,
        SzError::BadSignature(_)
        | SzError::ChecksumVerificationFailed
        | SzError::NextHeaderCrcMismatch
        | SzError::BadTerminatedStreamsInfo(_)
        | SzError::BadTerminatedUnpackInfo
        | SzError::BadTerminatedPackInfo(_)
        | SzError::BadTerminatedSubStreamsInfo
        | SzError::BadTerminatedHeader(_)
        | SzError::FileNotFound => ArchiveError::Corrupt(e.to_string()),
        SzError::UnsupportedVersion { .. }
        | SzError::UnsupportedCompressionMethod(_)
        | SzError::ExternalUnsupported
        | SzError::Unsupported(_) => ArchiveError::Backend(format!("7z: {e}")),
        other => ArchiveError::Backend(format!("7z: {other}")),
    }
}

/// A 7z entry's last-modified NTFS FILETIME as a [`SystemTime`], or `None` when absent, zero, or out
/// of a sane range. The raw tick count is attacker-controlled and the crate's `NtTime -> SystemTime`
/// conversion panics on overflow, so we reject anything above ~year 9999 before converting.
fn seven_z_mtime(f: &ArchiveEntry) -> Option<std::time::SystemTime> {
    if !f.has_last_modified_date {
        return None;
    }
    // 100 ns ticks since 1601. ~year 9999 ≈ 2.65e18 ticks, far below `u64::MAX` (1.8e19) and within
    // the representable `SystemTime` range on Windows, so the `+` in the conversion cannot overflow.
    const MAX_SANE_TICKS: u64 = 2_650_000_000_000_000_000;
    let raw = u64::from(f.last_modified_date);
    if raw == 0 || raw >= MAX_SANE_TICKS {
        return None;
    }
    Some(std::time::SystemTime::from(f.last_modified_date))
}

/// Map a 7z metadata entry to a cram [`Entry`], funneling the name through the zip-slip guard
/// (returns `None` for an unsafe name → the caller drops it). `encrypted` is decided at the archive
/// level by [`archive_has_aes`] and applied to every file, since 7z carries no per-entry flag.
fn cram_entry(f: &ArchiveEntry, encrypted: bool) -> Option<Entry> {
    EntryPath::from_raw(&f.name).map(|safe| Entry {
        index: 0,
        path: safe,
        kind: if f.is_directory {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: f.size,
        compressed_size: None, // per-file compressed size is meaningless in a solid block
        // 7z stores an NTFS FILETIME (100 ns ticks since 1601) per entry; surface it so extract can
        // restore it. The value is attacker-controlled, and `NtTime -> SystemTime` adds a `Duration`
        // with a plain `+` that PANICS on overflow, so a crafted near-`u64::MAX` FILETIME could crash
        // the reader. Bound it to a sane range (0 < t < ~year 9999) before converting; anything else
        // is treated as "no timestamp" rather than trusted.
        modified: seven_z_mtime(f),
        unix_mode: None,
        crc32: f.has_crc.then_some(f.crc as u32),
        encrypted: encrypted && !f.is_directory, // container-level (7z has no per-entry flag)
    })
}

/// Whether any block's coder chain uses the AES-256-SHA-256 method (id `06 F1 07 01`). 7z never sets a
/// per-entry encryption flag, so content encryption (a `7z a -pPASS` archive, whose header lists fine
/// without a password) would otherwise be reported as "unprotected". Reading it off the header blocks
/// is the reliable signal, a header-encrypted (`-mhe`) archive never reaches here (open fails first
/// with a password error, handled upstream).
fn archive_has_aes(archive: &sevenz_rust2::Archive) -> bool {
    archive.blocks.iter().any(|b| {
        b.coders
            .iter()
            .any(|c| matches!(c.encoder_method_id(), [0x06, 0xF1, 0x07, 0x01]))
    })
}

/// Header-only pass → the entry list, off `archive().files` (no block is decoded). Also the point
/// where header encryption surfaces (open fails without the right password).
fn read_metadata(path: &Path, secret: &Secret) -> std::result::Result<Vec<Entry>, SzError> {
    let reader = SzReader::open(path, Password::new(secret.expose()))?;
    let aes = archive_has_aes(reader.archive());
    let mut out = Vec::new();
    for f in &reader.archive().files {
        if let Some(e) = cram_entry(f, aes) {
            out.push(e);
        }
    }
    Ok(out)
}

/// One extraction pass: decode every block, buffering each entry and pushing it over `tx`. Sets
/// `*sent_any` once anything has been emitted (so a caller can tell a pre-emit failure, safe to
/// retry with a new password, from a mid-stream one). Stops early (Ok) if the consumer drops.
fn extract_pass(
    path: &Path,
    secret: &Secret,
    tx: &SyncSender<SzMsg>,
    sent_any: &mut bool,
) -> std::result::Result<(), SzError> {
    let mut reader = SzReader::open(path, Password::new(secret.expose()))?;
    // Entries needing NO block decode (directories, empty files) are BUFFERED until the content
    // password is proven by the first successful non-empty read, then flushed in walk order. This
    // keeps `sent_any` false until a real decode succeeds, so even when such an entry precedes the
    // first encrypted file in the walk (common in `7z a -p` archives, whose header is plaintext and
    // whose folders are listed first), a content-password failure still satisfies the worker's
    // `!sent_any` retry gate. Emitting them eagerly would set `sent_any` before any block decode and
    // permanently defeat the lazy retry (a retry re-walks from scratch, so nothing may be emitted).
    let mut pending: Vec<SzMsg> = Vec::new();
    let mut proven = false;
    reader.for_each_entries(|entry, rd| {
        let Some(cram) = cram_entry(entry, false) else {
            // The extract stream never consults `encrypted`, so its value is irrelevant here.
            return Ok(true); // zip-slip name → drop, keep going
        };
        if entry.is_directory {
            let msg = SzMsg::Dir(cram);
            if proven {
                if tx.send(msg).is_err() {
                    return Ok(false); // consumer dropped → stop the walk cleanly
                }
            } else {
                pending.push(msg);
            }
            return Ok(true);
        }
        // Read the FIRST chunk before emitting anything for this file: a content-password failure
        // surfaces on the first block decode, and must happen while `sent_any` is still false. 7z
        // uses one archive-wide key, so once a block decodes there are no further password errors.
        // Streaming in bounded chunks (vs one whole-entry Vec) means a huge/bomb entry can't OOM.
        let mut buf = vec![0u8; STREAM_CHUNK];
        let mut n = rd.read(&mut buf)?;
        if n == 0 {
            // Empty file: no block decoded, so it can't prove the password; buffer it too (unless
            // the password is already proven, in which case emit it now).
            let (start, end) = (SzMsg::FileStart(cram), SzMsg::FileEnd);
            if proven {
                if tx.send(start).is_err() || tx.send(end).is_err() {
                    return Ok(false);
                }
            } else {
                pending.push(start);
                pending.push(end);
            }
            return Ok(true);
        }
        // A non-empty block decoded → the password is proven. Flush any buffered no-decode entries
        // (in walk order) ahead of this file, then emit immediately from here on.
        if !proven {
            proven = true;
            *sent_any = true; // about to emit → a retry-from-scratch is no longer safe
            for msg in pending.drain(..) {
                if tx.send(msg).is_err() {
                    return Ok(false);
                }
            }
        }
        if tx.send(SzMsg::FileStart(cram)).is_err() {
            return Ok(false);
        }
        while n > 0 {
            if tx.send(SzMsg::Chunk(buf[..n].to_vec())).is_err() {
                return Ok(false);
            }
            n = rd.read(&mut buf)?;
        }
        if tx.send(SzMsg::FileEnd).is_err() {
            return Ok(false);
        }
        Ok(true)
    })?;
    // Walk finished cleanly with entries still buffered → the archive had no non-empty file to prove
    // a password (all dirs / empty files), so there was nothing to decrypt: emit them now.
    for msg in pending {
        if tx.send(msg).is_err() {
            break;
        }
        *sent_any = true;
    }
    Ok(())
}

/// The worker: run [`extract_pass`], resolving a *content* password on the first pre-emit failure
/// (header-plain / content-encrypted archives) and retrying from the start. `secret` starts as the
/// password that read the header (empty when the header was plain, 7z uses one password archive-wide).
fn worker(
    path: PathBuf,
    name: String,
    mut secret: Secret,
    pw: Arc<dyn PasswordProvider>,
    tx: SyncSender<SzMsg>,
) {
    let mut attempt = 0u32;
    loop {
        let mut sent_any = false;
        match extract_pass(&path, &secret, &tx, &mut sent_any) {
            Ok(()) => return,
            // A password failure before any entry was emitted → content is encrypted and our
            // header password (possibly empty) is wrong. Ask the provider and retry from scratch.
            Err(e) if is_password_error(&e) && !sent_any => {
                match pw.password(&PasswordRequest {
                    archive: &name,
                    entry: None,
                    for_header: false,
                    attempt,
                }) {
                    Some(s) => {
                        secret = s;
                        attempt += 1;
                    }
                    None => {
                        let _ = tx.send(SzMsg::Err(map_sevenz(e)));
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(SzMsg::Err(map_sevenz(e)));
                return;
            }
        }
    }
}

/// Streams one file entry's body from the worker channel, one chunk at a time. On drop it drains any
/// unread chunks up to `FileEnd`, so an entry the engine abandons early (e.g. a write error, where
/// the sequential path does not drain) still leaves the channel aligned to the next entry, the
/// "drain before the next `next_entry`" invariant stays local to this backend.
struct SzBody<'a> {
    rx: &'a Receiver<SzMsg>,
    cur: io::Cursor<Vec<u8>>,
    done: bool,
}

impl Read for SzBody<'_> {
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
                Ok(SzMsg::Chunk(bytes)) => self.cur = io::Cursor::new(bytes),
                Ok(SzMsg::FileEnd) => {
                    self.done = true;
                    return Ok(0);
                }
                Ok(SzMsg::Err(e)) => {
                    self.done = true;
                    return Err(io::Error::other(e.to_string()));
                }
                Ok(_) => {
                    self.done = true;
                    return Err(io::Error::other("7z stream desync"));
                }
                Err(_) => {
                    self.done = true;
                    return Err(io::Error::other("7z worker ended mid-entry"));
                }
            }
        }
    }
}

impl Drop for SzBody<'_> {
    fn drop(&mut self) {
        // Discard any remaining chunks up to the entry boundary so the next entry starts clean.
        if self.done {
            return;
        }
        loop {
            match self.rx.recv() {
                Ok(SzMsg::FileEnd) | Ok(SzMsg::Err(_)) | Err(_) => break,
                Ok(_) => {} // leftover chunk → drop
            }
        }
    }
}

/// A 7z archive opened for sequential extraction. Metadata is read up front; bodies stream from a
/// worker thread on first use.
/// How much memory the per-thread block cache may use, as a fraction of installed RAM.
///
/// Installed, not available, for the same reason the writer's thread sizing is: an extraction whose
/// parallelism depends on what else is running cannot be reasoned about or benchmarked.
const BLOCK_CACHE_RAM_FRACTION: u64 = 4;

/// Distinguishes archives in the shared thread-local cache, so two concurrent extractions cannot
/// serve each other's blocks.
static RA_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One decoded block held by a worker: the archive it belongs to, its index, and each entry's bytes
/// paired with the archive file index that identifies it.
type CachedBlock = (u64, usize, Vec<(usize, Vec<u8>)>);

thread_local! {
    /// The one block this thread last decoded: `(archive id, block index, entries)`.
    ///
    /// One block, not an LRU. The engine clusters groups by [`RandomAccessReader::locality_key`] and
    /// hands each worker a contiguous run, so a worker walks a block to its end before moving on and
    /// a single slot hits nearly always. An LRU would multiply the memory by its depth to catch the
    /// occasional boundary miss.
    static BLOCK_CACHE: std::cell::RefCell<Option<CachedBlock>> =
        const { std::cell::RefCell::new(None) };
}

/// What a block costs to serve: how many entries share it, and whether its decoded form fits the
/// per-worker budget. Computed once at open; see [`SevenZRandomAccess::build`].
struct BlockPlan {
    /// Entries with a stream in this block. `1` means caching it buys nothing.
    entries: usize,
    /// Whether the decoded block fits the per-worker cache budget.
    fits: bool,
}

/// Random access over a 7z archive, by decoding whole blocks and serving entries from the last one.
///
/// 7z is solid: an entry cannot be decoded without decoding its block from the start, so serving
/// many entries from one block is only viable if the decoded block can be kept. That is fine for
/// archives cram writes (bounded solid blocks) and impossible for the general case — **7-Zip's own
/// default puts the entire archive in one block**, and caching that means holding the whole
/// uncompressed archive per worker.
///
/// The cache is what amortises a decode across a block's *other* entries, so a block holding one
/// entry gains nothing from it: there is no second reader to serve, and buffering the whole entry
/// only to write it out again costs memory for no decode saved. Those blocks are streamed instead,
/// and the budget does not apply to them.
///
/// **That distinction is the difference between this path running and not running.** Budgeting on
/// the largest block outright, as this first did, refused the published benchmark corpus over a
/// single 263.3 MiB block against a 249.6 MiB budget — one stored video, alone in its block, needing
/// no cache — and with it the 48 ordinary blocks behind it. Extraction stayed at 1.30 effective
/// cores. Excluding single-entry blocks from the budget takes the same corpus to 4.63 cores and
/// 2.67 s against 8.34 s.
///
/// A block that is over budget *and* holds many entries is still refused for the whole archive:
/// streaming it would re-decode it once per entry, which for 7-Zip's one-folder default would mean
/// decoding 2.6 GB tens of thousands of times. [`SevenZReader::as_random_access`] then returns
/// `None` and the sequential path runs exactly as before.
pub struct SevenZRandomAccess {
    id: u64,
    path: PathBuf,
    password: Password,
    archive: sevenz_rust2::Archive,
    entries: Vec<Entry>,
    /// cram entry index → `(block index, archive file index)`; `None` for entries with no stream.
    loc: Vec<Option<(usize, usize)>>,
    /// Per block, indexed by block number.
    plan: Vec<BlockPlan>,
    blocks: usize,
    /// Blocks cut into independently-decodable LZMA2 segments, where that was possible. Empty when
    /// no block could be split, which is the case for everything cram writes (already many blocks)
    /// and for any archive written by a single-threaded encoder.
    segmented: Vec<BlockSegments>,
    /// cram entry index → index into [`segmented`]'s flattened unit list. `None` for an entry whose
    /// block was not split.
    unit_of: Vec<Option<usize>>,
}

/// One block's LZMA2 segments, plus where its entries sit in the decoded stream.
struct BlockSegments {
    block: usize,
    /// Absolute file offset one past the block's last packed byte, so a worker reading across a
    /// segment boundary for a straddling entry knows where the stream really ends.
    pack_end: u64,
    segs: Vec<lzma2seg::Segment>,
    /// `(archive file index, uncompressed offset within the block, size)`, in archive order.
    layout: Vec<(usize, u64, u64)>,
}

/// One unit of parallel work: a segment of a segmented block.
#[derive(Clone, Copy)]
struct SegUnit {
    /// Index into `segmented`.
    group: usize,
    /// Index into that group's `segs`.
    seg: usize,
}

impl SevenZRandomAccess {
    /// Build the view, or `None` when block-caching would not be safe.
    fn build(path: &Path, secret: &Secret, entries: &[Entry]) -> Option<Self> {
        // The budget is per worker, since each caches its own block.
        let hw = crate::hw::HwProfile::detect();
        let per_thread = (hw.ram_total / BLOCK_CACHE_RAM_FRACTION) / hw.physical.max(1) as u64;
        Self::build_within(path, secret, entries, per_thread)
    }

    /// [`build`](Self::build) with the cache budget supplied rather than derived, so the decision
    /// this makes can be tested against a real archive without needing one sized in gigabytes. The
    /// gate is the part that silently turned the whole parallel path off, and it went unnoticed
    /// until a benchmark measured it, so it is worth being able to assert on directly.
    fn build_within(
        path: &Path,
        secret: &Secret,
        entries: &[Entry],
        per_thread: u64,
    ) -> Option<Self> {
        let password = Password::new(secret.expose());
        let mut file = std::fs::File::open(path).ok()?;
        let archive = sevenz_rust2::Archive::read(&mut file, &password).ok()?;

        // Never for an encrypted archive. Content encryption is resolved LAZILY by the sequential
        // worker — the header reads with an empty password and the real one is asked for on the
        // first block that fails — and this view has only the header password, so every block would
        // fail with "password required". That is exactly how it broke
        // `sevenz_encrypted_multi_entry_round_trips`. Decoding is the sequential path's job until
        // this can carry a resolved content password.
        if archive_has_aes(&archive) {
            return None;
        }

        // How many entries share each block. A block is usually over budget *because* it holds one
        // large file, and that block is exactly the one the cache cannot help, so count first and
        // judge after.
        let mut per_block = vec![0usize; archive.blocks.len()];
        for b in archive.stream_map.file_block_index.iter().flatten() {
            // A block index the block list does not have: refuse rather than guess.
            *per_block.get_mut(*b)? += 1;
        }

        let plan: Vec<BlockPlan> = archive
            .blocks
            .iter()
            .zip(&per_block)
            .map(|(b, &entries)| BlockPlan {
                entries,
                fits: b.get_unpack_size() <= per_thread,
            })
            .collect();

        // Where each file's bytes sit inside its block, in archive order. Needed to decide which
        // segment serves an entry, and cheap enough to compute for every archive.
        let mut layouts: Vec<Vec<(usize, u64, u64)>> = vec![Vec::new(); archive.blocks.len()];
        let mut at = vec![0u64; archive.blocks.len()];
        for (fi, f) in archive.files.iter().enumerate() {
            if let Some(b) = archive.stream_map.file_block_index[fi] {
                let off = at.get_mut(b)?;
                layouts.get_mut(b)?.push((fi, *off, f.size));
                *off += f.size;
            }
        }

        // Cut what can be cut. A block written by a multi-threaded LZMA2 encoder carries dictionary
        // resets, and each is somewhere a decoder can start cold.
        let segmented = Self::segment_blocks(path, &archive, &mut layouts, per_thread);
        let is_segmented: Vec<bool> = {
            let mut v = vec![false; archive.blocks.len()];
            for g in &segmented {
                v[g.block] = true;
            }
            v
        };

        // Refuse only for a block that needs the cache and cannot have it. Single-entry blocks are
        // streamed, and a segmented block is decoded a segment at a time, so neither one's size is
        // this decision's business. An archive with no block holding any bytes has nothing for this
        // path to do and is left to the sequential one.
        let has_content = archive.blocks.iter().any(|b| b.get_unpack_size() > 0);
        let stuck = plan
            .iter()
            .enumerate()
            .any(|(b, p)| p.entries > 1 && !p.fits && !is_segmented[b]);
        if plan.is_empty() || !has_content || stuck {
            return None;
        }

        // cram's entry list is `archive.files` filtered through `cram_entry`, so the two index
        // spaces differ; walk them together in the same order the list was built.
        let aes = archive_has_aes(&archive);
        let mut loc = Vec::with_capacity(entries.len());
        for (fi, f) in archive.files.iter().enumerate() {
            if cram_entry(f, aes).is_some() {
                loc.push(archive.stream_map.file_block_index[fi].map(|b| (b, fi)));
            }
        }
        if loc.len() != entries.len() {
            // The two walks disagreed; refuse rather than serve the wrong bytes for an entry.
            return None;
        }

        // Which segment serves each entry: the one its bytes START in. An entry straddling a
        // boundary is served by that worker reading on past it, rather than by stitching two.
        let mut group_of = vec![usize::MAX; archive.blocks.len()];
        for (gi, g) in segmented.iter().enumerate() {
            group_of[g.block] = gi;
        }
        let mut base = Vec::with_capacity(segmented.len());
        let mut units = 0usize;
        for g in &segmented {
            base.push(units);
            units += g.segs.len();
        }
        let unit_of: Vec<Option<usize>> = loc
            .iter()
            .map(|l| {
                let (block, fi) = (*l)?;
                let gi = *group_of.get(block)?;
                let g = segmented.get(gi)?;
                let (_, off, _) = *g.layout.iter().find(|(f, _, _)| *f == fi)?;
                // The last segment starting at or before this entry's first byte.
                let s = g.segs.partition_point(|s| s.unpacked_start <= off).max(1) - 1;
                Some(base[gi] + s)
            })
            .collect();

        let blocks = archive.blocks.len();
        Some(Self {
            id: RA_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            path: path.to_path_buf(),
            password,
            archive,
            entries: entries.to_vec(),
            loc,
            plan,
            blocks,
            segmented,
            unit_of,
        })
    }

    /// Cut every block that is a lone LZMA2 coder into independently-decodable segments.
    ///
    /// Restricted to a single coder on purpose. A BCJ or delta filter carries state across the
    /// boundary and BCJ2 is several interleaved streams, so a chain would need the filter re-run
    /// from the folder start and the cut would buy nothing. Those blocks keep the whole-block path.
    fn segment_blocks(
        path: &Path,
        archive: &sevenz_rust2::Archive,
        layouts: &mut [Vec<(usize, u64, u64)>],
        per_thread: u64,
    ) -> Vec<BlockSegments> {
        /// 7z method id for LZMA2.
        const LZMA2: [u8; 1] = [0x21];
        /// A 7z file opens with a 32-byte signature header; pack offsets are relative to its end.
        const SIGNATURE_HEADER: u64 = 32;

        let Ok(mut file) = std::fs::File::open(path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (b, block) in archive.blocks.iter().enumerate() {
            if block.coders.len() != 1 || block.coders[0].encoder_method_id() != LZMA2 {
                continue;
            }
            let Some(&pi) = archive.stream_map.block_first_pack_stream_index().get(b) else {
                continue;
            };
            let (Some(&rel), Some(&len)) = (
                archive.stream_map.pack_stream_offsets().get(pi),
                archive.pack_sizes().get(pi),
            ) else {
                continue;
            };
            let start = SIGNATURE_HEADER + archive.pack_pos() + rel;
            // A walk that fails or finds one segment is not a fault: it means this stream is not
            // splittable, which is the answer for everything a single-threaded encoder wrote.
            let Ok(Some(segs)) = lzma2seg::walk(&mut file, start, len) else {
                continue;
            };
            // Each concurrent segment holds a dictionary window, so the same per-worker budget that
            // bounds the block cache has to bound these. Workers never exceed the core count and the
            // budget is a quarter of RAM divided by it, so passing here caps the whole fan-out at a
            // quarter of RAM however many segments the archive turns out to have.
            //
            // Without this the segment path had no memory bound at all: 19 workers each holding a
            // 128 MiB window reached 2.8 GB on the corpus, which is fine on this machine and would
            // not have been on a small one.
            let widest = segs.iter().map(|s| s.unpacked).max().unwrap_or(0);
            if widest > per_thread {
                continue;
            }
            out.push(BlockSegments {
                block: b,
                pack_end: start + len,
                segs,
                layout: std::mem::take(&mut layouts[b]),
            });
        }
        out
    }

    /// Run `f` over the decoded bytes of cram entry `index`, decoding its block if this thread does
    /// not already hold it.
    fn with_entry_bytes<T>(&self, index: usize, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        let Some(Some((block, file_idx))) = self.loc.get(index).copied() else {
            // A directory, or an entry with no stream: no bytes, which is not an error.
            return Ok(f(&[]));
        };
        BLOCK_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            let hit = matches!(&*c, Some((id, b, _)) if *id == self.id && *b == block);
            if !hit {
                // Drop the previous block before decoding the next, so peak memory is one block per
                // thread rather than two.
                *c = None;
                *c = Some((self.id, block, self.decode_block(block)?));
            }
            let (_, _, items) = c.as_ref().expect("just populated");
            match items.iter().find(|(fi, _)| *fi == file_idx) {
                Some((_, bytes)) => Ok(f(bytes)),
                None => Err(ArchiveError::Corrupt(format!(
                    "7z: entry {file_idx} missing from its own block {block}"
                ))),
            }
        })
    }

    /// Decode one block, returning `(archive file index, bytes)` for each entry in it.
    fn decode_block(&self, block: usize) -> Result<Vec<(usize, Vec<u8>)>> {
        let mut file = std::fs::File::open(&self.path)
            .map_err(|e| ArchiveError::Backend(format!("{}: {e}", self.path.display())))?;
        // One thread: parallelism here comes from decoding different blocks at once, and asking for
        // more would oversubscribe every worker against every other.
        let decoder =
            sevenz_rust2::BlockDecoder::new(1, block, &self.archive, &self.password, &mut file);
        let first = self.archive.stream_map.block_first_file_index[block];
        let mut out = Vec::new();
        let mut n = 0usize;
        decoder
            .for_each_entries(&mut |_entry, reader| {
                let mut buf = Vec::new();
                reader.read_to_end(&mut buf)?;
                out.push((first + n, buf));
                n += 1;
                Ok(true)
            })
            .map_err(map_sevenz)?;
        Ok(out)
    }

    /// Decode one entry straight into `sink`, holding no more than the copy buffer.
    ///
    /// The counterpart to [`decode_block`](Self::decode_block), for the blocks where caching is the
    /// wrong trade: a block holding one entry (the cache would save no decode) or one too large to
    /// hold (the gate has already established that such a block holds one entry). Callers get the
    /// same bytes either way; the difference is peak memory.
    ///
    /// `skip` bytes are discarded first and at most `limit` are copied, so this serves a range as
    /// well as a whole entry. Decoding still starts at the block, because 7z has no way in.
    fn stream_entry(
        &self,
        block: usize,
        file_idx: usize,
        skip: u64,
        limit: u64,
        sink: &mut dyn io::Write,
    ) -> Result<u64> {
        let mut file = std::fs::File::open(&self.path)
            .map_err(|e| ArchiveError::Backend(format!("{}: {e}", self.path.display())))?;
        let decoder =
            sevenz_rust2::BlockDecoder::new(1, block, &self.archive, &self.password, &mut file);
        let first = self.archive.stream_map.block_first_file_index[block];
        let mut n = 0usize;
        let mut written = 0u64;
        let mut found = false;
        decoder
            .for_each_entries(&mut |_entry, reader| {
                let this = first + n;
                n += 1;
                if this != file_idx {
                    // Not ours, but the stream is solid and the decoder is positioned mid-block, so
                    // it has to be advanced past rather than skipped over.
                    io::copy(reader, &mut io::sink())?;
                    return Ok(true);
                }
                if skip > 0 {
                    io::copy(&mut Read::take(&mut *reader, skip), &mut io::sink())?;
                }
                written = io::copy(&mut Read::take(&mut *reader, limit), sink)?;
                found = true;
                // Nothing after this entry is wanted, and finishing the block would decode the rest
                // of it for nobody.
                Ok(false)
            })
            .map_err(map_sevenz)?;
        if !found {
            return Err(ArchiveError::Corrupt(format!(
                "7z: entry {file_idx} missing from its own block {block}"
            )));
        }
        Ok(written)
    }

    /// Resolve a flattened unit index back to its group and segment.
    fn unit(&self, unit: usize) -> Option<SegUnit> {
        let mut base = 0usize;
        for (group, g) in self.segmented.iter().enumerate() {
            if unit < base + g.segs.len() {
                return Some(SegUnit {
                    group,
                    seg: unit - base,
                });
            }
            base += g.segs.len();
        }
        None
    }

    /// Decode one LZMA2 segment and hand over the entries that start inside it, in order.
    ///
    /// The reader is given the rest of the block's pack stream rather than just this segment, so an
    /// entry straddling the boundary is served by reading on into the next segment. Crossing a
    /// dictionary reset mid-read is ordinary — it is what the sequential decoder does — and it
    /// costs only the tail of the one entry, since the reader is dropped straight after.
    fn stream_segment(
        &self,
        u: SegUnit,
        want: &[(usize, usize)],
        visit: &mut dyn FnMut(usize, &mut dyn Read) -> bool,
    ) -> Result<()> {
        let g = &self.segmented[u.group];
        let seg = &g.segs[u.seg];

        // (cram index, offset within the block, size), in stream order.
        let mut serve: Vec<(usize, u64, u64)> = want
            .iter()
            .filter_map(|&(fi, index)| {
                g.layout
                    .iter()
                    .find(|(f, _, _)| *f == fi)
                    .map(|&(_, off, size)| (index, off, size))
            })
            .collect();
        serve.sort_unstable_by_key(|&(_, off, _)| off);
        if serve.is_empty() {
            return Ok(());
        }

        // How far past this segment the last entry reaches, which the dictionary must also cover.
        let seg_end = seg.unpacked_start + seg.unpacked;
        let spill = serve
            .iter()
            .map(|&(_, off, size)| (off + size).saturating_sub(seg_end))
            .max()
            .unwrap_or(0);

        let mut file = std::fs::File::open(&self.path)
            .map_err(|e| ArchiveError::Backend(format!("{}: {e}", self.path.display())))?;
        file.seek(std::io::SeekFrom::Start(seg.comp_off))
            .map_err(|e| ArchiveError::Backend(format!("{}: {e}", self.path.display())))?;
        let src = file.take(g.pack_end.saturating_sub(seg.comp_off));
        let mut r = lzma_rust2::Lzma2Reader::new(src, lzma2seg::dict_window(seg, spill), None);

        let mut pos = seg.unpacked_start;
        for (index, off, size) in serve {
            if off > pos {
                // The head of an entry that began in the previous segment. It belongs to that
                // segment's worker, so decode past it without keeping it.
                io::copy(&mut (&mut r).take(off - pos), &mut io::sink())?;
            }
            let mut body = (&mut r).take(size);
            let go = visit(index, &mut body);
            // Whatever the visitor left has to go through the decoder anyway: the next entry starts
            // where this one ends.
            io::copy(&mut body, &mut io::sink())?;
            pos = off + size;
            if !go {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Where entry `index` lives and what its block costs, or `None` when it has no bytes.
    fn placement(&self, index: usize) -> Option<(usize, usize, &BlockPlan)> {
        let (block, file_idx) = self.loc.get(index).copied().flatten()?;
        let plan = self.plan.get(block)?;
        Some((block, file_idx, plan))
    }
}

impl crate::reader::RandomAccessReader for SevenZRandomAccess {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// One shot over the whole entry, so the cache earns its memory only when the block has other
    /// entries to serve from it. A block of one is streamed however small it is: buffering a file
    /// in RAM to write it straight back out saves no decode.
    fn copy_entry(&self, index: usize, out: &mut dyn io::Write) -> Result<u64> {
        match self.placement(index) {
            Some((block, file_idx, plan)) if plan.entries == 1 || !plan.fits => {
                self.stream_entry(block, file_idx, 0, u64::MAX, out)
            }
            _ => self.with_entry_bytes(index, |bytes| {
                out.write_all(bytes)?;
                Ok(bytes.len() as u64)
            })?,
        }
    }

    /// The mount primitive, called repeatedly with small ranges of one entry, so here the cache is
    /// worth having even for a block of one — streaming would decode the block again per range.
    /// Only a block too large to hold is streamed, and the gate guarantees it holds one entry.
    fn read_range(&self, index: usize, off: u64, len: u64) -> Result<Vec<u8>> {
        match self.placement(index) {
            Some((block, file_idx, plan)) if !plan.fits => {
                let mut buf = Vec::new();
                self.stream_entry(block, file_idx, off, len, &mut buf)?;
                Ok(buf)
            }
            _ => self.with_entry_bytes(index, |bytes| {
                let start = (off as usize).min(bytes.len());
                let end = start.saturating_add(len as usize).min(bytes.len());
                bytes[start..end].to_vec()
            }),
        }
    }

    /// The decode unit, so the engine keeps its entries on one worker and decodes it about once:
    /// the LZMA2 segment where the block was splittable, the block itself where it was not.
    ///
    /// One key space for both, with segments numbered above every block, so a segmented and an
    /// unsegmented block can never collide onto one work item.
    fn locality_key(&self, index: usize) -> Option<u64> {
        if let Some(unit) = self.unit_of.get(index).copied().flatten() {
            return Some((self.blocks + unit) as u64);
        }
        self.loc
            .get(index)
            .copied()
            .flatten()
            .map(|(b, _)| b as u64)
    }

    /// A block costs one full decode, so its entries must be ONE work item, not 1,215 adjacent ones
    /// that rayon is free to steal apart. Without this, extracting the test archive cost 110
    /// CPU-seconds against 11 sequential — every block decoded roughly ten times.
    fn coalesce_locality(&self) -> bool {
        true
    }

    fn streams_units(&self) -> bool {
        true
    }

    /// One pass per block, entries handed over as they decode. Nothing is buffered, so extracting or
    /// verifying a 7z costs the copy buffer rather than a block per worker.
    ///
    /// Grouped by block rather than assuming one, because the engine's collision groups are formed
    /// from destination names and a fused cluster can in principle span blocks. Blocks are visited in
    /// increasing order and entries within a block in archive order, which together is archive order,
    /// so two entries resolving to one destination still land last-writer-wins.
    fn copy_unit(
        &self,
        indices: &[usize],
        visit: &mut dyn FnMut(usize, &mut dyn Read) -> bool,
    ) -> Result<()> {
        let mut by_block: std::collections::BTreeMap<usize, Vec<(usize, usize)>> =
            std::collections::BTreeMap::new();
        let mut by_segment: std::collections::BTreeMap<usize, Vec<(usize, usize)>> =
            std::collections::BTreeMap::new();
        let mut streamless = Vec::new();
        for &i in indices {
            match self.loc.get(i).copied().flatten() {
                Some((block, file_idx)) => {
                    match self.unit_of.get(i).copied().flatten() {
                        // A segmented block: the work item is the segment, not the block.
                        Some(unit) => by_segment.entry(unit).or_default().push((file_idx, i)),
                        None => by_block.entry(block).or_default().push((file_idx, i)),
                    }
                }
                // A directory or an empty file: no block holds it, and it costs nothing to serve.
                None => streamless.push(i),
            }
        }
        for i in streamless {
            if !visit(i, &mut io::empty()) {
                return Ok(());
            }
        }

        for (unit, want) in by_segment {
            let Some(u) = self.unit(unit) else {
                return Err(ArchiveError::Corrupt(format!(
                    "7z: no such decode unit {unit}"
                )));
            };
            self.stream_segment(u, &want, visit)?;
        }

        for (block, mut want) in by_block {
            want.sort_unstable();
            let mut file = std::fs::File::open(&self.path)
                .map_err(|e| ArchiveError::Backend(format!("{}: {e}", self.path.display())))?;
            // One thread: parallelism comes from decoding different blocks at once, and asking for
            // more here would oversubscribe every worker against every other.
            let decoder =
                sevenz_rust2::BlockDecoder::new(1, block, &self.archive, &self.password, &mut file);
            let first = self.archive.stream_map.block_first_file_index[block];
            let mut n = 0usize;
            let mut pos = 0usize;
            let mut stopped = false;
            decoder
                .for_each_entries(&mut |_entry, reader| {
                    let file_idx = first + n;
                    n += 1;
                    if pos < want.len() && want[pos].0 == file_idx {
                        let index = want[pos].1;
                        pos += 1;
                        if !visit(index, &mut *reader) {
                            stopped = true;
                        }
                    }
                    // Whatever the visitor did not take still has to go through the decoder: the
                    // block is one stream and the next entry begins where this one ends.
                    io::copy(reader, &mut io::sink())?;
                    // Nothing later in this block is wanted, so stop rather than decode it for
                    // nobody. This is what makes extracting a handful of entries from a large solid
                    // block cost the prefix rather than the whole thing.
                    Ok(!stopped && pos < want.len())
                })
                .map_err(map_sevenz)?;
            if stopped {
                return Ok(());
            }
        }
        Ok(())
    }

    /// What the planner fans out over. A segmented block contributes its segments rather than
    /// itself — the whole point being that a 7-Zip archive is one block and 21 units.
    fn decode_units(&self) -> Option<usize> {
        let segs: usize = self.segmented.iter().map(|g| g.segs.len()).sum();
        Some(self.blocks - self.segmented.len() + segs)
    }
}

pub struct SevenZReader {
    path: PathBuf,
    name: String,
    /// The password that read the header (empty if the header was not encrypted). Reused as the
    /// starting point for content decryption.
    header_pw: Secret,
    pw: Arc<dyn PasswordProvider>,
    entries: Vec<Entry>,
    rx: Option<Receiver<SzMsg>>,
    started: bool,
    /// Present only when block-caching is safe; see [`SevenZRandomAccess`].
    ra: Option<SevenZRandomAccess>,
}

impl SevenZReader {
    pub fn open(path: &Path, pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // Resolve the header password: try empty first, then ask the provider on a password error
        // (encrypted-names archives can't even be listed without it).
        let mut secret = Secret::new(String::new());
        let mut attempt = 0u32;
        let entries = loop {
            match read_metadata(path, &secret) {
                Ok(entries) => break entries,
                Err(e) if is_password_error(&e) => {
                    match pw.password(&PasswordRequest {
                        archive: &name,
                        entry: None,
                        for_header: true,
                        attempt,
                    }) {
                        Some(s) => {
                            secret = s;
                            attempt += 1;
                        }
                        None => {
                            return Err(if attempt == 0 {
                                ArchiveError::PasswordRequired
                            } else {
                                ArchiveError::WrongPassword
                            });
                        }
                    }
                }
                Err(e) => return Err(map_sevenz(e)),
            }
        };

        // Offered only when the archive's blocks fit the cache budget; `None` leaves the sequential
        // path in place, which is what every 7z archive got before this existed.
        let ra = SevenZRandomAccess::build(path, &secret, &entries);

        Ok(Self {
            path: path.to_path_buf(),
            name,
            header_pw: secret,
            pw,
            entries,
            rx: None,
            started: false,
            ra,
        })
    }

    /// Spawn the extraction worker on first use.
    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let (tx, rx) = sync_channel::<SzMsg>(1); // bounded → backpressure, ~1–2 entries in flight
        let path = self.path.clone();
        let name = self.name.clone();
        let secret = self.header_pw.clone();
        let pw = Arc::clone(&self.pw);
        thread::spawn(move || worker(path, name, secret, pw, tx));
        self.rx = Some(rx);
    }
}

impl ArchiveReader for SevenZReader {
    fn as_random_access(&self) -> Option<&dyn crate::reader::RandomAccessReader> {
        self.ra
            .as_ref()
            .map(|ra| ra as &dyn crate::reader::RandomAccessReader)
    }

    fn format(&self) -> Format {
        Format::sevenz()
    }

    fn entries(&self) -> Result<&[Entry]> {
        Ok(&self.entries)
    }

    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>> {
        self.ensure_started();
        let rx = self.rx.as_ref().unwrap();
        match rx.recv() {
            Ok(SzMsg::FileStart(entry)) => Ok(Some(EntryStream {
                entry,
                body: Box::new(SzBody {
                    rx,
                    cur: io::Cursor::new(Vec::new()),
                    done: false,
                }),
                meta_final: true,
            })),
            Ok(SzMsg::Dir(entry)) => Ok(Some(EntryStream {
                entry,
                body: Box::new(io::empty()),
                meta_final: true,
            })),
            Ok(SzMsg::Err(e)) => Err(e),
            // A stray body message with no active entry means the stream is out of sync.
            Ok(SzMsg::Chunk(_)) | Ok(SzMsg::FileEnd) => {
                Err(ArchiveError::Backend("7z stream desync".into()))
            }
            Err(_) => Ok(None), // channel closed → done
        }
    }
}

#[cfg(test)]
mod mtime_guard_tests {
    use super::*;
    use sevenz_rust2::{ArchiveEntry, NtTime};

    #[test]
    fn seven_z_mtime_rejects_hostile_filetime_without_panicking() {
        let mut e = ArchiveEntry::new();
        // A near-`u64::MAX` FILETIME would overflow `NtTime -> SystemTime`, must be rejected, not
        // converted (the conversion uses a panicking `+`).
        e.has_last_modified_date = true;
        e.last_modified_date = NtTime::from(u64::MAX);
        assert!(seven_z_mtime(&e).is_none());
        // A realistic ~2020 FILETIME (≈1.32e17 ticks since 1601) converts fine.
        e.last_modified_date = NtTime::from(132_223_104_000_000_000);
        assert!(seven_z_mtime(&e).is_some());
        // The presence flag is honored.
        e.has_last_modified_date = false;
        assert!(seven_z_mtime(&e).is_none());
    }
}

/// The cache budget decides whether 7z extraction runs in parallel at all, and getting it wrong is
/// invisible: extraction still succeeds, just on one thread. That is exactly what happened —
/// budgeting on the largest block outright refused the published benchmark corpus over one 263.3
/// MiB stored video sitting alone in its block, taking the other 48 blocks down with it, and only a
/// benchmark noticed. These assert on the decision rather than on a timing.
#[cfg(test)]
mod gate_tests {
    use super::*;

    /// A two-block archive: one block holding several small entries, one holding a single large
    /// entry. Returns the archive path and the entry list read back from it.
    fn two_block_archive(dir: &Path, big: usize) -> (PathBuf, Vec<Entry>) {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for i in 0..8 {
            std::fs::write(
                src.join(format!("small{i}.txt")),
                vec![b'a' + i as u8; 4096],
            )
            .unwrap();
        }
        // Incompressible, so the writer stores it and the block is its full size.
        let mut noise = Vec::with_capacity(big);
        let mut x = 0x9E3779B97F4A7C15u64;
        while noise.len() < big {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            noise.extend_from_slice(&x.to_le_bytes());
        }
        noise.truncate(big);
        std::fs::write(src.join("big.bin"), &noise).unwrap();

        let out = dir.join("a.7z");
        crate::engine::create::create(
            &out,
            Format::sevenz(),
            std::slice::from_ref(&src),
            crate::writer::CreateOptions::default(),
            &crate::progress::NullSink,
        )
        .unwrap();
        let entries = read_metadata(&out, &Secret::new(String::new())).unwrap();
        (out, entries)
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cram-7z-gate-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The corpus case: one block over budget, holding one entry. It must NOT veto the archive —
    /// that block is streamed and every other block still gets the cache.
    #[test]
    fn a_single_entry_block_over_budget_does_not_refuse_the_archive() {
        let dir = scratch("single");
        let big = 512 * 1024;
        let (path, entries) = two_block_archive(&dir, big);
        let secret = Secret::new(String::new());

        // A budget under the big block but over the small one: the exact 5.5%-miss shape.
        let ra = SevenZRandomAccess::build_within(&path, &secret, &entries, (big / 2) as u64)
            .expect("a single-entry block over budget must not refuse the archive");
        assert!(
            ra.plan.iter().any(|p| !p.fits && p.entries == 1),
            "test archive did not produce the over-budget single-entry block it is about"
        );

        // Every entry still reads back correctly, streamed or cached.
        for (i, e) in entries.iter().enumerate() {
            if e.kind != EntryKind::File {
                continue;
            }
            let mut got = Vec::new();
            let n = crate::reader::RandomAccessReader::copy_entry(&ra, i, &mut got).unwrap();
            let name = e.path.safe().file_name().unwrap();
            let want = std::fs::read(dir.join("src").join(name)).unwrap();
            assert_eq!(n, want.len() as u64, "{}", e.path.raw());
            assert_eq!(got, want, "{}", e.path.raw());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The case the budget exists for: a block over budget that many entries share. Streaming it
    /// would re-decode it once per entry, so the whole archive falls back to sequential.
    #[test]
    fn a_multi_entry_block_over_budget_still_refuses_the_archive() {
        let dir = scratch("multi");
        let (path, entries) = two_block_archive(&dir, 512 * 1024);
        let secret = Secret::new(String::new());
        // One byte of budget: even the small shared block cannot be held.
        assert!(
            SevenZRandomAccess::build_within(&path, &secret, &entries, 1).is_none(),
            "a shared block that cannot be cached must fall back to the sequential path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The unit pass must hand over every entry, in archive order, with the right bytes. This is the
    /// path extraction and `cram t` both take, so a fault here is silent wrong output.
    #[test]
    fn copy_unit_serves_every_entry_in_archive_order() {
        let dir = scratch("unit-all");
        let (path, entries) = two_block_archive(&dir, 512 * 1024);
        let secret = Secret::new(String::new());
        let ra = SevenZRandomAccess::build_within(&path, &secret, &entries, u64::MAX).unwrap();

        let all: Vec<usize> = (0..entries.len())
            .filter(|&i| entries[i].kind == EntryKind::File)
            .collect();
        let mut seen = Vec::new();
        crate::reader::RandomAccessReader::copy_unit(&ra, &all, &mut |i, body| {
            let mut got = Vec::new();
            body.read_to_end(&mut got).unwrap();
            seen.push((i, got));
            true
        })
        .unwrap();

        assert_eq!(seen.len(), all.len(), "every entry must be served once");
        let order: Vec<usize> = seen.iter().map(|(i, _)| *i).collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted, "entries must arrive in archive order");
        for (i, got) in &seen {
            let name = entries[*i].path.safe().file_name().unwrap();
            assert_eq!(*got, std::fs::read(dir.join("src").join(name)).unwrap());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Asking for part of a block must serve exactly that part. The entries around it still have to
    /// be decoded to reach it, but they must not be handed over.
    #[test]
    fn copy_unit_serves_only_the_entries_asked_for() {
        let dir = scratch("unit-some");
        let (path, entries) = two_block_archive(&dir, 512 * 1024);
        let secret = Secret::new(String::new());
        let ra = SevenZRandomAccess::build_within(&path, &secret, &entries, u64::MAX).unwrap();

        let files: Vec<usize> = (0..entries.len())
            .filter(|&i| entries[i].kind == EntryKind::File)
            .collect();
        let want = vec![files[1], files[3]];
        let mut seen = Vec::new();
        crate::reader::RandomAccessReader::copy_unit(&ra, &want, &mut |i, body| {
            let mut got = Vec::new();
            body.read_to_end(&mut got).unwrap();
            seen.push(i);
            true
        })
        .unwrap();
        assert_eq!(seen, want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A visitor that returns `false` is cancelling, and the pass must stop rather than decode the
    /// rest of the block for nobody.
    #[test]
    fn copy_unit_stops_when_the_visitor_says_so() {
        let dir = scratch("unit-stop");
        let (path, entries) = two_block_archive(&dir, 512 * 1024);
        let secret = Secret::new(String::new());
        let ra = SevenZRandomAccess::build_within(&path, &secret, &entries, u64::MAX).unwrap();

        let files: Vec<usize> = (0..entries.len())
            .filter(|&i| entries[i].kind == EntryKind::File)
            .collect();
        let mut n = 0;
        crate::reader::RandomAccessReader::copy_unit(&ra, &files, &mut |_i, _body| {
            n += 1;
            false
        })
        .unwrap();
        assert_eq!(n, 1, "the pass must stop at the first refusal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A range served by streaming must match one served from the cache, since the same entry can
    /// take either path depending only on how big its block is.
    #[test]
    fn a_streamed_range_matches_a_cached_one() {
        let dir = scratch("range");
        let big = 512 * 1024;
        let (path, entries) = two_block_archive(&dir, big);
        let secret = Secret::new(String::new());
        let idx = entries
            .iter()
            .position(|e| e.path.raw().ends_with("big.bin"))
            .unwrap();

        let cached = SevenZRandomAccess::build_within(&path, &secret, &entries, u64::MAX).unwrap();
        let streamed =
            SevenZRandomAccess::build_within(&path, &secret, &entries, (big / 2) as u64).unwrap();
        assert!(cached.plan.iter().all(|p| p.fits));
        assert!(streamed.plan.iter().any(|p| !p.fits));

        use crate::reader::RandomAccessReader;
        for (off, len) in [
            (0u64, 100u64),
            (1000, 4096),
            (big as u64 - 10, 64),
            (0, u64::MAX),
        ] {
            assert_eq!(
                RandomAccessReader::read_range(&cached, idx, off, len).unwrap(),
                RandomAccessReader::read_range(&streamed, idx, off, len).unwrap(),
                "range {off}+{len}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
