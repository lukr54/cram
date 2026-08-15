//! The sequential extraction path, one entry at a time via [`ArchiveReader::next_entry`]. Used for
//! non-random-access formats (RAR, tar, raw single-stream, solid 7z): the reader yields each entry
//! as a stream, and the engine owns path resolution, directory creation, progress and cancellation
//!, the same write machinery the parallel path uses, so every backend inherits it.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::engine::{restore_mtime, skip, ProgressWriter};
use crate::error::{ArchiveError, Report, Result};
use crate::progress::ProgressSink;
use crate::reader::ArchiveReader;

/// One reusable copy buffer for the whole extraction, allocated once.
///
/// **It used to be an 8 MiB `BufWriter` built inside the per-entry loop**, so a 94,778-entry
/// `.tar.gz` allocated 94,778 of them, and every byte landed in `io::copy`'s 8 KiB stack buffer
/// before reaching one. Kernel tree to `/dev/shm`: **14.25 s → 13.22, peak RSS 97 MB → 86.**
///
/// 1 MiB rather than 8: the same constant was measured on the parallel path today and 8 MiB bought
/// nothing on either a RAM disk or a real disk. Allocated once here, so the size costs nothing.
///
/// **Not to be confused with the memset in the profile.** A profile of this extraction puts 34% in
/// `__memset_avx2_unaligned_erms`, which is mimalloc zeroing fresh pages on a spawned thread, and it
/// is unchanged by any of this. Building without mimalloc removes it and the extraction takes
/// exactly as long — 13.30 s against 13.42, in 49 MB instead of 84 — so that CPU gates nothing. At
/// 141% CPU this path is bound by its single decode thread, and a flat profile's sample share is not
/// a wall-time attribution when more than one thread is running.
const WRITE_BUF: usize = 1024 * 1024;

/// Copy `from` into `to` through a caller-owned buffer.
///
/// `io::copy` would allocate its own, and a `BufWriter` on top of it would add a second hop: bytes
/// landed in an 8 KiB stack buffer, then an 8 MiB heap buffer, then the file. This is one buffer and
/// one hop, and the buffer outlives the entry.
fn copy_through<R: io::Read + ?Sized, W: io::Write + ?Sized>(
    from: &mut R,
    to: &mut W,
    buf: &mut [u8],
) -> io::Result<u64> {
    let mut total = 0u64;
    loop {
        let n = match from.read(buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buf[..n])?;
        total += n as u64;
    }
}

/// Extract every entry of `reader` under `dest`, streaming front-to-back. Per-entry failures are
/// collected into the [`Report`] (non-fatal); cancellation stops before the next entry. When
/// `skip_existing` is set, an entry whose destination already matches (size + CRC) is not written
/// (the stream is still decoded by the backend, but the disk write; the wall; is skipped).
pub fn run(
    reader: &mut dyn ArchiveReader,
    dest: &Path,
    skip_existing: bool,
    sink: &dyn ProgressSink,
    created: &super::unwind::CreatedLog,
) -> Result<Report> {
    fs::create_dir_all(dest)?;
    let mut report = Report::default();
    // Directory mtimes are applied only after the whole tree is written (a child write bumps the
    // parent's mtime), so collect them here and flush in a final pass below.
    let mut dir_times: Vec<(PathBuf, SystemTime)> = Vec::new();
    // Once, for the whole extraction. See [`WRITE_BUF`].
    let mut copy_buf = vec![0u8; WRITE_BUF];

    loop {
        sink.wait_if_paused();
        if sink.is_cancelled() {
            report.cancelled = true;
            break;
        }
        let mut es = match reader.next_entry() {
            Ok(Some(es)) => es,
            Ok(None) => break,
            // A password problem is not damage: it applies to the whole archive and the caller needs
            // to be able to prompt and retry, so it stays fatal.
            Err(e @ (ArchiveError::PasswordRequired | ArchiveError::WrongPassword)) => {
                return Err(e)
            }
            // The stream could not advance, a damaged header, a truncated archive, a volume that
            // ends early. Everything already extracted is on disk and correct, so keep it and stop
            // here instead of failing the whole job and reporting nothing. `Report::is_ok()` is
            // false while `failed` is non-empty, so this can never be mistaken for a clean run.
            Err(e) => {
                report.push_failure("<archive>", e);
                break;
            }
        };
        let entry = es.entry.clone();

        let outpath = entry.path.join_under(dest);
        if entry.is_dir() {
            let _ = created.ensure_dir(&outpath);
            if let Some(t) = entry.modified {
                dir_times.push((outpath, t));
            }
            continue;
        }

        // Skip-already-correct: destination already matches → don't write it (body is dropped).
        if skip_existing && skip::dest_already_correct(&outpath, &entry) {
            sink.on_bytes(entry.size);
            sink.on_file_done(&entry);
            report.skipped += 1;
            continue;
        }
        sink.on_entry_start(&entry);

        // Parent-dir creation and file open are non-fatal per entry (matches the parallel path): a
        // single bad name, e.g. a Linux-authored entry that's invalid on Windows, records one
        // failure and continues rather than aborting the whole job. Drain the (already-buffered)
        // body first so the reader stays in sync for the next entry.
        if let Some(parent) = outpath.parent() {
            if let Err(e) = created.ensure_dir(parent) {
                let _ = io::copy(&mut es.body, &mut io::sink());
                report.push_failure(entry.name(), e);
                continue;
            }
        }
        let file = match created.create_file(&outpath) {
            Ok(f) => f,
            Err(e) => {
                let _ = io::copy(&mut es.body, &mut io::sink());
                report.push_failure(entry.name(), e);
                continue;
            }
        };
        let mut writer = ProgressWriter::new(file, sink);
        match copy_through(&mut es.body, &mut writer, &mut copy_buf).and_then(|n| {
            writer.flush()?;
            Ok(n)
        }) {
            // Short decode with no error = silent truncation. Only enforce when the header size is
            // authoritative: `meta_final == false` means the backend deferred the real length (raw
            // single-stream sources report size 0), where a mismatch is expected and legitimate.
            Ok(n) if es.meta_final && n != entry.size => {
                drop(writer);
                let _ = fs::remove_file(&outpath);
                report.push_failure(
                    entry.name(),
                    io::Error::other(format!(
                        "decoded {n} bytes but the archive declared {}",
                        entry.size
                    )),
                );
            }
            Ok(n) => {
                // Stamped on the descriptor we still hold, not by reopening `outpath`.
                super::restore_mtime_open(writer.get_ref(), entry.modified);
                crate::diag::diag().entry(entry.name(), Some(n), "ok");
                report.extracted += 1;
                report.bytes += n;
                sink.on_file_done(&entry);
            }
            Err(e) => {
                drop(writer);
                let _ = fs::remove_file(&outpath);
                if sink.is_cancelled() {
                    report.cancelled = true;
                    break;
                }
                report.push_failure(entry.name(), e);
            }
        }
    }

    // Final pass: stamp directory mtimes now that every child has been written.
    for (path, t) in dir_times {
        restore_mtime(&path, Some(t));
    }
    Ok(report)
}
