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

/// Depth past which the walk starts checking for a directory cycle. Chosen so no genuine tree pays
/// for it: the deepest real tree measured on a developer machine here was 13 levels, and even a
/// pathological build tree is two orders of magnitude short of this.
const CYCLE_GUARD_DEPTH: u32 = 1_000;

/// How often a running walk reports what it has found. Long enough that a whole-drive scan does not
/// flood the UI, short enough that the thing never looks hung -- which is what it did look like.
const SCAN_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

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
    let d = crate::diag::diag();
    d.checkpoint_begin("dedup scan");
    d.op(
        "command",
        format!(
            "dedup {} root(s), min_size={}, similar_images={}",
            roots.len(),
            opts.min_size,
            opts.similar_images
        ),
    );

    // ---- 1. Walk ------------------------------------------------------------------------------
    // The longest silent stretch of the whole operation: on a whole drive this is minutes during
    // which nothing is reported, because there is no total to report progress against until it is
    // done. That silence is exactly what made a crash in here impossible to place.
    d.checkpoint_phase("walk (finding files)");
    let mut files: Vec<ScannedFile> = Vec::new();
    for root in collapse_roots(roots) {
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
    d.metric("files found", report.files_scanned.to_string());
    d.metric("bytes found", report.bytes_scanned.to_string());
    d.metric("unreadable", report.unreadable.to_string());

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

    d.metric("size-gate candidates", candidates.len().to_string());

    // ---- 3. Partial-hash gate -----------------------------------------------------------------
    // Only worth doing for files big enough that 128 KiB is meaningfully cheaper than the whole file.
    let (big, small): (Vec<usize>, Vec<usize>) = candidates
        .iter()
        .partition(|&&i| files[i].size >= PARTIAL_MIN_SIZE);
    if !big.is_empty() {
        d.checkpoint_phase("partial hash");
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
    d.checkpoint_phase("full hash");
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
        d.checkpoint_phase("perceptual hash (similar images)");
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
    d.metric("duplicate groups", report.groups.len().to_string());
    d.metric("reclaimable bytes", report.reclaimable().to_string());
    // Reached only by getting all the way here. Everything else -- an error return, a panic, a
    // process that dies outright -- leaves the checkpoint on disk, which is what makes its presence
    // evidence rather than noise.
    d.checkpoint_end();
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

/// Drop every root that another root already covers, comparing canonical paths so `.`, `..` and a
/// relative spelling all resolve to the same place. `cram dedup D:\photos D:\photos\2019` otherwise
/// walks the inner folder twice: the same file lands in its own duplicate group under one identical
/// path, and the reclaim plan then contains the same action twice.
///
/// The kept roots are the caller's originals, not the canonical forms; walking a canonicalized path
/// on Windows would print every result with a `\\?\` prefix.
fn collapse_roots(roots: &[PathBuf]) -> Vec<&PathBuf> {
    let canon: Vec<PathBuf> = roots
        .iter()
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone()))
        .collect();
    roots
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            !canon.iter().enumerate().any(|(j, other)| {
                // Equal roots keep the first spelling; a nested root loses to its ancestor.
                j != *i && canon[*i].starts_with(other) && (canon[*i] != *other || j < *i)
            })
        })
        .map(|(_, r)| r)
        .collect()
}

/// The scratch names [`super::reclaim`] renames through while it swaps a duplicate for a hard link.
/// A crash between those two renames leaves the victim under one of them; it is a real file, but it
/// is the wreckage of an interrupted swap rather than something to plan a second swap for, so the
/// walk leaves it where it is.
fn is_reclaim_scratch(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) if n.starts_with('.') => n,
        _ => return false,
    };
    let Some(pos) = name.rfind(".cram-") else {
        return false;
    };
    match name[pos + ".cram-".len()..].rsplit_once('-') {
        Some((tag, pid)) => {
            matches!(tag, "link" | "old")
                && !pid.is_empty()
                && pid.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Collect regular files at or above `min_size`. Symlinks are never followed, that would invent
/// "duplicates" that are really one file, and could loop forever. Unreadable directories are
/// counted and skipped rather than aborting a scan that may span many drives.
///
/// The descent uses an explicit stack rather than recursion, because recursion put a hard ceiling
/// on how deep a tree could be scanned at all. Each recursive frame cost 3,264 bytes here (eight
/// register saves plus a `sub $0xc78,%rsp` prologue, measured on the shipped binary), so a scan
/// running on a 2 MiB worker thread died at roughly 640 levels. It died badly: a stack overflow on
/// Windows is a hardware exception, not a Rust panic, so nothing unwinds, the panic hook never
/// runs, no diagnostic is written and the process simply vanishes. Depth is now bounded by the heap
/// instead, which no real tree reaches.
fn walk(
    root: &Path,
    min_size: u64,
    out: &mut Vec<ScannedFile>,
    unreadable: &mut u64,
    sink: &dyn ProgressSink,
) -> Result<()> {
    // Depth rides along only to arm the cycle guard below; the descent itself does not care.
    let mut stack: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 0)];
    let mut seen_deep: std::collections::HashSet<FileId> = std::collections::HashSet::new();
    let mut dirs: u64 = 0;
    let mut last_report = std::time::Instant::now();
    while let Some((path, depth)) = stack.pop() {
        if sink.is_cancelled() {
            return Err(ArchiveError::Cancelled);
        }
        // Rate-limited to once a second inside, and `out.len()` is cumulative across roots, so this
        // is a relaxed atomic load per directory in the common case.
        crate::diag::diag().checkpoint_tick(out.len() as u64, Some(&path));
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                *unreadable += 1;
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            // Past this depth a tree is no longer plausibly a tree, so start paying for a cycle
            // check. Skipping symlinks and mount points covers every reparse tag std classifies as
            // a link, but a directory carrying any OTHER tag is not a link to std and is descended
            // into -- and since the walk stopped being able to overflow the stack, such a cycle
            // would run forever and grow the heap instead, which is quieter than what it replaced.
            //
            // The identity lookup costs an open per directory, so it is armed by depth rather than
            // paid on every scan: a normal tree never reaches it and is charged nothing.
            if depth > CYCLE_GUARD_DEPTH {
                match file_identity(&path) {
                    Some(id) if !seen_deep.insert(id) => continue, // already walked; a cycle
                    _ => {}
                }
            }
            let entries = match std::fs::read_dir(&path) {
                Ok(e) => e,
                Err(_) => {
                    *unreadable += 1;
                    continue;
                }
            };
            dirs += 1;
            // Children are pushed in reverse so they pop in `read_dir` order, leaving the traversal
            // order identical to the recursion this replaces.
            let mut children: Vec<(PathBuf, u32)> = entries
                .flatten()
                .map(|e| (e.path(), depth.saturating_add(1)))
                .collect();
            children.reverse();
            stack.append(&mut children);
            // The walk has no total to report a fraction against -- that is what it is computing --
            // so it reports what it has found. Rate-limited because a whole-drive scan visits
            // hundreds of thousands of directories and each call crosses into the UI.
            if last_report.elapsed() >= SCAN_REPORT_INTERVAL {
                last_report = std::time::Instant::now();
                sink.on_scan_progress(out.len() as u64, dirs);
            }
        } else if meta.is_file() && meta.len() >= min_size.max(1) && !is_reclaim_scratch(&path) {
            out.push(ScannedFile {
                size: meta.len(),
                id: file_id(&meta),
                path,
            });
        }
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

    // One OS thread per volume so separate drives read at the same time; inside a volume, a
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
/// two different frames of a burst, and this runs over irreplaceable data, so
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
    /// resized/re-compressed copy, which is exactly the case this feature exists to catch, while at
    /// 8 it does and different photos still never group (verified up to [`MAX_DISTANCE`]).
    /// Raising it trades review noise for recall, safely, since similar findings are never actionable.
    pub const DEFAULT_DISTANCE: u32 = 8;
    /// Largest distance the banded index can serve without missing matches (see [`find`]).
    pub const MAX_DISTANCE: u32 = 15;

    /// Side of the square colour fingerprint used to *confirm* a candidate pair.
    ///
    /// The dHash is a 9×8 greyscale gradient — 72 pixels — which is all a candidate stage needs and
    /// nowhere near enough to decide. A terminal screenshot at that size is a dark rectangle: a
    /// missing word changes no bits at all, so a thousand unrelated CLI captures hash alike and, via
    /// the transitive union below, collapse into one group. 64×64 in colour is where "the same shot
    /// retaken" and "a different shot entirely" stop looking the same.
    #[cfg(feature = "phash")]
    const VERIFY_DIM: u32 = 64;

    /// Mean absolute channel difference, 0..1, above which a candidate pair is rejected.
    ///
    /// Chosen by measurement, not taste. Over synthetic terminal captures and photos, at 64x64:
    ///
    /// | must stay together      |        | must separate            |        |
    /// |-------------------------|--------|--------------------------|--------|
    /// | photo, resized to half  | 0.0002 | two different terminals  | 0.0132 |
    /// | terminal retake, 1 word | 0.0009 | two different photos     | 0.0341 |
    /// | photo, half + JPEG q70  | 0.0029 |                          |        |
    /// | photo, half + JPEG q40  | 0.0037 |                          |        |
    ///
    /// The lower bound is set by lossy re-encoding, not by the retakes: a resized JPEG of the same
    /// photo is the case this feature exists to catch, and it is far noisier than two captures of
    /// one screen. 0.007 is the geometric mean of the worst keep (0.0037) and the closest separate
    /// (0.0132), so it sits ~1.9x from each.
    ///
    /// That margin is real but not generous, and these are synthetic images. A corpus that pushes
    /// past it will show up as a group that should have split (raise it) or a retake that was missed
    /// (lower it). This constant is the knob.
    const VERIFY_MAX_DIFF: f32 = 0.007;

    /// Aspect ratios differing by more than this are never the same picture. Cheap, and it separates
    /// terminal captures taken at different window sizes before any pixel is compared.
    const VERIFY_ASPECT_TOL: f32 = 0.10;

    /// A small colour thumbnail, kept only long enough to confirm the pairs the hash proposed.
    ///
    /// Colour on purpose. Greyscale is right for the *hash*, where discarding chroma is what makes a
    /// re-encoded copy still match — JPEG subsamples chroma, profiles come and go, hues drift. None
    /// of that applies here: this compares two decoded images directly, and screenshots are lossless
    /// PNG. Meanwhile terminal output is full of colour signal, and Rec. 601 luma will happily map a
    /// red error dump and a green success run onto the same greys.
    struct Fingerprint {
        aspect: f32,
        px: Vec<u8>, // VERIFY_DIM * VERIFY_DIM * 3, RGB
    }

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

        // ---- candidate pairs, from the hash alone ----
        // Collected rather than unioned on sight. The hash is a filter, not a verdict: it decides
        // what is worth looking at, and the pixels decide what actually matches.
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for members in index.values() {
            if members.len() < 2 || members.len() > 4096 {
                // A band shared by thousands of images is a degenerate bucket (e.g. flat-colour
                // images); comparing it pairwise would dominate the whole scan for no signal.
                continue;
            }
            for (a_pos, &a) in members.iter().enumerate() {
                for &b in &members[a_pos + 1..] {
                    if (hashes[a].1 ^ hashes[b].1).count_ones() <= distance {
                        pairs.push(if a < b { (a, b) } else { (b, a) });
                    }
                }
            }
            if sink.is_cancelled() {
                return Vec::new();
            }
        }
        // The banded index offers the same pair once per band it shares.
        pairs.sort_unstable();
        pairs.dedup();

        // ---- confirm each pair against the actual pixels ----
        // Only images that reached a candidate pair are decoded again, so this is bounded by what
        // the hash proposed rather than by the size of the scan.
        let mut needed: Vec<usize> = pairs.iter().flat_map(|&(a, b)| [a, b]).collect();
        needed.sort_unstable();
        needed.dedup();
        let prints = fingerprints(&needed, &hashes, sink);

        let mut uf = UnionFind::new(hashes.len());
        for (a, b) in pairs {
            match (prints.get(&a), prints.get(&b)) {
                // Unreadable a second time: fall back to the hash's opinion rather than silently
                // dropping a finding the user would otherwise have seen.
                (Some(x), Some(y)) if !verify(x, y) => {}
                _ => uf.union(a, b),
            }
        }
        if sink.is_cancelled() {
            return Vec::new();
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
    /// Decode the candidates to fingerprints, in parallel, exactly as the hash pass does.
    #[cfg(feature = "phash")]
    fn fingerprints(
        needed: &[usize],
        hashes: &[(ScannedFile, u64)],
        sink: &dyn ProgressSink,
    ) -> HashMap<usize, Fingerprint> {
        use rayon::prelude::*;
        needed
            .par_iter()
            .filter_map(|&slot| {
                if sink.is_cancelled() {
                    return None;
                }
                fingerprint(&hashes[slot].0.path).map(|f| (slot, f))
            })
            .collect()
    }

    /// Without `phash` nothing is ever hashed, so nothing is ever a candidate and this is never
    /// reached. It exists so the crate still builds without the feature, exactly like `hash_images`.
    #[cfg(not(feature = "phash"))]
    fn fingerprints(
        _needed: &[usize],
        _hashes: &[(ScannedFile, u64)],
        _sink: &dyn ProgressSink,
    ) -> HashMap<usize, Fingerprint> {
        HashMap::new()
    }

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

    /// Decode an image down to a small colour square for comparison. Squashed to a fixed square
    /// rather than fitted: both sides of a comparison are distorted identically, so it costs
    /// nothing, and the aspect ratio is kept separately where it does real work.
    #[cfg(feature = "phash")]
    fn fingerprint(path: &std::path::Path) -> Option<Fingerprint> {
        let img = image::open(path).ok()?;
        let (w, h) = (img.width().max(1), img.height().max(1));
        let small = image::imageops::resize(
            &img.to_rgb8(),
            VERIFY_DIM,
            VERIFY_DIM,
            image::imageops::FilterType::Triangle,
        );
        Some(Fingerprint {
            aspect: w as f32 / h as f32,
            px: small.into_raw(),
        })
    }

    /// Is this candidate pair actually the same picture?
    ///
    /// The hash said "maybe"; this says yes or no. Mean absolute difference over every channel,
    /// after an aspect-ratio gate that rejects the easy cases without touching a pixel.
    fn verify(a: &Fingerprint, b: &Fingerprint) -> bool {
        // Relative difference, so the tolerance means the same thing for a 4:3 shot and a 21:9 one.
        let (lo, hi) = if a.aspect < b.aspect {
            (a.aspect, b.aspect)
        } else {
            (b.aspect, a.aspect)
        };
        if lo <= 0.0 || (hi - lo) / lo > VERIFY_ASPECT_TOL {
            return false;
        }
        if a.px.len() != b.px.len() || a.px.is_empty() {
            return false;
        }
        let sum: u64 =
            a.px.iter()
                .zip(b.px.iter())
                .map(|(x, y)| u64::from(x.abs_diff(*y)))
                .sum();
        let mean = sum as f32 / (a.px.len() as f32 * 255.0);
        mean <= VERIFY_MAX_DIFF
    }

    /// **dHash**: downscale to 9×8 greyscale and emit one bit per horizontal neighbour pair,
    /// "is this pixel brighter than the one to its right". Encoding *gradients* rather than absolute
    /// values is what makes it survive re-encoding, resizing and brightness shifts while still
    /// separating different pictures.
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
    fn a_root_nested_inside_another_root_is_walked_once() {
        // `cram dedup D:\photos D:\photos\2019`. Walking the inner folder twice puts one path into
        // its own duplicate group twice, and the reclaim plan then holds the same action twice.
        let dir = scratch("overlap");
        let blob = vec![0x5Au8; 200 * 1024];
        write(&dir.join("k.bin"), &blob);
        write(&dir.join("sub/dup.bin"), &blob);

        let sink = Progress::new(0, 0);
        let roots = vec![dir.clone(), dir.join("sub")];
        let rep = scan(&roots, &DedupOptions::default(), &sink).unwrap();
        assert_eq!(rep.files_scanned, 2, "two files, not three");
        let g: Vec<_> = rep.exact_groups().collect();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].files.len(), 2, "each path listed once");
        assert_eq!(g[0].reclaimable, 200 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Scanning a deep tree must not cost stack in proportion to its depth.
    ///
    /// The walk used to recurse at 3,264 bytes of frame per level, so a scan on a 2 MiB worker
    /// thread died at roughly 640 directories down. It died as a stack overflow, which on Windows
    /// is a hardware exception rather than a Rust panic: nothing unwinds, the panic hook never
    /// runs, no diagnostic is written, and the process simply disappears. A real 14,566-level tree
    /// on a test machine is what found it.
    ///
    /// Depth is pinned indirectly, by giving the scan a deliberately small stack rather than a
    /// 640-deep tree: macOS caps a path at 1024 bytes, so a tree deep enough to overflow a normal
    /// stack cannot even be created there. 400 recursive frames would need 1.3 MB against the
    /// 512 KiB below, while an iterative descent is flat in depth and fits whatever it is given.
    ///
    /// If recursion is ever reintroduced this test will not fail politely: it will overflow and
    /// take the test binary down with it. That is still an unmistakable signal, and a guard-page
    /// hit cannot be caught in-process to make it tidier.
    #[test]
    fn a_deep_tree_does_not_cost_stack() {
        const DEPTH: usize = 400;
        const STACK: usize = 512 * 1024;

        let dir = scratch("deep");
        let mut deep = dir.clone();
        for _ in 0..DEPTH {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        // Two identical files at the bottom, so the scan has a finding to bring back up.
        let body = vec![0x5Au8; 8 * 1024];
        write(&deep.join("one.bin"), &body);
        write(&deep.join("two.bin"), &body);

        let root = dir.clone();
        let found = std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || {
                let sink = Progress::new(0, 0);
                let rep = scan(&[root], &DedupOptions::default(), &sink).unwrap();
                rep.exact_groups().map(|g| g.files.len()).sum::<usize>()
            })
            .unwrap()
            .join()
            .unwrap();

        assert_eq!(found, 2, "both copies {DEPTH} directories down");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Past `CYCLE_GUARD_DEPTH` the walk starts resolving each directory's filesystem identity to
    /// notice a cycle. A false positive there would silently skip real directories, which is the
    /// failure this whole area keeps producing, so the guard is checked against a legitimately deep
    /// tree: every distinct directory must still be walked and the file at the bottom still found.
    ///
    /// Not run on macOS, where `PATH_MAX` is 1024 and a tree deep enough to arm the guard cannot be
    /// addressed by absolute path at all.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_cycle_guard_does_not_reject_a_legitimately_deep_tree() {
        let depth = (CYCLE_GUARD_DEPTH + 200) as usize;
        let dir = scratch("cycleguard");
        let mut deep = dir.clone();
        for _ in 0..depth {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        let body = vec![0x77u8; 4096];
        write(&deep.join("one.bin"), &body);
        write(&deep.join("two.bin"), &body);

        let sink = Progress::new(0, 0);
        let rep = scan(std::slice::from_ref(&dir), &DedupOptions::default(), &sink).unwrap();
        assert_eq!(
            rep.exact_groups().map(|g| g.files.len()).sum::<usize>(),
            2,
            "both copies {depth} levels down, with the guard armed for the last 200"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wreckage_of_an_interrupted_link_swap_is_left_alone() {
        let dir = scratch("orphan");
        let blob = vec![0x1Fu8; 100 * 1024];
        write(&dir.join("k.bin"), &blob);
        // What a crash between reclaim's two renames leaves behind.
        write(&dir.join(".d.bin.cram-old-1234"), &blob);
        // A file that merely looks similar is an ordinary file and must still be scanned.
        write(&dir.join("notes.cram-old-1234"), &blob);

        let sink = Progress::new(0, 0);
        let rep = scan(std::slice::from_ref(&dir), &DedupOptions::default(), &sink).unwrap();
        assert_eq!(rep.files_scanned, 2, "the orphan is not scanned");
        let g: Vec<_> = rep.exact_groups().collect();
        assert_eq!(g.len(), 1);
        assert!(
            !g[0].files.iter().any(|f| f
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with('.')),
            "an interrupted swap must never be planned for another one"
        );
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
    /// (recall), and two different photos never match (no false positives; the dangerous
    /// direction, since a false pair invites a human to delete a photo that isn't a duplicate).
    /// A synthetic terminal capture: dark background, a few rows of light "text" blocks.
    #[cfg(feature = "phash")]
    fn synthetic_terminal(seed: u32, words: usize) -> image::RgbImage {
        let (w, h) = (960u32, 540u32);
        let mut img = image::RgbImage::from_pixel(w, h, image::Rgb([12, 12, 14]));
        let mut rng = seed.wrapping_mul(2654435761).wrapping_add(1);
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        // Rows of short bright runs, the shape of a wall of monospaced output.
        for i in 0..words {
            let row = (i / 8) as u32;
            let y = 20 + row * 18;
            if y + 9 >= h {
                break;
            }
            let x = 16 + ((i % 8) as u32) * 112 + (next() % 12);
            let len = 40 + (next() % 60);
            let shade = 170 + (next() % 70) as u8;
            for dy in 0..9u32 {
                for dx in 0..len.min(w - x - 1) {
                    img.put_pixel(x + dx, y + dy, image::Rgb([shade, shade, shade]));
                }
            }
        }
        img
    }

    /// The case this stage exists for. A wall of terminal output hashes to almost nothing at 9×8 --
    /// it is a dark rectangle -- so every CLI screenshot on a disk lands in one candidate bucket and
    /// the transitive union then welds them into a single group of hundreds. Confirming candidates
    /// against the actual pixels is what tells "the same shot, retaken" from "a different shot".
    #[cfg(feature = "phash")]
    #[test]
    fn two_different_terminal_captures_do_not_group() {
        let dir = scratch("terminal");
        // Same *kind* of image, different content: the real corpus, in miniature.
        let a = dir.join("run-a.png");
        let b = dir.join("run-b.png");
        synthetic_terminal(1, 40).save(&a).unwrap();
        synthetic_terminal(2, 40).save(&b).unwrap();

        // And a genuine re-take: the same capture with one "word" missing, which is exactly the
        // pair a user wants found.
        let c = dir.join("retake-full.png");
        let d = dir.join("retake-cut.png");
        synthetic_terminal(3, 40).save(&c).unwrap();
        synthetic_terminal(3, 39).save(&d).unwrap();

        let sink = Progress::new(0, 0);
        let opts = DedupOptions {
            similar_images: true,
            ..Default::default()
        };
        let rep = scan(std::slice::from_ref(&dir), &opts, &sink).unwrap();
        let groups: Vec<Vec<String>> = rep
            .similar_groups()
            .map(|g| {
                let mut v: Vec<String> = g
                    .files
                    .iter()
                    .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                v.sort();
                v
            })
            .collect();

        let together = |x: &str, y: &str| {
            groups
                .iter()
                .any(|g| g.iter().any(|n| n == x) && g.iter().any(|n| n == y))
        };

        assert!(
            !together("run-a.png", "run-b.png"),
            "two different terminal captures must not be called similar; groups: {groups:?}"
        );
        assert!(
            together("retake-full.png", "retake-cut.png"),
            "a retake of the same screen, one word short, is exactly what should be found; groups: {groups:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
