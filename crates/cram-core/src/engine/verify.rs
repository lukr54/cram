//! `cram test`, verify an archive's integrity **without extracting**. Decode every entry and, where
//! the container carries a per-entry checksum (ZIP / 7z CRC-32), confirm the decoded bytes match it.
//! Nothing is written to disk, bodies stream through a hashing sink, so even a decompression-bombed
//! entry is bounded (counted and discarded, never buffered whole).
//!
//! Dispatch mirrors [`extract`](super::extract): **random-access** formats (ZIP, `.cram`) stream each
//! entry via `RandomAccessReader::copy_entry`; everything else uses the sequential
//! `ArchiveReader::next_entry` stream. Using `copy_entry` for `.cram` matters, `next_entry`
//! materializes a whole entry body in
//! memory and refuses one past its in-RAM cap, so a large (multi-GiB) but perfectly healthy `.cram`
//! entry would otherwise be reported as a failure even though `cram x` extracts it fine.
//!
//! What each format's "verified" means:
//! - **ZIP / 7z**: a stored CRC-32 is recomputed over the decoded bytes and compared, real content
//!   integrity.
//! - **tar**: no per-entry checksum, so a clean full decode plus a declared-size match is the check
//!   (catches truncation / a broken codec stream).
//! - **`.cram`**: a clean decode of every pack. Encrypted packs are authenticated by their AES-GCM
//!   tag and compressed packs by their codec framing; an **unencrypted, stored** pack (what
//!   incompressible media compresses to) carries no per-chunk checksum in the frozen v1 format, so
//!   `cram test` confirms it decodes structurally but cannot detect an in-place bit flip in it. For
//!   guaranteed content integrity on such archives, pair with `cram sign` (ed25519) or `cram rec`
//!   (parity), which cover the whole file.
//!
//! This is the backup-verification workflow ("is my archive still good?") every archiver offers.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use crate::error::Result;
use crate::model::Entry;
use crate::progress::ProgressSink;
use crate::reader::EntryStream;
use crate::secret::PasswordProvider;
use crate::{formats, sniff};

/// Outcome of a verify pass. `ok()` is the headline the CLI reports.
#[derive(Debug, Default)]
pub struct VerifyReport {
    /// File entries that decoded AND passed their checksum/size check.
    pub checked: u64,
    /// Of those, how many had a stored CRC-32 that matched; the strongest per-entry guarantee.
    pub crc_verified: u64,
    /// Total uncompressed bytes decoded (across passed and failed entries alike).
    pub bytes: u64,
    /// `(entry name, reason)` for every entry that failed to decode or whose checksum/size mismatched.
    pub failures: Vec<(String, String)>,
    /// Set if the pass was cancelled before reaching the end of the archive.
    pub cancelled: bool,
}

impl VerifyReport {
    /// The archive verified clean: every entry decoded and no checksum/size mismatch, and the pass
    /// ran to completion. `checked + failures.len()` is the total number of file entries examined.
    pub fn ok(&self) -> bool {
        self.failures.is_empty() && !self.cancelled
    }

    /// The sentence a caller must print alongside a clean result when nothing about the *content*
    /// was actually checked.
    ///
    /// A stored, unencrypted `.cram` carries no per-entry checksum (the frozen v1 index records
    /// chunk locations, not hashes), so a pass over one proves the archive still decodes and is the
    /// declared length, and nothing more. `OK: 3 entries verified (0 by CRC)` is true and reads as a
    /// full integrity pass to the person checking whether their backup survived; a bit flipped in a
    /// stored pack goes unreported. The exit code deliberately does not change, scripts already
    /// depend on `cram t` returning 0 for an archive that decodes.
    pub fn content_unverified(&self) -> Option<&'static str> {
        (self.checked > 0 && self.crc_verified == 0).then_some(
            "warning: no entry carried a content checksum, structure and length were verified but \
             the bytes were not; use `cram sign` or `cram rec` to detect damage in this archive",
        )
    }
}

/// A `Write` sink that CRC-32s and counts the bytes streamed through it (never buffering), reports
/// progress, and aborts the copy on cancellation. Mirrors the engine's `ProgressWriter` cancel
/// discipline: `Other` (not `Interrupted`) so `io::copy` unwinds at once instead of retrying.
struct CrcSink<'a> {
    crc: crc32fast::Hasher,
    n: u64,
    sink: &'a dyn ProgressSink,
}

impl<'a> CrcSink<'a> {
    fn new(sink: &'a dyn ProgressSink) -> Self {
        Self {
            crc: crc32fast::Hasher::new(),
            n: 0,
            sink,
        }
    }
}

impl Write for CrcSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.sink.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        self.crc.update(buf);
        self.n += buf.len() as u64;
        self.sink.on_bytes(buf.len() as u64);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Apply the per-entry verdict to `report`: on a CRC or size mismatch, record a failure; otherwise
/// count the entry as verified. `checked` is incremented **only** for a passing entry, so
/// `checked + failures.len()` is the exact number of file entries examined (no double-counting).
fn record(
    report: &mut VerifyReport,
    entry: &Entry,
    n: u64,
    crc: u32,
    meta_final: bool,
    sink: &dyn ProgressSink,
) {
    report.bytes += n;
    // Length check runs INDEPENDENTLY of the CRC, never as its `else`. A crafted archive can declare
    // a 10 GiB entry, supply only a few bytes, and store the CRC *of those few bytes*; the checksum
    // then matches and, if that short-circuited the size check, `cram test` would pass a file that
    // decoded to a fraction of its declared length. `meta_final == false` means the backend deferred
    // the real size (raw single-stream sources), where a mismatch is expected.
    if meta_final && n != entry.size {
        report.failures.push((
            entry.name().to_string(),
            format!("size mismatch (declared {}, decoded {n})", entry.size),
        ));
        return;
    }
    // A stored CRC-32 (ZIP / 7z) additionally proves the bytes themselves are right, not just the
    // count, the codec framing can accept a body that decoded to the wrong content.
    if let Some(stored) = entry.crc32 {
        if crc != stored {
            report.failures.push((
                entry.name().to_string(),
                format!("CRC mismatch (stored {stored:08x}, computed {crc:08x})"),
            ));
            return;
        }
        report.crc_verified += 1;
    }
    report.checked += 1;
    sink.on_file_done(entry);
}

/// Decode and verify every entry of the archive at `path`. `pw` supplies passwords for encrypted
/// entries; `sink` receives progress and can request cancellation. A per-entry failure is collected
/// (the pass continues over the rest of the archive) rather than aborting, so one bad member doesn't
/// mask the health of the others.
pub fn verify(
    path: &Path,
    pw: Arc<dyn PasswordProvider>,
    sink: &dyn ProgressSink,
) -> Result<VerifyReport> {
    let fmt = sniff::sniff_path(path)?;
    let mut reader = formats::open(path, fmt, pw)?;
    let mut report = VerifyReport::default();

    // Random-access (ZIP, `.cram`) → stream each entry via `copy_entry` (its own handle, no whole-body
    // buffering, so multi-GiB entries verify). Everything else → the sequential `next_entry` stream.
    if let Some(ra) = reader.as_random_access() {
        let entries = ra.entries();
        for (i, entry) in entries.iter().enumerate() {
            sink.wait_if_paused();
            if sink.is_cancelled() {
                report.cancelled = true;
                break;
            }
            if entry.is_dir() {
                continue;
            }
            sink.on_entry_start(entry);
            let mut cs = CrcSink::new(sink);
            if let Err(e) = ra.copy_entry(i, &mut cs) {
                if sink.is_cancelled() {
                    report.cancelled = true;
                    break;
                }
                report
                    .failures
                    .push((entry.name().to_string(), format!("decode failed: {e}")));
                continue;
            }
            let (n, crc) = (cs.n, cs.crc.finalize());
            // Random-access entries carry authoritative metadata (`meta_final` is implicitly true).
            record(&mut report, entry, n, crc, true, sink);
        }
        return Ok(report);
    }

    loop {
        sink.wait_if_paused();
        if sink.is_cancelled() {
            report.cancelled = true;
            break;
        }
        let Some(es) = reader.next_entry()? else {
            break;
        };
        let EntryStream {
            entry,
            mut body,
            meta_final,
        } = es;
        if entry.is_dir() {
            continue;
        }
        sink.on_entry_start(&entry);

        let mut cs = CrcSink::new(sink);
        if let Err(e) = io::copy(&mut body, &mut cs) {
            drop(body); // release the reader borrow before touching `reader` again
            if sink.is_cancelled() {
                report.cancelled = true;
                break;
            }
            report
                .failures
                .push((entry.name().to_string(), format!("decode failed: {e}")));
            continue;
        }
        let (n, crc) = (cs.n, cs.crc.finalize());
        drop(body);
        record(&mut report, &entry, n, crc, meta_final, sink);
    }

    Ok(report)
}
