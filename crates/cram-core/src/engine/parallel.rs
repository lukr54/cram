//! The parallel per-entry extraction path. LPT ordering (largest first) keeps the pool balanced; a
//! rayon pool sized to `plan.workers` fans out; every task calls [`RandomAccessReader::copy_entry`],
//! which opens its own handle. Per-entry writers keep the NVMe saturated where a single write stream
//! underutilizes it, so that is the shape.

use std::cmp::Reverse;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use rayon::prelude::*;
use rayon::ThreadPoolBuilder;

use crate::engine::{restore_mtime, skip, EntryOutcome, ProgressWriter};
use crate::error::{ArchiveError, Report, Result};
use crate::model::Entry;
use crate::progress::ProgressSink;
use crate::reader::RandomAccessReader;

/// 8 MiB write blocks — sized to keep the write stream saturated.
const WRITE_BUF: usize = 8 * 1024 * 1024;

/// Extract every file entry of `ra` under `dest` across `workers` threads. Per-entry failures are
/// collected into the [`Report`] (non-fatal); cancellation stops scheduling new work. When
/// `skip_existing` is set, an entry whose destination already matches (size + CRC) is skipped
/// **before** it is decoded or written.
pub fn run(
    ra: &dyn RandomAccessReader,
    dest: &Path,
    workers: usize,
    skip_existing: bool,
    sink: &dyn ProgressSink,
) -> Result<Report> {
    let entries = ra.entries();
    fs::create_dir_all(dest)?;

    // Materialize directories up front (so empty dirs exist even with no files under them).
    // Their mtimes are stamped only after all files are written (a child write bumps the parent's
    // mtime), so remember them here and apply in a final pass below.
    let mut dir_times: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for e in entries.iter().filter(|e| e.is_dir()) {
        let p = e.path.join_under(dest);
        let _ = fs::create_dir_all(&p);
        if let Some(t) = e.modified {
            dir_times.push((p, t));
        }
    }

    // Deduplicate by DESTINATION path before scheduling. Two entries can map to one on-disk file —
    // literal duplicate names (legal in ZIP), case-variants (`A.txt`/`a.txt` on case-insensitive
    // NTFS), Win32 trailing-dot/space normalization, or device-name mangling collisions — and two
    // workers writing the same file interleave their 8 MiB blocks into silent corruption (each
    // passes its own size check, so the report says clean), while a failing worker's remove_file
    // deletes its sibling's finished output. Schedule only the LAST occurrence per folded path
    // (sequential extraction's last-writer-wins) and count the shadowed ones as skipped.
    //
    // The fold must match the TARGET filesystem's own collision rule, or it wrongly drops a distinct
    // file: on Windows/NTFS, case + trailing dots/spaces collide, so fold them; on a case-sensitive
    // filesystem (Linux/macOS-hfsx) only a byte-identical path is the same file, so dedup exactly.
    let dest_key = |e: &Entry| -> String {
        e.path
            .safe()
            .components()
            .map(|c| {
                let s = c.as_os_str().to_string_lossy();
                #[cfg(windows)]
                {
                    s.trim_end_matches([' ', '.']).to_lowercase()
                }
                #[cfg(not(windows))]
                {
                    s.into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    };
    let mut last_for_path: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if !e.is_dir() {
            last_for_path.insert(dest_key(e), i);
        }
    }
    let mut shadowed = 0u64;
    // Longest-processing-time-first: extract the biggest entries first to keep the pool balanced.
    let mut order: Vec<usize> = (0..entries.len())
        .filter(|&i| !entries[i].is_dir())
        .filter(|&i| {
            let wins = last_for_path.get(&dest_key(&entries[i])) == Some(&i);
            if !wins {
                shadowed += 1;
            }
            wins
        })
        .collect();
    order.sort_by_key(|&i| Reverse(entries[i].size));

    let report = Mutex::new(Report::default());
    let pool = ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .build()
        .map_err(|e| ArchiveError::Backend(e.to_string()))?;

    pool.install(|| {
        order.par_iter().for_each(|&i| {
            sink.wait_if_paused();
            if sink.is_cancelled() {
                return;
            }
            // Isolate each entry: a panic inside a decoder (e.g. a malformed or pathological
            // compressed stream) is caught and recorded as a failed entry rather than unwinding the
            // whole extraction — one bad entry in a big archive can't take down the rest or crash the
            // host process (which matters for the GUI, which extracts in-process).
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                extract_one(ra, dest, i, &entries[i], skip_existing, sink)
            }));
            match outcome {
                Ok(Ok(EntryOutcome::Wrote(bytes))) => {
                    let mut r = report.lock().unwrap();
                    r.extracted += 1;
                    r.bytes += bytes;
                }
                Ok(Ok(EntryOutcome::Skipped)) => report.lock().unwrap().skipped += 1,
                Ok(Err(ArchiveError::Cancelled)) => {}
                Ok(Err(e)) => report.lock().unwrap().push_failure(entries[i].name(), e),
                Err(panic) => report.lock().unwrap().push_failure(
                    entries[i].name(),
                    ArchiveError::Backend(panic_message(panic.as_ref())),
                ),
            }
        });
    });

    // Final pass: stamp directory mtimes now that every child has been written.
    for (path, t) in dir_times {
        restore_mtime(&path, Some(t));
    }

    let mut r = report.into_inner().unwrap();
    // Shadowed duplicates were not written (their later twin owns the destination) — surface them
    // as skipped, never as extracted.
    r.skipped += shadowed;
    r.cancelled = sink.is_cancelled();
    Ok(r)
}

/// Best-effort human-readable text from a caught panic payload.
fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        format!("decoder panicked: {s}")
    } else if let Some(s) = p.downcast_ref::<String>() {
        format!("decoder panicked: {s}")
    } else {
        "decoder panicked".to_string()
    }
}

/// Extract one entry (position `pos` in `ra.entries()`) to disk, streaming through an 8 MiB writer
/// that reports progress and honors cancellation. Removes the partial file on error/cancel.
fn extract_one(
    ra: &dyn RandomAccessReader,
    dest: &Path,
    pos: usize,
    entry: &Entry,
    skip_existing: bool,
    sink: &dyn ProgressSink,
) -> Result<EntryOutcome> {
    if sink.is_cancelled() {
        return Err(ArchiveError::Cancelled);
    }

    let outpath = entry.path.join_under(dest);
    // Skip-already-correct: if the destination already matches, don't decode or write it.
    if skip_existing && skip::dest_already_correct(&outpath, entry) {
        sink.on_bytes(entry.size); // account for its bytes so progress still completes
        sink.on_file_done(entry);
        return Ok(EntryOutcome::Skipped);
    }

    sink.on_entry_start(entry);
    if let Some(parent) = outpath.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(&outpath)?;
    let mut writer = ProgressWriter::new(BufWriter::with_capacity(WRITE_BUF, file), sink);

    match ra.copy_entry(pos, &mut writer).and_then(|n| {
        writer.flush()?;
        Ok(n)
    }) {
        // A body stream that ends EARLY without erroring must not pass as a success. A crafted ZIP
        // can declare a 10 GiB entry, supply 5 bytes whose CRC matches, and the decoder returns
        // Ok(0) at EOF — we would write a 5-byte file and report a clean extraction. Compare what
        // actually decoded against what the header promised.
        Ok(n) if n != entry.size => {
            drop(writer);
            let _ = fs::remove_file(&outpath);
            Err(ArchiveError::Corrupt(format!(
                "{}: decoded {n} bytes but the archive declared {}",
                entry.name(),
                entry.size
            )))
        }
        Ok(n) => {
            restore_mtime(&outpath, entry.modified);
            sink.on_file_done(entry);
            Ok(EntryOutcome::Wrote(n))
        }
        Err(e) => {
            drop(writer);
            let _ = fs::remove_file(&outpath);
            Err(if sink.is_cancelled() {
                ArchiveError::Cancelled
            } else {
                e
            })
        }
    }
}

#[cfg(test)]
mod dest_race_tests {
    use super::*;
    use crate::progress::NullSink;
    use crate::secret::NoPassword;
    use std::sync::Arc;

    /// Case-variant names (`A.txt` / `a.txt`) resolve to ONE file on NTFS, so only the LAST entry
    /// may be scheduled. Guards against: two workers racing the same destination and interleaving
    /// their write blocks into silent corruption.
    #[test]
    fn duplicate_destinations_are_deduped_last_wins() {
        use zip::write::SimpleFileOptions;
        let mut w = zip::write::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        // The bigger entry first: LPT ordering would schedule it first if it survived dedup.
        w.start_file("A.txt", SimpleFileOptions::default()).unwrap();
        w.write_all(&vec![b'A'; 300_000]).unwrap();
        w.start_file("a.txt", SimpleFileOptions::default()).unwrap();
        w.write_all(b"last-writer-wins").unwrap();
        let bytes = w.finish().unwrap().into_inner();

        let dir = std::env::temp_dir().join(format!("cram-dest-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("dup.zip");
        fs::write(&zip_path, &bytes).unwrap();

        let reader = crate::formats::open(
            &zip_path,
            crate::format::Format::zip(),
            Arc::new(NoPassword),
        )
        .unwrap();
        let ra = reader.as_random_access().expect("zip is random-access");
        let out = dir.join("out");
        let report = run(ra, &out, 4, false, &NullSink).unwrap();

        assert!(report.failed.is_empty(), "failures: {:?}", report.failed);
        assert_eq!(report.extracted, 1, "only the winning duplicate is written");
        assert_eq!(
            report.skipped, 1,
            "the shadowed duplicate is surfaced as skipped"
        );
        // Whatever the filesystem's case behavior, the LAST entry's content must be what's on disk.
        let got = fs::read(out.join("a.txt"))
            .or_else(|_| fs::read(out.join("A.txt")))
            .unwrap();
        assert_eq!(got, b"last-writer-wins");
        let _ = fs::remove_dir_all(&dir);
    }
}
