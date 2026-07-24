//! The archive-creation path: walk the on-disk sources into an entry list, then stream each into an
//! `ArchiveWriter`. The container's writer owns the compression/encryption/central-directory
//! mechanics; this engine owns the tree walk, per-entry file I/O, progress, and cancellation.
//!
//! Naming mirrors the CLI intuition: a top-level input keeps its own base name as the archive root
//! (`cramc a out.zip pics` → `pics/…`, `cramc a out.zip note.txt` → `note.txt`). Directories are
//! emitted before their children so empty dirs are preserved.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{ArchiveError, Result};
use crate::format::{Codec, Container, Format};
use crate::model::{Entry, EntryKind, EntryPath};
use crate::probe::{self, ProbeSummary};
use crate::progress::{CountingReader, ProgressSink};
use crate::writer::{CreateOptions, CreateReport, Level, WriteHint};
use crate::{formats, model};

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

/// Recurse a directory, emitting its own entry first, then its children under `prefix`.
fn collect_dir(dir: &Path, prefix: &str, items: &mut Vec<CreateItem>) -> Result<()> {
    let dir_mtime = fs::metadata(dir).ok().and_then(|m| m.modified().ok());
    if let Some(entry) = make_entry(prefix, 0, true, dir_mtime) {
        items.push(CreateItem {
            entry,
            disk_path: None,
            hint: WriteHint::default(),
        });
    }
    // Sort children for a deterministic archive layout.
    let mut children: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let name = child.file_name().to_string_lossy().into_owned();
        let child_name = format!("{prefix}/{name}");
        let ft = child.file_type()?;
        if ft.is_dir() {
            collect_dir(&child.path(), &child_name, items)?;
        } else if ft.is_file() {
            let md = child.metadata()?;
            let size = md.len();
            if let Some(entry) = make_entry(&child_name, size, false, md.modified().ok()) {
                items.push(CreateItem {
                    entry,
                    disk_path: Some(child.path()),
                    hint: WriteHint::default(),
                });
            }
        }
        // Symlinks and other special files are skipped for now (classic-container create).
    }
    Ok(())
}

/// Expand one CLI input (file or directory) into archive members, rooted at its base name.
fn collect_input(input: &Path, items: &mut Vec<CreateItem>) -> Result<()> {
    let base = input
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| ArchiveError::Backend(format!("cannot derive a name for {input:?}")))?;
    let meta = fs::metadata(input)?;
    if meta.is_dir() {
        collect_dir(input, base, items)?;
    } else if meta.is_file() {
        if let Some(entry) = make_entry(base, meta.len(), false, meta.modified().ok()) {
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
/// the writer's [`CreateReport`]. Cancellation stops before the next entry (the partial archive is
/// still finalized so it stays a valid file).
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

    // Plan the full member list up front (also sizes the progress bar).
    let mut items = Vec::new();
    for input in inputs {
        collect_input(input, &mut items)?;
    }
    let entries: Vec<Entry> = items.iter().map(|i| i.entry.clone()).collect();
    let (total_bytes, total_files) = model::totals(&entries);
    let _ = (total_bytes, total_files); // sizing hook for a caller-supplied Progress

    // Adaptive probe (Level::Auto only): classify each file store-vs-compress. A per-entry hint is
    // honored by the random-access backends (ZIP, 7z); the aggregate summary lets a whole-stream
    // backend (tar.gz/xz) avoid burning CPU on a mostly-already-compressed input. An explicit
    // `--store` or a forced codec skips the probe entirely, the user has already decided.
    if opts.level == Level::Auto && opts.codec.is_none() {
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
    }

    // Build in a sibling staging file and rename over `archive` only after a successful finish,
    // writing directly to `archive` truncated any pre-existing archive at that path the moment the
    // writer opened, so a create that then failed (unreadable input, disk full, ZIP64 overflow)
    // destroyed the user's old archive and left a headerless fragment in its place.
    let staging = super::staging_path(archive);
    let result = (|| {
        let mut writer = formats::create(&staging, fmt, &opts)?;
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
                Some(disk) => {
                    let file = File::open(disk)?;
                    let mut body = CountingReader::new(file, sink);
                    writer.add_file(&item.entry, &mut body, item.hint)?;
                }
            }
            sink.on_file_done(&item.entry);
        }
        writer.finish()
    })();
    match result {
        Ok(report) => {
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
