//! The archive-creation path: walk the on-disk sources into an entry list, then stream each into an
//! `ArchiveWriter`. The container's writer owns the compression/encryption/central-directory
//! mechanics; this engine owns the tree walk, per-entry file I/O, progress, and cancellation.
//!
//! Naming mirrors the CLI intuition: a top-level input keeps its own base name as the archive root
//! (`cramc a out.zip pics` → `pics/…`, `cramc a out.zip note.txt` → `note.txt`). Directories are
//! emitted before their children so empty dirs are preserved.

use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::time::SystemTime;

use crate::error::{ArchiveError, Result};
use crate::format::{Codec, Container, Format};
use crate::formats;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::probe::{self, ProbeSummary};
use crate::progress::{CountingReader, ProgressSink};
use crate::writer::{CreateOptions, CreateReport, Level, WriteHint};

/// One planned archive member: its metadata entry plus, for files, the disk path to stream from.
struct CreateItem {
    entry: Entry,
    /// `Some` for files (source to read), `None` for directory entries.
    disk_path: Option<PathBuf>,
    /// The adaptive probe's per-entry decision (filled in a pre-pass; default for dirs).
    hint: WriteHint,
}

/// Build a cram [`Entry`] for an archive member; `None` if the (generated) name is somehow unsafe.
/// `modified` is the source's on-disk mtime, carried into the classic containers (tar/zip/7z) so
/// extraction can restore it. `.cram` ignores it (the frozen format stores no timestamps).
fn make_entry(
    archive_name: &str,
    size: u64,
    is_dir: bool,
    modified: Option<SystemTime>,
) -> Option<Entry> {
    EntryPath::from_raw(archive_name).map(|path| Entry {
        index: 0,
        path,
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

/// One thing still to walk: a directory to descend into, or a file whose metadata has already been
/// taken from the directory entry that named it.
enum Pending {
    Dir(PathBuf, String),
    File(PathBuf, String, fs::Metadata),
}

/// Walk a directory, emitting its own entry first, then its children under `prefix`.
///
/// The descent uses an explicit stack. It recursed until a deep tree was found to kill the process
/// outright: a stack overflow on Windows is a hardware exception rather than a Rust panic, so
/// nothing unwinds, no diagnostic is written, and `cram a` simply vanishes mid-archive. See the
/// note on [`super::dedup::walk`], where the same defect was measured at 3,264 bytes of frame per
/// level, giving out around 640 directories down.
///
/// Emission order is unchanged, and deliberately so, because it is the archive's layout: each
/// directory's own entry, then its children sorted by name, with a subdirectory's whole subtree
/// emitted before the next sibling. Children are pushed in reverse so they pop back in sorted
/// order, and a file carries the `Metadata` already read from its directory entry so the walk costs
/// no more syscalls than the recursion did.
fn collect_dir(
    dir: &Path,
    prefix: &str,
    items: &mut Vec<CreateItem>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    let mut stack: Vec<Pending> = vec![Pending::Dir(dir.to_path_buf(), prefix.to_string())];
    while let Some(next) = stack.pop() {
        let (path, name) = match next {
            Pending::File(path, name, md) => {
                let size = md.len();
                if let Some(entry) = make_entry(&name, size, false, md.modified().ok()) {
                    items.push(CreateItem {
                        entry,
                        disk_path: Some(path),
                        hint: WriteHint::default(),
                    });
                }
                continue;
            }
            Pending::Dir(path, name) => (path, name),
        };

        let dir_mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        if let Some(entry) = make_entry(&name, 0, true, dir_mtime) {
            items.push(CreateItem {
                entry,
                disk_path: None,
                hint: WriteHint::default(),
            });
        }
        // Sort children for a deterministic archive layout.
        let mut children: Vec<_> = fs::read_dir(&path)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|e| e.file_name());
        let mut pending: Vec<Pending> = Vec::with_capacity(children.len());
        for child in children {
            // Refuse a name that isn't UTF-8 rather than lossily replacing the undecodable units:
            // `a\u{D800}.txt` and `a\u{D801}.txt` both became `a\u{FFFD}.txt`, so tar and `.cram` wrote
            // two entries under one name and extraction destroyed the first file, with `cram a` and
            // `cram x` both reporting success. `collect_input` below already refuses such a name one
            // level up; this only makes the walk agree with it.
            let file_name = child.file_name();
            let Some(child_base) = file_name.to_str() else {
                return Err(ArchiveError::Backend(format!(
                    "{}: file name is not valid UTF-8, archive entry names must be",
                    child.path().display()
                )));
            };
            let child_name = format!("{name}/{child_base}");
            let ft = child.file_type()?;
            if ft.is_symlink() {
                // NOT archived, and the caller is told so. The `.cram` index has no field for a link
                // target -- `EntryMeta` is `is_dir | name | size | mode | chunk_ids` and `mode` is
                // defined as permission bits only -- so representing one is a v1 format change, not a
                // code change. Until that is decided, the honest behaviour is to say what was left out.
                //
                // Dropping them in silence was the actual bug: a kernel tree went in with 99 symlinks
                // and came out with none, while `cram t` called the archive clean. For something sold on
                // backup integrity, unreported loss is the worst failure mode there is.
                //
                // Dereferencing instead is not the safe default it looks like. 7-Zip and WinRAR do it,
                // and on that same tree it silently duplicated 8,011 files behind twelve directory
                // symlinks -- and it turns a link cycle into an unbounded walk.
                skipped.push(child_name);
                continue;
            }
            if ft.is_dir() {
                pending.push(Pending::Dir(child.path(), child_name));
            } else if ft.is_file() {
                pending.push(Pending::File(child.path(), child_name, child.metadata()?));
            }
            // Anything else (fifo, socket, device) has no archive representation either and is
            // simply not a file; it is left out without comment.
        }
        pending.reverse();
        stack.append(&mut pending);
    }
    Ok(())
}

/// Expand one CLI input (file or directory) into archive members, rooted at its base name.
fn collect_input(
    input: &Path,
    items: &mut Vec<CreateItem>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    // `.`, `./` and `..` have no `file_name`, and they are how people actually spell "this
    // directory". Resolve them to the directory they name, so `cram a out.zip .` roots the archive
    // exactly as naming that directory would. Only the filesystem root is left with no answer.
    let base = match input.file_name().and_then(|s| s.to_str()) {
        Some(b) => b.to_string(),
        None => fs::canonicalize(input)
            .ok()
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .ok_or_else(|| ArchiveError::Backend(format!("cannot derive a name for {input:?}")))?,
    };
    // A named input that is itself a symlink. `fs::metadata` follows it, so without this check a
    // symlinked directory passed on the command line would be walked and archived as a real one.
    if fs::symlink_metadata(input)
        .map(|m| m.is_symlink())
        .unwrap_or(false)
    {
        skipped.push(base);
        return Ok(());
    }
    let meta = fs::metadata(input)?;
    if meta.is_dir() {
        collect_dir(input, &base, items, skipped)?;
    } else if meta.is_file() {
        if let Some(entry) = make_entry(&base, meta.len(), false, meta.modified().ok()) {
            items.push(CreateItem {
                entry,
                disk_path: Some(input.to_path_buf()),
                hint: WriteHint::default(),
            });
        }
    } else {
        return Err(ArchiveError::Backend(format!(
            "unsupported source (not a file or directory): {input:?}"
        )));
    }
    Ok(())
}

/// Create `archive` of format `fmt` from `inputs`, honoring `opts` (level/codec/encryption). Returns
/// the writer's [`CreateReport`]. Cancellation abandons the job: the staging file is removed and
/// `archive` is never written, so a cancelled create can never be mistaken for a complete archive
/// that happens to be missing files.
pub fn create(
    archive: &Path,
    fmt: Format,
    inputs: &[PathBuf],
    mut opts: CreateOptions,
    sink: &dyn ProgressSink,
) -> Result<CreateReport> {
    if inputs.is_empty() {
        return Err(ArchiveError::Backend("no input files to add".into()));
    }

    let create_t0 = std::time::Instant::now();
    // Plan the full member list up front (also sizes the progress bar).
    let walk_t0 = std::time::Instant::now();
    let mut items = Vec::new();
    // Names the walk refused to archive. Surfaced in the report rather than dropped in silence:
    // see the symlink arm of `collect_dir`.
    let mut skipped: Vec<String> = Vec::new();
    for input in inputs {
        collect_input(input, &mut items, &mut skipped)?;
    }
    super::prof::WALK_NANOS.store(walk_t0.elapsed().as_nanos() as u64, Relaxed);
    // Sizing hook for a caller-supplied Progress. Accumulated over the plan in place: handing
    // `model::totals` a slice meant cloning every Entry (144 bytes plus two heap allocations each)
    // into a Vec that outlived the whole create for two numbers.
    let (mut total_bytes, mut total_files) = (0u64, 0u64);
    for item in items.iter().filter(|i| !i.entry.is_dir()) {
        // Sizes are read off the filesystem here, but saturate anyway: this is the same accounting
        // `model::totals` does for untrusted archive metadata.
        total_bytes = total_bytes.saturating_add(item.entry.size);
        total_files += 1;
    }
    let _ = total_files;
    // The walk has already counted every byte for the progress bar, so handing the figure to the
    // writer is free. brotli is the one backend that changes its output because of it; see
    // `CreateOptions::total_bytes`.
    opts.total_bytes = Some(total_bytes);

    // Adaptive probe (Level::Auto only): classify each file store-vs-compress. A per-entry hint is
    // honored by the random-access backends (ZIP, 7z); the aggregate summary lets a whole-stream
    // backend (tar.gz/xz) avoid burning CPU on a mostly-already-compressed input. An explicit
    // `--store` or a forced codec skips the probe entirely, the user has already decided.
    // The pre-pass now runs ONLY for the one thing that needs an answer before the loop starts: a
    // whole-stream tar codec picking its level from the aggregate. Everything else is decided inline
    // below, from the handle the loop already holds.
    //
    // It used to run for every format, and it classifies by opening and sampling, so every file was
    // opened twice -- once here and once in the loop -- for a per-entry verdict the loop could have
    // reached for free. On a 94,829-file tree that is 94,829 redundant opens at ~178 us each, and
    // `File::open` is the largest single cost in create.
    let adaptive = opts.level == Level::Auto && opts.codec.is_none();
    let wants_summary = fmt.container == Container::Tar && fmt.codec != Codec::None;
    if adaptive && wants_summary {
        let probe_t0 = std::time::Instant::now();
        let mut summary = ProbeSummary::default();
        for item in &mut items {
            if let Some(disk) = &item.disk_path {
                let verdict = probe::classify_file(disk, item.entry.size);
                summary.add(item.entry.size, verdict);
                item.hint = WriteHint {
                    store: verdict.is_store(),
                };
            }
        }
        // tar can't switch method per entry; if the input is dominated by already-compressed
        // bytes, drop the whole-stream level to Fastest (spend the least CPU for ~0 ratio gain).
        if fmt.container == Container::Tar
            && fmt.codec != Codec::None
            && summary.mostly_incompressible()
        {
            opts.level = Level::Fastest;
        }
        super::prof::PROBE_NANOS.store(probe_t0.elapsed().as_nanos() as u64, Relaxed);
    }

    // DO NOT sort `items` by type here. It was tried on 2026-08-03 and measured: grouping the
    // entry list by `(store, extension, path)` before this loop, so that a `.cram` pack would hold
    // one kind of file, ran the 94,829-file kernel tree roughly 25x slower. Killed after ten
    // minutes having written 150 MB of a 191 MB archive, against 20-36 s in tree order.
    //
    // The cause is that this list is also the read order. In tree order a directory's metadata is
    // warm while its files are opened; sorted by extension every consecutive open lands in a
    // different directory, and `File::open` is already the single largest cost in create at 52% and
    // ~178 us per file. Reordering multiplies the one thing that was already dominant.
    //
    // The idea itself is sound -- what shares a pack decides how well that pack compresses, which is
    // why 7-Zip sorts into its solid blocks. It belongs at the pack-assignment layer, not here:
    // read in tree order, route chunks to a per-class pack buffer. That keeps the locality and
    // still gets homogeneous packs. See docs/PERFORMANCE_FINDINGS.md.

    // Build in a sibling staging file and rename over `archive` only after a successful finish,
    // writing directly to `archive` truncated any pre-existing archive at that path the moment the
    // writer opened, so a create that then failed (unreadable input, disk full, ZIP64 overflow)
    // destroyed the user's old archive and left a headerless fragment in its place.
    let staging = super::staging_path(archive);
    let result = (|| {
        let mut writer = formats::create(&staging, fmt, &opts)?;
        // Asked once, before the loop: `.cram` reads its own sources so it can chunk them off this
        // thread, and then opening the file here, sampling it for a hint it ignores, and streaming
        // it through a counting reader would all be work done twice or not at all.
        let hands_over_paths = writer.takes_paths();
        for item in &items {
            sink.wait_if_paused();
            if sink.is_cancelled() {
                // Do NOT finalize. Calling `finish()` here would write a valid-looking archive that
                // is silently missing every remaining file, and return a success report with no
                // indication it was cut short. Abandon the partial and report the cancellation.
                return Err(ArchiveError::Cancelled);
            }
            sink.on_entry_start(&item.entry);
            match &item.disk_path {
                None => writer.add_dir(&item.entry)?,
                Some(disk) if hands_over_paths => {
                    // The writer opens and reads it on its own schedule, so byte progress is
                    // reported per entry from the plan rather than per read. `on_file_done` below
                    // already had that shape; this makes the byte counter match it.
                    writer.add_path(&item.entry, disk, item.hint).map_err(|e| {
                        if sink.is_cancelled() {
                            ArchiveError::Cancelled
                        } else {
                            e
                        }
                    })?;
                    sink.on_bytes(item.entry.size);
                }
                Some(disk) => {
                    // Name the file. One locked or unreadable source aborts the whole create, and
                    // `io::Error` from `File::open` carries no path, so on a 100 000-file backup the
                    // operator could not tell what to exclude.
                    let open_t0 = std::time::Instant::now();
                    let file = File::open(disk)
                        .map_err(|e| ArchiveError::Backend(format!("{}: {e}", disk.display())))?;
                    super::prof::OPEN_NANOS.fetch_add(open_t0.elapsed().as_nanos() as u64, Relaxed);
                    super::prof::OPEN_COUNT.fetch_add(1, Relaxed);

                    // Classify store-vs-compress from the handle we are already holding, then hand
                    // the sampled bytes back to the writer ahead of the rest of the file so nothing
                    // is read twice either.
                    //
                    // The decision must match `probe::classify_file` exactly, or zip and 7z archives
                    // would change: empty is Compress, a known extension settles it outright, below
                    // the minimum sample size is Compress, and only what is left gets sampled.
                    // `WriteHint::default()` is Compress, which is why the untouched branches simply
                    // fall through.
                    let mut head = Vec::new();
                    let mut hint = item.hint;
                    if adaptive && !wants_summary && item.entry.size > 0 {
                        match probe::ext_only_verdict(disk) {
                            Some(verdict) => {
                                hint = WriteHint {
                                    store: verdict.is_store(),
                                }
                            }
                            None if item.entry.size >= probe::PROBE_MIN_SAMPLE => {
                                (&file)
                                    .take(probe::PROBE_SAMPLE_BYTES)
                                    .read_to_end(&mut head)?;
                                if !head.is_empty() {
                                    hint = WriteHint {
                                        store: probe::sample_verdict(&head).is_store(),
                                    };
                                }
                            }
                            None => {}
                        }
                    }
                    let mut body = CountingReader::new(Cursor::new(head).chain(file), sink);
                    // A cancel that fires mid-body comes back as whatever the backend wrapped the
                    // reader's error in (`chunker: … "cancelled"` for `.cram`). Translate it back,
                    // the same way both extract paths do, so a user pressing Cancel is told the job
                    // was cancelled rather than shown what reads like archive damage.
                    writer.add_file(&item.entry, &mut body, hint).map_err(|e| {
                        if sink.is_cancelled() {
                            ArchiveError::Cancelled
                        } else {
                            e
                        }
                    })?;
                }
            }
            crate::diag::diag().entry(item.entry.name(), Some(item.entry.size), "add");
            sink.on_file_done(&item.entry);
        }
        writer.finish()
    })();
    // The `.cram` writer prints its own detailed profile, but the walk and the probe run before any
    // writer exists and belong to the engine. Printing them here means every backend gets the same
    // breakdown, which is what the ZIP work needed: the serial head of a create is invisible from
    // inside a writer, and it is where the time was.
    if std::env::var_os("CRAM_PROFILE").is_some() {
        let ms = |n: u64| n as f64 / 1e6;
        let wall = create_t0.elapsed().as_nanos() as f64 / 1e6;
        let walk = super::prof::WALK_NANOS.load(Relaxed);
        let probe = super::prof::PROBE_NANOS.load(Relaxed);
        let open = super::prof::OPEN_NANOS.load(Relaxed);
        let opens = super::prof::OPEN_COUNT.load(Relaxed);
        let pct = |n: f64| if wall > 0.0 { n / wall * 100.0 } else { 0.0 };
        eprintln!("-- engine create ------------------------------------------------");
        eprintln!(
            "wall            {wall:9.1} ms  {} entries planned",
            items.len()
        );
        eprintln!(
            "walk (serial)   {:9.1} ms  {:5.1}%   runs to completion before the writer exists",
            ms(walk),
            pct(ms(walk))
        );
        eprintln!(
            "probe (serial)  {:9.1} ms  {:5.1}%",
            ms(probe),
            pct(ms(probe))
        );
        eprintln!(
            "open (serial)   {:9.1} ms  {:5.1}%   {} files{}",
            ms(open),
            pct(ms(open)),
            opens,
            if opens == 0 {
                ", writer took paths and opens them on its own threads"
            } else {
                ""
            }
        );
    }
    match result {
        Ok(mut report) => {
            // What the walk refused to archive travels back with the result. A caller that prints
            // only `entries` would otherwise have no way to know the archive is not the tree.
            report.skipped_links = skipped;
            if let Err(e) = fs::rename(&staging, archive) {
                // Couldn't take the destination (e.g. the old archive is open in another program):
                // remove the staging file so nothing new is left behind; the old archive survives.
                let _ = fs::remove_file(&staging);
                return Err(e.into());
            }
            Ok(report)
        }
        Err(e) => {
            let _ = fs::remove_file(&staging);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order `collect_dir` emits members in *is* the archive's layout, so it is pinned here
    /// rather than left to whatever a walk happens to produce. The contract: a directory's own
    /// entry first, then its children sorted by name, and a subdirectory's whole subtree before the
    /// next sibling.
    ///
    /// This exists because the walk was converted from recursion to an explicit stack (a deep tree
    /// overflowed the stack and killed the process), and a stack pops in reverse. Nothing else in
    /// the suite would have caught the order silently inverting: `tests/reproducible.rs` proves the
    /// same input twice gives identical bytes, which stays true under any consistent order.
    #[test]
    fn members_are_emitted_depth_first_in_sorted_order() {
        let dir = std::env::temp_dir().join(format!("cram-create-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // b/ sorts between the two loose files, so a subtree really does interrupt the sibling run.
        for d in ["a_dir", "b_dir/inner"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in [
            "a_dir/two.txt",
            "a_dir/one.txt",
            "b_dir/inner/deep.txt",
            "b_dir/mid.txt",
            "zz_last.txt",
            "aa_first.txt",
        ] {
            File::create(dir.join(f)).unwrap();
        }

        let (mut items, mut skipped) = (Vec::new(), Vec::new());
        collect_dir(&dir, "root", &mut items, &mut skipped).unwrap();
        let got: Vec<&str> = items.iter().map(|i| i.entry.name()).collect();

        assert_eq!(
            got,
            vec![
                "root",
                "root/a_dir",
                "root/a_dir/one.txt",
                "root/a_dir/two.txt",
                "root/aa_first.txt",
                "root/b_dir",
                "root/b_dir/inner",
                "root/b_dir/inner/deep.txt",
                "root/b_dir/mid.txt",
                "root/zz_last.txt",
            ]
        );
        assert!(skipped.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
