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

/// 8 MiB write blocks, sized to keep the write stream saturated.
const WRITE_BUF: usize = 8 * 1024 * 1024;

/// Does the filesystem holding `dir` treat two names differing only in case as the same file?
///
/// Worth a syscall rather than a `cfg!`, because this is a property of the filesystem and not of the
/// OS: NTFS and a default APFS/HFS+ volume fold case, ext4 does not, and both Windows 10+ and APFS
/// can flip the behaviour per-directory or per-volume. Guessing from the OS gets macOS wrong in the
/// dangerous direction, since it would leave two entries scheduled onto one real file.
///
/// Write a probe file, then ask for the same name in flipped case: getting it back means the lookup
/// ignored case. If the probe cannot run at all (read-only destination), fall back to the platform's
/// usual default, which is the answer for the overwhelming majority of volumes on each OS.
/// Order destination groups for the pool, balancing two concerns that pull opposite ways.
///
/// Load balance wants longest-processing-time-first, so the heaviest work is handed out early and
/// the pool does not finish on one straggler. Locality wants groups sharing a decode unit adjacent,
/// so concurrent workers land on the same pack rather than scattering across unrelated ones.
///
/// So: cluster by locality key, order clusters heaviest-first, keep LPT inside a cluster. A format
/// whose entries decode independently returns `None` for everything, which collapses to one cluster
/// and reproduces pure LPT exactly.
///
/// Separated from `run` so it can be tested without a filesystem or an archive. The scattering this
/// prevents is not a tuning matter: it re-decoded `.cram` packs more than sixteen times each,
/// tripped the anti-decompression-bomb budget, and failed 60,052 entries of a sound archive.
fn order_groups(
    groups: Vec<Vec<usize>>,
    entries: &[Entry],
    locality: impl Fn(usize) -> Option<u64>,
) -> Vec<Vec<usize>> {
    let weight = |g: &Vec<usize>| -> u64 {
        g.iter()
            .fold(0u64, |acc, &i| acc.saturating_add(entries[i].size))
    };
    let mut clusters: Vec<(Option<u64>, Vec<Vec<usize>>)> = Vec::new();
    let mut cluster_of: std::collections::HashMap<Option<u64>, usize> =
        std::collections::HashMap::new();
    for g in groups {
        // A group's members run in sequence on one worker, so its first member decides where it
        // belongs.
        let key = g.first().copied().and_then(&locality);
        match cluster_of.get(&key) {
            Some(&c) => clusters[c].1.push(g),
            None => {
                cluster_of.insert(key, clusters.len());
                clusters.push((key, vec![g]));
            }
        }
    }
    for (_, gs) in &mut clusters {
        gs.sort_by_key(|g| Reverse(weight(g)));
    }
    clusters.sort_by_key(|(_, gs)| Reverse(gs.iter().map(weight).fold(0u64, u64::saturating_add)));
    clusters.into_iter().flat_map(|(_, gs)| gs).collect()
}

fn dest_is_case_insensitive(dir: &Path) -> bool {
    let name = format!(".cram-case-probe-{}", std::process::id());
    let lower = dir.join(&name);
    let upper = dir.join(name.to_uppercase());
    if fs::write(&lower, b"").is_err() {
        return cfg!(any(windows, target_os = "macos"));
    }
    let insensitive = upper.exists();
    let _ = fs::remove_file(&lower);
    insensitive
}

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

    // Group by DESTINATION path before scheduling. Two entries can map to one on-disk file,
    // literal duplicate names (legal in ZIP), case-variants (`A.txt`/`a.txt` on case-insensitive
    // NTFS), Win32 trailing-dot/space normalization, or device-name mangling collisions, and two
    // workers writing the same file interleave their 8 MiB blocks into silent corruption (each
    // passes its own size check, so the report says clean), while a failing worker's remove_file
    // deletes its sibling's finished output.
    //
    // A colliding group is SERIALISED onto one task in archive order, never thinned. Keeping only
    // the last entry per folded key destroyed files whenever the fold was harsher than the target
    // filesystem's own rule: `K.txt` (U+212A) and `k.txt`, `ẞ.txt` (U+1E9E) and `ß.txt`, `Å.txt`
    // (U+00C5) and `Å.txt` (U+212B) are each two distinct files on NTFS and each fold to one key
    // under Rust's full-Unicode `to_lowercase`, so a four-pair archive lost three files with exit 0
    // while the sequential path wrote all eight. Writing a group in order gives last-writer-wins
    // where the destinations really are one file (the semantics the dedup was for, and the ones
    // sequential extraction already has) and gives every file where they are not.
    //
    // That leaves the fold a scheduling prefilter rather than a decision about data, so it is
    // deliberately AGGRESSIVE: over-folding costs one group a little parallelism, under-folding puts
    // two workers back on one file. Case folding is still probed on `dest` at runtime rather than
    // assumed from `cfg`, because case-insensitivity belongs to the filesystem and not to the OS.
    let fold_case = dest_is_case_insensitive(dest);
    let dest_key = |e: &Entry| -> String {
        e.path
            .safe()
            .components()
            .map(|c| {
                let s = c.as_os_str().to_string_lossy();
                // Mangle first, exactly as `join_under` does: `safe()` records the archive's own
                // name, so `NUL` and `_NUL` only become one destination here. Skipping this put two
                // workers back on one file and interleaved their blocks (2 runs in 12).
                let s = crate::model::mangle_dos_device(&s);
                #[cfg(windows)]
                let s = s.trim_end_matches([' ', '.']).to_string();
                #[cfg(not(windows))]
                let s = s.into_owned();
                if fold_case {
                    s.to_lowercase()
                } else {
                    s
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    };
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.is_dir() {
            continue;
        }
        let key = dest_key(e);
        match group_of.get(&key) {
            Some(&g) => groups[g].push(i),
            None => {
                group_of.insert(key, groups.len());
                groups.push(vec![i]);
            }
        }
    }
    // Order the groups. Two competing concerns, and both matter.
    //
    // LOAD BALANCE wants longest-processing-time-first: hand the pool its heaviest work early so it
    // drains evenly rather than finishing with one long straggler.
    //
    // LOCALITY wants groups that share a decode unit adjacent, so concurrent workers land on the
    // same pack instead of scattering across unrelated ones. Ignoring this is what made extracting a
    // 94,778-file `.cram` fail: workers thrashed the 32-slot pack cache, packs were re-decoded well
    // over sixteen times each, and the anti-bomb budget tripped on 60,052 entries of a sound archive.
    //
    // So: cluster by locality key, order clusters heaviest-first, and keep LPT inside a cluster.
    // Formats whose entries decode independently report `None` for every entry, which collapses to a
    // single cluster and leaves the old pure-LPT behaviour exactly as it was.
    let groups = order_groups(groups, entries, |i| ra.locality_key(i));

    let report = Mutex::new(Report::default());
    let pool = ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .build()
        .map_err(|e| ArchiveError::Backend(e.to_string()))?;

    pool.install(|| {
        groups.par_iter().for_each(|group| {
            // One task per destination group; its members run in archive order on this thread, so
            // two entries that resolve to the same file can never be in flight at once.
            for &i in group {
                sink.wait_if_paused();
                if sink.is_cancelled() {
                    return;
                }
                // Isolate each entry: a panic inside a decoder (e.g. a malformed or pathological
                // compressed stream) is caught and recorded as a failed entry rather than unwinding
                // the whole extraction, one bad entry in a big archive can't take down the rest or
                // crash the host process (which matters for the GUI, which extracts in-process).
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
            }
        });
    });

    // Final pass: stamp directory mtimes now that every child has been written.
    for (path, t) in dir_times {
        restore_mtime(&path, Some(t));
    }

    let mut r = report.into_inner().unwrap();
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
        // Ok(0) at EOF, we would write a 5-byte file and report a clean extraction. Compare what
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

    fn sized_entries(sizes: &[u64]) -> Vec<Entry> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| Entry {
                index: i,
                path: crate::model::EntryPath::from_raw(&format!("f{i}")).unwrap(),
                kind: crate::model::EntryKind::File,
                size,
                compressed_size: None,
                modified: None,
                unix_mode: None,
                crc32: None,
                encrypted: false,
            })
            .collect()
    }

    /// Groups sharing a decode unit must come out adjacent, or concurrent workers scatter across
    /// unrelated packs, thrash the cache and re-decode enough to trip the anti-bomb budget. That is
    /// not hypothetical: it failed 60,052 entries of a sound 94,778-file archive.
    ///
    /// Entry sizes are chosen so pure weight ordering would interleave the two packs (30, 25, 20,
    /// 15), which is exactly the arrangement that broke.
    #[test]
    fn groups_sharing_a_pack_are_scheduled_together() {
        let entries = sized_entries(&[30, 25, 20, 15]);
        // Entries 0 and 2 live in pack 7; entries 1 and 3 live in pack 3.
        let key = |i: usize| Some(if i.is_multiple_of(2) { 7u64 } else { 3u64 });
        let ordered = order_groups(vec![vec![0], vec![1], vec![2], vec![3]], &entries, key);
        let keys: Vec<u64> = ordered.iter().map(|g| key(g[0]).unwrap()).collect();
        assert_eq!(
            keys,
            vec![7, 7, 3, 3],
            "each pack's groups must be contiguous, heavier pack first"
        );
        // And LPT is preserved inside a cluster.
        assert_eq!(ordered[0], vec![0], "heaviest of pack 7 leads it");
        assert_eq!(ordered[2], vec![1], "heaviest of pack 3 leads it");
    }

    /// A format whose entries decode independently reports no key, and must keep the pure
    /// longest-processing-time-first order it had before locality existed.
    #[test]
    fn no_locality_key_is_plain_lpt() {
        let entries = sized_entries(&[10, 40, 20, 30]);
        let ordered = order_groups(vec![vec![0], vec![1], vec![2], vec![3]], &entries, |_| None);
        assert_eq!(ordered, vec![vec![1], vec![3], vec![2], vec![0]]);
    }

    /// Case-variant names (`A.txt` / `a.txt`) resolve to ONE file on a case-insensitive filesystem,
    /// so both must run on ONE task in archive order: that is what stops two workers racing the same
    /// destination and interleaving their write blocks into silent corruption, and it leaves the
    /// last entry's content on disk exactly as sequential extraction would. Where the two names are
    /// distinct files, both are written and keep their own content. Either way nothing is discarded.
    ///
    /// Which of those holds is decided by the filesystem under the temp directory, not by the OS, so
    /// the test asks the same question the engine asks instead of branching on `cfg`. That keeps it
    /// honest on a case-sensitive APFS volume and on a case-insensitive mount under Linux.
    #[test]
    fn duplicate_destinations_are_serialised_last_wins() {
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
        if dest_is_case_insensitive(&out) {
            // `A.txt` and `a.txt` are ONE destination: both entries are written, in archive order,
            // and the last one owns the file. Nothing is dropped and nothing is called "skipped".
            assert_eq!(report.extracted, 2, "both entries are written, in order");
            assert_eq!(report.skipped, 0, "a collision is not a skip");
            let got = fs::read(out.join("a.txt"))
                .or_else(|_| fs::read(out.join("A.txt")))
                .unwrap();
            assert_eq!(got, b"last-writer-wins");
        } else {
            // The two names are distinct files, so both are written, nothing is shadowed, and each
            // keeps its own content.
            assert_eq!(
                report.extracted, 2,
                "both case-distinct entries are written"
            );
            assert_eq!(
                report.skipped, 0,
                "nothing is shadowed on a case-sensitive fs"
            );
            assert_eq!(fs::read(out.join("A.txt")).unwrap(), vec![b'A'; 300_000]);
            assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"last-writer-wins");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// `K` (U+212A KELVIN SIGN) and `k` fold to one key under Rust's full-Unicode `to_lowercase`,
    /// and whether they are one file or two is the *filesystem's* decision, not ours. They must
    /// both survive wherever they are two: the fold decides scheduling, never which file is worth
    /// keeping. Where they are genuinely one file, archive order decides the winner, which is what
    /// sequential extraction does.
    ///
    /// The probe below asks that exact question rather than asking whether the destination is
    /// case-insensitive, because those are different questions and conflating them is what made this
    /// test fail on macOS. NTFS is case-insensitive through the fixed `$UpCase` table and still keeps
    /// these two apart; APFS applies full Unicode folding and does not. `dest_is_case_insensitive`
    /// answers `true` on both and would predict the wrong outcome on one of them.
    #[test]
    fn unicode_fold_collision_writes_both_files() {
        use zip::write::SimpleFileOptions;
        let mut w = zip::write::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file("\u{212A}.txt", SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"KELVIN").unwrap();
        w.start_file("k.txt", SimpleFileOptions::default()).unwrap();
        w.write_all(b"ascii-k").unwrap();
        let bytes = w.finish().unwrap().into_inner();

        let dir = std::env::temp_dir().join(format!("cram-fold-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("fold.zip");
        fs::write(&zip_path, &bytes).unwrap();

        // Ask this filesystem directly whether U+212A and `k` are one name or two.
        let probe = dir.join("\u{212A}-probe");
        fs::write(&probe, b"").unwrap();
        let folds_kelvin = dir.join("k-probe").exists();
        let _ = fs::remove_file(&probe);

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
        // Both entries are written either way. Where they land on one file the group is serialised
        // and the later one wins; nothing is ever dropped, which is the property under test.
        assert_eq!(report.extracted, 2, "both entries are written");
        assert_eq!(report.skipped, 0, "neither is discarded as a duplicate");
        if folds_kelvin {
            assert_eq!(
                fs::read(out.join("k.txt")).unwrap(),
                b"ascii-k",
                "one file here, and archive order picks the winner"
            );
        } else {
            assert_eq!(fs::read(out.join("\u{212A}.txt")).unwrap(), b"KELVIN");
            assert_eq!(fs::read(out.join("k.txt")).unwrap(), b"ascii-k");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
