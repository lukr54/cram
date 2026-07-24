//! Find duplicate files across folders and drives, **without** archiving anything.
//!
//! The `.cram` writer dedups *chunks inside one archive*; this dedups *whole files across a whole
//! collection*, which is the problem an inherited photo/video pile actually has: the same JPEG sitting
//! under a dozen random names on five drives. It answers "what is duplicated, and how much space would
//! I get back", reading as little as possible to be certain.
//!
//! ## Reading as little as possible
//!
//! Hashing hundreds of terabytes is not viable, and it is not necessary. Three gates run in order, each
//! only over what survived the last:
//!
//! 1. **Size.** A file whose size is unique in the whole set *cannot* have a byte-identical twin, no
//!    read at all. On a real collection this eliminates most files (and most bytes) outright.
//! 2. **Partial hash**, the first and last 64 KiB of each same-size file. Near-misses (re-encodes,
//!    different captures that happen to match in size) separate here for ~128 KiB instead of a full read.
//! 3. **Full BLAKE3**, only for files that still share both size and partial hash. Equal full hash is
//!    the confirmation; nothing is called a duplicate on a partial read.
//!
//! ## Per-drive scheduling
//!
//! Parallel reads make an SSD faster and a spinning disk *slower* (seek thrash). Files are therefore
//! grouped by the volume they live on, every volume is worked concurrently (a 5-drive pile reads 5
//! drives at once), but the reader count *within* a volume comes from [`crate::hw`]'s media detection:
//! one sequential reader on an HDD, several on an SSD.
//!
//! ## What is safe to act on
//!
//! [`GroupKind::Exact`] groups are byte-identical and interchangeable. [`GroupKind::Similar`] groups
//! (see [`similar`]) are *visually* alike and are **not** interchangeable, a burst of near-identical
//! frames is a legitimate set of distinct photos. Similar groups always report `reclaimable == 0` and
//! exist for human review only; no automated action may consume them.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::error::{ArchiveError, Result};
use crate::hw;
use crate::progress::ProgressSink;

/// Bytes read from each end of a file during the cheap pre-hash pass.
const PARTIAL_WINDOW: u64 = 64 * 1024;
/// Below this size the partial pass would read the whole file anyway, so it is skipped and the file
/// goes straight to a full hash.
const PARTIAL_MIN_SIZE: u64 = 4 * PARTIAL_WINDOW;
/// Streaming read buffer for the full hash.
const READ_BUF: usize = 256 * 1024;
/// Upper bound on concurrent readers per volume: hashing is I/O-bound long before BLAKE3 is the wall.
const MAX_READERS_PER_VOLUME: usize = 8;

/// Knobs for a duplicate scan.
#[derive(Clone, Debug)]
pub struct DedupOptions {
    /// Ignore files smaller than this. Defaults to 1, which drops zero-byte files; every empty file
    /// is "identical" to every other, which is noise rather than a finding.
    pub min_size: u64,
    /// Also look for *visually similar* images (not just byte-identical ones). Report-only.
    pub similar_images: bool,
    /// Maximum perceptual-hash Hamming distance for two images to count as similar. Small is strict;
    /// see [`similar::MAX_DISTANCE`].
    pub similar_distance: u32,
}

impl Default for DedupOptions {
    fn default() -> Self {
        Self {
            min_size: 1,
            similar_images: false,
            similar_distance: similar::DEFAULT_DISTANCE,
        }
    }
}

/// Filesystem identity of a file: two paths with the same [`FileId`] are the *same bytes on disk*
/// (a hard link), not two copies. Reclaimable space must not count them twice, or the tool would
/// promise back space that is not there, and, worse, claim to have freed it a second time on a
/// re-run over an already-linked collection.
///
/// On Unix this is `(dev, ino)` and comes free with the `stat` the walk already does. On Windows it is
/// the volume serial plus 64-bit file index, which requires actually opening the file, so it is
/// resolved lazily, only for files that turn out to be duplicates (see [`file_identity`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FileId {
    dev: u64,
    ino: u64,
}

/// One file found by the scan.
#[derive(Clone, Debug)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub size: u64,
    /// Filesystem identity, where the platform exposes it (see [`FileId`]).
    pub id: Option<FileId>,
}

/// Why a group's files are grouped, and, decisively, whether they are interchangeable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupKind {
    /// Byte-identical: same size, same full BLAKE3. Interchangeable; safe to act on.
    Exact,
    /// Visually similar images within the perceptual-hash threshold. **Not** byte-identical and **not**
    /// interchangeable, review by hand. Never eligible for automated deletion or linking.
    Similar,
}

/// A set of files that are duplicates of one another.
#[derive(Clone, Debug)]
pub struct DupeGroup {
    pub kind: GroupKind,
    /// Members, sorted by path so output is deterministic.
    pub files: Vec<ScannedFile>,
    /// Bytes recoverable by reducing this group to a single physical copy. Always 0 for
    /// [`GroupKind::Similar`], whose members are not interchangeable.
    pub reclaimable: u64,
}

/// The outcome of a scan.
#[derive(Clone, Debug, Default)]
pub struct DedupReport {
    pub groups: Vec<DupeGroup>,
    /// Files considered (after `min_size` filtering).
    pub files_scanned: u64,
    /// Total logical size of those files.
    pub bytes_scanned: u64,
    /// Bytes actually read to reach the verdict, the number that shows how much work the size gate saved.
    pub bytes_hashed: u64,
    /// Files skipped because they could not be opened or read (permissions, dead links, I/O errors).
    pub unreadable: u64,
    /// True if the scan stopped early because the sink was cancelled; results so far are still valid.
    pub cancelled: bool,
}

impl DedupReport {
    /// Only the byte-identical groups, the ones that may be acted on.
    pub fn exact_groups(&self) -> impl Iterator<Item = &DupeGroup> {
        self.groups.iter().filter(|g| g.kind == GroupKind::Exact)
    }
    /// Only the visually-similar groups (review-only).
    pub fn similar_groups(&self) -> impl Iterator<Item = &DupeGroup> {
        self.groups.iter().filter(|g| g.kind == GroupKind::Similar)
    }
    /// Total space recoverable by reducing every exact group to one copy.
    pub fn reclaimable(&self) -> u64 {
        self.exact_groups().map(|g| g.reclaimable).sum()
    }
    /// Number of redundant physical copies across all exact groups.
    pub fn redundant_files(&self) -> u64 {
        self.exact_groups()
            .map(|g| distinct_copies(&g.files).saturating_sub(1) as u64)
            .sum()
    }
}

/// Scan `roots` for duplicate files. Read-only: nothing is created, moved, or deleted.
///
/// Progress is reported to `sink` as bytes are hashed, and the scan stops promptly (returning what it
/// has, with `cancelled` set) when the sink is cancelled.
pub fn scan(
    roots: &[PathBuf],
    opts: &DedupOptions,
    sink: &dyn ProgressSink,
) -> Result<DedupReport> {
    let mut report = DedupReport::default();

    // ---- 1. Walk ------------------------------------------------------------------------------
    let mut files: Vec<ScannedFile> = Vec::new();
    for root in roots {
        walk(
            root,
            opts.min_size,
            &mut files,
            &mut report.unreadable,
            sink,
        )?;
    }
    report.files_scanned = files.len() as u64;
    report.bytes_scanned = files.iter().map(|f| f.size).sum();

    // ---- 2. Size gate -------------------------------------------------------------------------
    // A unique size cannot have a byte-identical twin, so those files are never read at all.
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        by_size.entry(f.size).or_default().push(i);
    }
    let mut candidates: Vec<usize> = by_size
        .values()
        .filter(|v| v.len() > 1)
        .flat_map(|v| v.iter().copied())
        .collect();

    // ---- 3. Partial-hash gate -----------------------------------------------------------------
    // Only worth doing for files big enough that 128 KiB is meaningfully cheaper than the whole file.
    let (big, small): (Vec<usize>, Vec<usize>) = candidates
        .iter()
        .partition(|&&i| files[i].size >= PARTIAL_MIN_SIZE);
    if !big.is_empty() {
        let partial = hash_pass(&files, &big, HashMode::Partial, sink, &mut report)?;
        // Survivors are those still sharing (size, partial hash) with someone else.
        let mut buckets: HashMap<(u64, [u8; 32]), Vec<usize>> = HashMap::new();
        for (&i, h) in &partial {
            buckets.entry((files[i].size, *h)).or_default().push(i);
        }
        candidates = buckets
            .values()
            .filter(|v| v.len() > 1)
            .flat_map(|v| v.iter().copied())
            .collect();
        candidates.extend(small);
    }

    // ---- 4. Full hash, the confirmation ------------------------------------------------------
    let full = hash_pass(&files, &candidates, HashMode::Full, sink, &mut report)?;
    let mut exact: HashMap<(u64, [u8; 32]), Vec<usize>> = HashMap::new();
    for (&i, h) in &full {
        exact.entry((files[i].size, *h)).or_default().push(i);
    }
    for ((size, _), idxs) in exact {
        if idxs.len() < 2 {
            continue;
        }
        let mut members: Vec<ScannedFile> = idxs.iter().map(|&i| files[i].clone()).collect();
        members.sort_by(|a, b| a.path.cmp(&b.path));
        // Resolve identity for any member the walk could not supply it for (Windows, where it costs
        // an open). Only duplicates need it, so this is a handful of files rather than the whole set;
        // and without it, copies that are *already* hard-linked would be counted as reclaimable space
        // that does not exist.
        for m in &mut members {
            if m.id.is_none() {
                m.id = file_identity(&m.path);
            }
        }
        // Hard-linked members are one physical file; only extra *physical* copies are reclaimable.
        let reclaimable = (distinct_copies(&members).saturating_sub(1) as u64) * size;
        report.groups.push(DupeGroup {
            kind: GroupKind::Exact,
            files: members,
            reclaimable,
        });
    }

    // ---- 5. Visually-similar images (opt-in, report-only) -------------------------------------
    if opts.similar_images && !sink.is_cancelled() {
        // Feed one representative per exact group plus every non-duplicated file, so an exact set does
        // not also reappear as a "similar" finding.
        let mut already: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for g in report.groups.iter().filter(|g| g.kind == GroupKind::Exact) {
            for f in g.files.iter().skip(1) {
                already.insert(f.path.clone());
            }
        }
        let pool: Vec<&ScannedFile> = files
            .iter()
            .filter(|f| !already.contains(&f.path))
            .collect();
        let sim = similar::find(&pool, opts.similar_distance, sink, &mut report.unreadable);
        report.groups.extend(sim);
    }

    // Biggest win first, that is the order a human wants to review.
    report.groups.sort_by(|a, b| {
        b.reclaimable.cmp(&a.reclaimable).then_with(|| {
            a.files
                .first()
                .map(|f| &f.path)
                .cmp(&b.files.first().map(|f| &f.path))
        })
    });
    report.cancelled = sink.is_cancelled();
    Ok(report)
}

/// Number of distinct *physical* files among `files`: hard links to one inode count once. Members whose
/// identity is unknown (see [`FileId`]) each count as their own copy.
fn distinct_copies(files: &[ScannedFile]) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut unknown = 0usize;
    for f in files {
        match f.id {
            Some(id) => {
                seen.insert(id);
            }
            None => unknown += 1,
        }
    }
    seen.len() + unknown
}

/// Recursively collect regular files at or above `min_size`. Symlinks are never followed, that would
/// invent "duplicates" that are really one file, and could loop forever. Unreadable directories are
/// counted and skipped rather than aborting a scan that may span many drives.
fn walk(
    path: &Path,
    min_size: u64,
    out: &mut Vec<ScannedFile>,
    unreadable: &mut u64,
    sink: &dyn ProgressSink,
) -> Result<()> {
    if sink.is_cancelled() {
        return Err(ArchiveError::Cancelled);
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => {
            *unreadable += 1;
            return Ok(());
        }
    };
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        let entries = match std::fs::read_dir(path) {
            Ok(e) => e,
            Err(_) => {
                *unreadable += 1;
                return Ok(());
            }
        };
        for entry in entries.flatten() {
            walk(&entry.path(), min_size, out, unreadable, sink)?;
        }
    } else if meta.is_file() && meta.len() >= min_size.max(1) {
        out.push(ScannedFile {
            path: path.to_path_buf(),
            size: meta.len(),
            id: file_id(&meta),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some(FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// Windows cannot answer this from a `Metadata`, it needs an open handle, so the walk leaves it
/// unset and [`file_identity`] fills it in later for the few files that turn out to be duplicates.
/// Paying an extra file open for every file in a multi-terabyte walk would cost far more than it saves.
#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> Option<FileId> {
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HashMode {
    /// First + last [`PARTIAL_WINDOW`] bytes only.
    Partial,
    /// The whole file.
    Full,
}

/// Hash every file in `idxs`, grouped by volume so each drive is read at its own best concurrency
/// (see the module docs). Returns index → hash; unreadable files are counted and omitted.
fn hash_pass(
    files: &[ScannedFile],
    idxs: &[usize],
    mode: HashMode,
    sink: &dyn ProgressSink,
    report: &mut DedupReport,
) -> Result<HashMap<usize, [u8; 32]>> {
    if idxs.is_empty() {
        return Ok(HashMap::new());
    }
    // Group by volume: same drive → same reader budget.
    let mut by_volume: HashMap<String, Vec<usize>> = HashMap::new();
    for &i in idxs {
        by_volume.entry(volume_key(&files[i])).or_default().push(i);
    }

    let out: Mutex<HashMap<usize, [u8; 32]>> = Mutex::new(HashMap::new());
    let unreadable = std::sync::atomic::AtomicU64::new(0);
    let hashed = std::sync::atomic::AtomicU64::new(0);
    let stop = AtomicBool::new(false);

    // One OS thread per volume so separate drives genuinely read at the same time; inside a volume, a
    // rayon pool sized to that drive's media (1 for a spinning disk).
    std::thread::scope(|scope| {
        for members in by_volume.values() {
            let (out, unreadable, hashed, stop) = (&out, &unreadable, &hashed, &stop);
            scope.spawn(move || {
                let readers = readers_for(&files[members[0]].path);
                let pool = match rayon::ThreadPoolBuilder::new().num_threads(readers).build() {
                    Ok(p) => p,
                    Err(_) => return, // fall back: this volume is simply not hashed
                };
                pool.install(|| {
                    members.par_iter().for_each(|&i| {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        if sink.is_cancelled() {
                            stop.store(true, Ordering::Relaxed);
                            return;
                        }
                        sink.wait_if_paused();
                        match hash_file(&files[i].path, files[i].size, mode, sink, stop) {
                            Ok(Some(h)) => {
                                hashed.fetch_add(read_len(files[i].size, mode), Ordering::Relaxed);
                                out.lock().unwrap().insert(i, h);
                            }
                            Ok(None) => {} // cancelled mid-file
                            Err(_) => {
                                unreadable.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                });
            });
        }
    });

    report.unreadable += unreadable.load(Ordering::Relaxed);
    report.bytes_hashed += hashed.load(Ordering::Relaxed);
    Ok(out.into_inner().unwrap())
}

/// Bytes a pass actually reads for a file of `size`, what progress and the "bytes read" tally mean.
fn read_len(size: u64, mode: HashMode) -> u64 {
    match mode {
        HashMode::Full => size,
        HashMode::Partial => (2 * PARTIAL_WINDOW).min(size),
    }
}

/// A key identifying the volume a file lives on, so files on one drive are scheduled together. On Unix
/// the device number is already in hand from `stat`; on Windows the path prefix (`C:`) stands in.
fn volume_key(f: &ScannedFile) -> String {
    if let Some(id) = f.id {
        return format!("dev:{}", id.dev);
    }
    volume_of(&f.path)
}

/// Volume identity for an arbitrary path, including one that does not exist yet (a quarantine
/// directory about to be created), in that case the nearest existing ancestor answers, since a new
/// directory lands on the same filesystem as its parent.
///
/// Whether two paths share a volume decides whether a hard link between them is even possible, so this
/// has to be right: on Unix it is the device number from `stat`, and on Windows the drive-letter prefix.
pub(crate) fn volume_of(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut probe = Some(path);
        while let Some(p) = probe {
            if let Ok(m) = std::fs::metadata(p) {
                return format!("dev:{}", m.dev());
            }
            probe = p.parent();
        }
    }
    match path.components().next() {
        Some(std::path::Component::Prefix(p)) => p.as_os_str().to_string_lossy().to_uppercase(),
        _ => String::new(),
    }
}

/// Whether two existing paths are the *same bytes on disk* (one inode, two names). Acting on a pair
/// that is already hard-linked would be pure churn, and would "reclaim" space that was never in use,
/// reporting a saving that did not happen.
pub(crate) fn same_physical_file(a: &Path, b: &Path) -> bool {
    match (file_identity(a), file_identity(b)) {
        (Some(x), Some(y)) => x == y,
        // Unknown identity must never be treated as "same file", that would skip a real duplicate.
        _ => false,
    }
}

/// Resolve a file's filesystem identity by path.
///
/// Unix reads it from `stat`. Windows has to open the file and ask
/// `GetFileInformationByHandle`, so this is called only where the answer changes a decision (duplicate
/// group members and reclaim actions), never for every file in the walk.
pub(crate) fn file_identity(path: &Path) -> Option<FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(FileId {
            dev: m.dev(),
            ino: m.ino(),
        })
    }
    #[cfg(windows)]
    {
        windows_identity(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

// --- Windows file identity (volume serial + 64-bit file index) ---------------------------------
//
// Raw FFI rather than a crate dependency, matching how `hw.rs` binds the handful of Win32 calls this
// engine needs. Every failure degrades to `None`, which is the safe direction: an unknown identity
// means "assume they are different files", so a genuine duplicate is never skipped.

#[cfg(windows)]
#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: [u32; 2],
    last_access_time: [u32; 2],
    last_write_time: [u32; 2],
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sec: *mut std::ffi::c_void,
        disposition: u32,
        flags: u32,
        template: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn GetFileInformationByHandle(
        h: *mut std::ffi::c_void,
        info: *mut ByHandleFileInformation,
    ) -> i32;
    fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
}

#[cfg(windows)]
fn windows_identity(path: &Path) -> Option<FileId> {
    use std::os::windows::ffi::OsStrExt;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4; // read | write | delete
    const OPEN_EXISTING: u32 = 3;
    // Lets the handle open a directory as well as a file, and asks for no access rights at all;
    // metadata only, so it cannot disturb anything else holding the file open.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path; the handle is closed on every path below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle as isize == -1 {
        return None;
    }
    let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    Some(FileId {
        dev: u64::from(info.volume_serial_number),
        ino: (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
    })
}

/// Plain BLAKE3 of a whole file, used to re-verify a pair immediately before anything destructive
/// happens to it. Deliberately **not** the scan's partial/size-mixed digest: this answers only
/// "are these two files identical *right now*", which is the question that must be re-asked after a
/// scan that may have finished hours ago.
pub(crate) fn hash_whole(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Concurrent readers to use on the drive backing `path`: one on a spinning disk (parallel reads make
/// an HDD slower, not faster), several on an SSD or when the media is unknown.
fn readers_for(path: &Path) -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    match hw::HwProfile::detect_for(path)
        .work_drive
        .as_ref()
        .and_then(|d| d.ssd)
    {
        Some(false) => 1,
        _ => logical.clamp(1, MAX_READERS_PER_VOLUME),
    }
}

/// BLAKE3 of a file under `mode`. `Ok(None)` means the scan was cancelled mid-file.
///
/// The partial hash mixes in the file size so a short file can never produce the same digest as a
/// longer one whose ends happen to match.
fn hash_file(
    path: &Path,
    size: u64,
    mode: HashMode,
    sink: &dyn ProgressSink,
    stop: &AtomicBool,
) -> std::io::Result<Option<[u8; 32]>> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());

    match mode {
        HashMode::Partial => {
            use std::io::{Seek, SeekFrom};
            let window = PARTIAL_WINDOW.min(size) as usize;
            let mut buf = vec![0u8; window];
            file.read_exact(&mut buf)?;
            hasher.update(&buf);
            sink.on_bytes(window as u64);
            if size > PARTIAL_WINDOW {
                file.seek(SeekFrom::End(-(window as i64)))?;
                file.read_exact(&mut buf)?;
                hasher.update(&buf);
                sink.on_bytes(window as u64);
            }
        }
        HashMode::Full => {
            let mut buf = vec![0u8; READ_BUF];
            loop {
                if stop.load(Ordering::Relaxed) || sink.is_cancelled() {
                    return Ok(None);
                }
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                sink.on_bytes(n as u64);
            }
        }
    }
    Ok(Some(*hasher.finalize().as_bytes()))
}

// Visually-similar images

/// Perceptual matching for images: finds photos that *look* the same without being byte-identical,
/// a resized copy, a re-save at different quality, a screenshot of the same shot.
///
/// **These findings are never actionable.** A perceptual hash cannot tell a redundant re-encode from
/// two genuinely different frames of a burst, and this runs over irreplaceable data, so
/// [`GroupKind::Similar`] groups always carry `reclaimable == 0` and exist purely for human review.
pub mod similar {
    use super::{DupeGroup, GroupKind, ScannedFile};
    use crate::progress::ProgressSink;
    use std::collections::HashMap;

    /// Default Hamming distance.
    ///
    /// Two *unrelated* 64-bit dHashes differ in ~32 bits (half of them, σ ≈ 4), so 8 sits about six
    /// standard deviations below chance, a coincidental match is vanishingly unlikely. It was chosen
    /// by measurement, not taste: at the stricter value of 3 a photo does **not** match its own
    /// resized/re-compressed copy, which is precisely the case this feature exists to catch, while at
    /// 8 it does and genuinely different photos still never group (verified up to [`MAX_DISTANCE`]).
    /// Raising it trades review noise for recall, safely, since similar findings are never actionable.
    pub const DEFAULT_DISTANCE: u32 = 8;
    /// Largest distance the banded index can serve without missing matches (see [`find`]).
    pub const MAX_DISTANCE: u32 = 15;

    /// Extensions decoded for perceptual hashing. HEIC/HEIF (modern iPhone photos) and RAW are absent:
    /// decoding them needs a C library, and adding one would break the pure-Rust build. Those files are
    /// still covered by exact-duplicate detection, which is format-agnostic.
    const IMAGE_EXTS: &[&str] = &[
        "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "ico", "tga", "pnm", "ppm",
    ];

    pub(super) fn is_image(f: &ScannedFile) -> bool {
        f.path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false)
    }

    /// Group visually-similar images among `pool`.
    ///
    /// Comparing every image with every other is quadratic and hopeless at this scale, so candidate
    /// pairs come from a **banded index**: the 64-bit hash is split into `distance + 1` bands, and two
    /// hashes within Hamming distance `d` must agree exactly on at least one band (pigeonhole, `d`
    /// differing bits cannot touch all `d + 1` bands). Only files sharing a band are compared, and the
    /// true distance is then checked. No matches are missed.
    pub fn find(
        pool: &[&ScannedFile],
        distance: u32,
        sink: &dyn ProgressSink,
        unreadable: &mut u64,
    ) -> Vec<DupeGroup> {
        let distance = distance.min(MAX_DISTANCE);
        let images: Vec<&ScannedFile> = pool.iter().copied().filter(|f| is_image(f)).collect();
        if images.len() < 2 {
            return Vec::new();
        }

        // ---- hash every image (decode is the expensive part; parallel across all of them) ----
        let hashes = hash_images(&images, sink, unreadable);
        if hashes.len() < 2 {
            return Vec::new();
        }

        // ---- banded candidate generation ----
        let bands = (distance + 1) as usize;
        let band_bits = 64 / bands;
        let mut index: HashMap<(usize, u64), Vec<usize>> = HashMap::new();
        for (slot, &(_, h)) in hashes.iter().enumerate() {
            for b in 0..bands {
                let shift = b * band_bits;
                let mask = if band_bits >= 64 {
                    u64::MAX
                } else {
                    (1u64 << band_bits) - 1
                };
                index
                    .entry((b, (h >> shift) & mask))
                    .or_default()
                    .push(slot);
            }
        }

        // ---- union-find over confirmed pairs ----
        let mut uf = UnionFind::new(hashes.len());
        for members in index.values() {
            if members.len() < 2 || members.len() > 4096 {
                // A band shared by thousands of images is a degenerate bucket (e.g. flat-colour
                // images); comparing it pairwise would dominate the whole scan for no signal.
                continue;
            }
            for (a_pos, &a) in members.iter().enumerate() {
                for &b in &members[a_pos + 1..] {
                    if (hashes[a].1 ^ hashes[b].1).count_ones() <= distance {
                        uf.union(a, b);
                    }
                }
            }
            if sink.is_cancelled() {
                return Vec::new();
            }
        }

        // ---- materialize groups ----
        let mut by_root: HashMap<usize, Vec<usize>> = HashMap::new();
        for slot in 0..hashes.len() {
            by_root.entry(uf.find(slot)).or_default().push(slot);
        }
        let mut groups: Vec<DupeGroup> = Vec::new();
        for members in by_root.values() {
            if members.len() < 2 {
                continue;
            }
            let mut files: Vec<ScannedFile> =
                members.iter().map(|&s| hashes[s].0.clone()).collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            groups.push(DupeGroup {
                kind: GroupKind::Similar,
                files,
                // Never reclaimable: similar is not interchangeable.
                reclaimable: 0,
            });
        }
        groups
    }

    /// Decode + perceptually hash each image. Returns the ones that decoded, paired with their hash.
    #[cfg(feature = "phash")]
    fn hash_images(
        images: &[&ScannedFile],
        sink: &dyn ProgressSink,
        unreadable: &mut u64,
    ) -> Vec<(ScannedFile, u64)> {
        use rayon::prelude::*;
        let failed = std::sync::atomic::AtomicU64::new(0);
        let out: Vec<(ScannedFile, u64)> = images
            .par_iter()
            .filter_map(|f| {
                if sink.is_cancelled() {
                    return None;
                }
                match dhash(&f.path) {
                    Some(h) => {
                        sink.on_bytes(f.size);
                        Some(((*f).clone(), h))
                    }
                    None => {
                        failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        None
                    }
                }
            })
            .collect();
        *unreadable += failed.load(std::sync::atomic::Ordering::Relaxed);
        out
    }

    #[cfg(not(feature = "phash"))]
    fn hash_images(
        _images: &[&ScannedFile],
        _sink: &dyn ProgressSink,
        _unreadable: &mut u64,
    ) -> Vec<(ScannedFile, u64)> {
        Vec::new()
    }

    /// **dHash**: downscale to 9×8 greyscale and emit one bit per horizontal neighbour pair,
    /// "is this pixel brighter than the one to its right". Encoding *gradients* rather than absolute
    /// values is what makes it survive re-encoding, resizing and brightness shifts while still
    /// separating genuinely different pictures.
    #[cfg(feature = "phash")]
    fn dhash(path: &std::path::Path) -> Option<u64> {
        let img = image::open(path).ok()?;
        let small = image::imageops::resize(
            &image::imageops::grayscale(&img),
            9,
            8,
            image::imageops::FilterType::Triangle,
        );
        let mut bits = 0u64;
        for y in 0..8u32 {
            for x in 0..8u32 {
                let l = small.get_pixel(x, y).0[0];
                let r = small.get_pixel(x + 1, y).0[0];
                bits = (bits << 1) | u64::from(l > r);
            }
        }
        Some(bits)
    }

    /// Disjoint-set over hash slots, so a chain of pairwise-similar images becomes one group.
    struct UnionFind {
        parent: Vec<usize>,
    }
    impl UnionFind {
        fn new(n: usize) -> Self {
            Self {
                parent: (0..n).collect(),
            }
        }
        fn find(&mut self, mut x: usize) -> usize {
            while self.parent[x] != x {
                self.parent[x] = self.parent[self.parent[x]]; // path halving
                x = self.parent[x];
            }
            x
        }
        fn union(&mut self, a: usize, b: usize) {
            let (ra, rb) = (self.find(a), self.find(b));
            if ra != rb {
                self.parent[ra] = rb;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Progress;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cram-dedup-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn write(path: &Path, bytes: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        File::create(path).unwrap().write_all(bytes).unwrap();
    }

    #[test]
    fn finds_identical_files_under_different_names_in_different_folders() {
        let dir = scratch("basic");
        // The real-world shape: one photo, three random names, three folders.
        let photo = vec![0xC3u8; 300 * 1024];
        write(&dir.join("a/IMG_0001.jpg"), &photo);
        write(&dir.join("b/8f3a91cc.jpg"), &photo);
        write(&dir.join("c/nested/copy of photo.jpg"), &photo);
        // A same-size file with different content must NOT be grouped with them.
        let mut other = vec![0xC3u8; 300 * 1024];
        other[299 * 1024] = 0x00;
        write(&dir.join("b/different.jpg"), &other);
        // A unique file is never even read.
        write(&dir.join("unique.txt"), b"solo");

        let sink = Progress::new(0, 0);
        let rep = scan(std::slice::from_ref(&dir), &DedupOptions::default(), &sink).unwrap();

        let exact: Vec<_> = rep.exact_groups().collect();
        assert_eq!(exact.len(), 1, "one duplicate set");
        assert_eq!(exact[0].files.len(), 3, "three copies found");
        assert_eq!(
            exact[0].reclaimable,
            2 * 300 * 1024,
            "two of the three copies are recoverable"
        );
        assert_eq!(rep.redundant_files(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_size_different_content_is_not_a_duplicate() {
        // Guards the gate that matters most for safety: size alone must never imply duplicate. These
        // differ only in the LAST byte, so they also survive the first-window partial hash and are
        // separated only by reading further, exactly the case a naive "head hash" tool gets wrong.
        let dir = scratch("collide");
        let n = 1024 * 1024;
        let mut a = vec![7u8; n];
        let mut b = vec![7u8; n];
        a[n - 1] = 1;
        b[n - 1] = 2;
        write(&dir.join("a.bin"), &a);
        write(&dir.join("b.bin"), &b);

        let sink = Progress::new(0, 0);
        let rep = scan(std::slice::from_ref(&dir), &DedupOptions::default(), &sink).unwrap();
        assert_eq!(
            rep.exact_groups().count(),
            0,
            "must not group different bytes"
        );
        assert_eq!(rep.reclaimable(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_sizes_are_never_read() {
        // The scale claim: files with a one-of-a-kind size cost zero bytes of I/O.
        let dir = scratch("sizegate");
        for i in 1..=6u8 {
            write(&dir.join(format!("f{i}.bin")), &vec![i; 1000 + i as usize]);
        }
        let sink = Progress::new(0, 0);
        let rep = scan(std::slice::from_ref(&dir), &DedupOptions::default(), &sink).unwrap();
        assert_eq!(rep.files_scanned, 6);
        assert_eq!(rep.bytes_hashed, 0, "no file should have been read");
        assert_eq!(rep.groups.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn already_hardlinked_copies_reclaim_nothing() {
        // Two names, one file. The group is still worth showing, but claiming space back from it
        // would be a lie, and would make a second run of `--link --apply` report a saving it did not
        // make. This is what makes the whole operation idempotent.
        let dir = scratch("hardlink");
        let blob = vec![0x4Du8; 300 * 1024];
        let a = dir.join("a.bin");
        write(&a, &blob);
        if std::fs::hard_link(&a, dir.join("b.bin")).is_err() {
            return; // filesystem without hard links: nothing to assert
        }

        let sink = Progress::new(0, 0);
        let rep = scan(std::slice::from_ref(&dir), &DedupOptions::default(), &sink).unwrap();
        let g: Vec<_> = rep.exact_groups().collect();
        assert_eq!(g.len(), 1, "both names are found");
        assert_eq!(g[0].files.len(), 2);
        assert_eq!(g[0].reclaimable, 0, "one physical file frees nothing");
        assert_eq!(rep.reclaimable(), 0);
        assert_eq!(rep.redundant_files(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_files_are_ignored_by_default() {
        let dir = scratch("empty");
        write(&dir.join("e1"), b"");
        write(&dir.join("e2"), b"");
        let sink = Progress::new(0, 0);
        let rep = scan(std::slice::from_ref(&dir), &DedupOptions::default(), &sink).unwrap();
        assert_eq!(rep.files_scanned, 0);
        assert_eq!(rep.groups.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a photo-like image: a gradient sky over ground with scattered blobs. Structure matters,
    /// a flat colour would hash identically to every other flat colour and prove nothing.
    #[cfg(feature = "phash")]
    fn synthetic_photo(seed: u32, w: u32, h: u32) -> image::RgbImage {
        let mut img = image::RgbImage::new(w, h);
        let mut rng = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        for y in 0..h {
            for x in 0..w {
                let t = y * 255 / h.max(1);
                let px = if y > h * 2 / 3 {
                    image::Rgb([48, 92, 40])
                } else {
                    image::Rgb([(60 + t / 2) as u8, (110 + t / 3) as u8, (200 - t / 4) as u8])
                };
                img.put_pixel(x, y, px);
            }
        }
        for _ in 0..24 {
            let cx = next() % w;
            let cy = h / 2 + next() % (h / 2).max(1);
            let r = 8 + next() % 30;
            let col = image::Rgb([
                (next() % 90) as u8,
                (60 + next() % 80) as u8,
                (next() % 70) as u8,
            ]);
            for y in cy.saturating_sub(r)..(cy + r).min(h) {
                for x in cx.saturating_sub(r)..(cx + r).min(w) {
                    let (dx, dy) = (x as i64 - cx as i64, y as i64 - cy as i64);
                    if dx * dx + dy * dy <= (r * r) as i64 {
                        img.put_pixel(x, y, col);
                    }
                }
            }
        }
        img
    }

    /// The two claims the perceptual feature lives or dies by: a photo matches its own resized copy
    /// (recall), and two genuinely different photos never match (no false positives; the dangerous
    /// direction, since a false pair invites a human to delete a photo that isn't a duplicate).
    #[cfg(feature = "phash")]
    #[test]
    fn similar_finds_resized_copy_but_not_different_photos() {
        let dir = scratch("similar");
        let photo = synthetic_photo(1, 640, 480);
        photo.save(dir.join("original.png")).unwrap();
        // Same photo, half size; byte-wise unrelated, visually the same.
        image::imageops::resize(&photo, 320, 240, image::imageops::FilterType::Triangle)
            .save(dir.join("resized.png"))
            .unwrap();
        // A different scene entirely.
        synthetic_photo(99, 640, 480)
            .save(dir.join("other.png"))
            .unwrap();

        let sink = Progress::new(0, 0);
        let opts = DedupOptions {
            similar_images: true,
            ..Default::default()
        };
        let rep = scan(std::slice::from_ref(&dir), &opts, &sink).unwrap();

        let groups: Vec<_> = rep.similar_groups().collect();
        assert_eq!(groups.len(), 1, "exactly one similar set");
        let names: Vec<String> = groups[0]
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"original.png".to_string()));
        assert!(names.contains(&"resized.png".to_string()));
        assert!(
            !names.contains(&"other.png".to_string()),
            "a different photo must never be grouped as similar"
        );
        // The safety invariant: similar findings never claim recoverable space.
        assert_eq!(groups[0].reclaimable, 0);
        assert_eq!(rep.reclaimable(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn similar_groups_are_never_reclaimable() {
        // The safety invariant the whole perceptual feature rests on: a Similar group must never
        // contribute space that an action could try to reclaim.
        let g = DupeGroup {
            kind: GroupKind::Similar,
            files: vec![],
            reclaimable: 0,
        };
        assert_eq!(g.reclaimable, 0);
        let rep = DedupReport {
            groups: vec![g],
            ..Default::default()
        };
        assert_eq!(rep.reclaimable(), 0);
        assert_eq!(rep.exact_groups().count(), 0);
    }
}
