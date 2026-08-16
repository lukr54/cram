//! `cram test`, verify an archive's integrity **without extracting**. Decode every entry and, where
//! the container carries a per-entry checksum (ZIP / 7z CRC-32), confirm the decoded bytes match it.
//! Nothing is written to disk, bodies stream through a hashing sink, so even a decompression-bombed
//! entry is bounded (counted and discarded, never buffered whole).
//!
//! Dispatch mirrors [`extract`](super::extract): **random-access** formats (ZIP, ISO 9660, `.cram`,
//! and a 7z whose blocks are usable) fan out across a rayon pool, each task streaming one entry
//! through [`RandomAccessReader::copy_entry`](crate::reader::RandomAccessReader::copy_entry) on its
//! own file handle; everything else uses the sequential `ArchiveReader::next_entry` stream. Using
//! `copy_entry` for `.cram` matters, `next_entry` materializes a whole entry body in memory and
//! refuses one past its in-RAM cap, so a large (multi-GiB) but perfectly healthy `.cram` entry would
//! otherwise be reported as a failure even though `cram x` extracts it fine.
//!
//! **Verify is CPU-bound on every machine**, which is what separates its plan from an extraction's.
//! It decodes every byte and writes none, so the write wall that shapes an extract plan does not
//! apply and the only ceiling is how many units decode independently. Getting that count wrong is
//! not academic: `.cram` reported a single decode unit until its backend learned to answer with its
//! pack count, and this pass ran a 1.6 GB archive at 96% of one core (18.1 s) on a 24-thread
//! machine while 7-Zip did the equivalent at 354% (1.55 s).
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
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex};

use rayon::prelude::*;
use rayon::ThreadPoolBuilder;

use crate::codec::plan::{block_count, plan_codec};
use crate::engine::parallel::{order_groups, panic_message};
use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::hw::{self, HwProfile, Op, Rates, Topology};
use crate::model::Entry;
use crate::progress::ProgressSink;
use crate::reader::{EntryStream, RandomAccessReader};
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

/// Running tally for a pass. Distinct from [`VerifyReport`] only so a failure can carry the entry
/// index it came from: the parallel path finishes entries in whatever order the pool happens to
/// drain, and what `cram t` prints must not depend on that. Failures are sorted back into archive
/// order on the way out, so the same damaged archive reports the same list every run.
#[derive(Default)]
struct Acc {
    checked: u64,
    crc_verified: u64,
    bytes: u64,
    failures: Vec<(usize, String, String)>,
}

impl Acc {
    fn into_report(mut self, cancelled: bool) -> VerifyReport {
        self.failures.sort_by_key(|f| f.0);
        VerifyReport {
            checked: self.checked,
            crc_verified: self.crc_verified,
            bytes: self.bytes,
            failures: self
                .failures
                .into_iter()
                .map(|(_, name, why)| (name, why))
                .collect(),
            cancelled,
        }
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

/// Apply the per-entry verdict to `acc`: on a CRC or size mismatch, record a failure; otherwise
/// count the entry as verified. `checked` is incremented **only** for a passing entry, so
/// `checked + failures.len()` is the exact number of file entries examined (no double-counting).
fn record(
    acc: &mut Acc,
    index: usize,
    entry: &Entry,
    n: u64,
    crc: u32,
    meta_final: bool,
    sink: &dyn ProgressSink,
) {
    acc.bytes += n;
    // Length check runs INDEPENDENTLY of the CRC, never as its `else`. A crafted archive can declare
    // a 10 GiB entry, supply only a few bytes, and store the CRC *of those few bytes*; the checksum
    // then matches and, if that short-circuited the size check, `cram test` would pass a file that
    // decoded to a fraction of its declared length. `meta_final == false` means the backend deferred
    // the real size (raw single-stream sources), where a mismatch is expected.
    if meta_final && n != entry.size {
        acc.failures.push((
            index,
            entry.name().to_string(),
            format!("size mismatch (declared {}, decoded {n})", entry.size),
        ));
        return;
    }
    // A stored CRC-32 (ZIP / 7z) additionally proves the bytes themselves are right, not just the
    // count, the codec framing can accept a body that decoded to the wrong content.
    if let Some(stored) = entry.crc32 {
        if crc != stored {
            acc.failures.push((
                index,
                entry.name().to_string(),
                format!("CRC mismatch (stored {stored:08x}, computed {crc:08x})"),
            ));
            return;
        }
        acc.crc_verified += 1;
    }
    acc.checked += 1;
    sink.on_file_done(entry);
}

/// How many threads to verify with.
///
/// The decision stays in the calibration layer rather than becoming a thread-count rule of its own
/// here, but the inputs differ from an extraction's in one way that decides the answer: **verify
/// writes nothing**, so its write wall is infinite. `derive_plan` reads that as unconditionally
/// CPU-bound and hands back `min(decode units, logical cores)`, which is exactly right, and the
/// spinning-disk guard still applies (concurrent reads thrash an HDD whether or not anything goes
/// back). Passing a real measured wall instead would let a fast enough NVMe classify a decode-only
/// pass as write-bound and clamp it to eight.
///
/// Deliberately does **not** go through `rates_and_wall`: that probes the drive by writing half a
/// gigabyte to it, and `cram t` must not write. Under an infinite wall the measured rates cannot
/// change the verdict, so default rates give the same plan without touching the disk.
fn verify_workers(fmt: Format, entries: &[Entry], units: Option<usize>, path: &Path) -> usize {
    let hw = HwProfile::detect_for(path);
    hw::derive_plan(
        Op::Extract,
        plan_codec(fmt, entries),
        // The backend's own count where it has one (`.cram` packs); the entry list otherwise.
        units.unwrap_or_else(|| block_count(fmt, entries)),
        &hw,
        Topology::SameDrive,
        &Rates::default(),
        // Sustained, because it is not a measurement at all: an infinite wall exists to make the
        // write side irrelevant to a pass that never writes. Calling it a burst would send the
        // write-bound branch to its floor for the opposite reason and by accident.
        hw::Wall::sustained(f64::INFINITY),
    )
    .workers
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

    // Random-access (ZIP, ISO, `.cram`, and a 7z that offered usable blocks) → parallel per-entry
    // over its own handles. Everything else → the sequential `next_entry` stream, which is one
    // front-to-back decode and cannot fan out.
    if reader.as_random_access().is_some() {
        let workers = {
            let units = reader.as_random_access().and_then(|ra| ra.decode_units());
            verify_workers(fmt, reader.entries()?, units, path)
        };
        let ra = reader.as_random_access().unwrap();
        return verify_random_access(ra, workers, sink);
    }

    let mut acc = Acc::default();
    let mut cancelled = false;
    // Sequential entries have no stable index of their own to sort failures by, and do not need
    // one: they are produced in archive order, so a running counter is that order.
    let mut seq = 0usize;
    loop {
        sink.wait_if_paused();
        if sink.is_cancelled() {
            cancelled = true;
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
        let i = seq;
        seq += 1;

        let mut cs = CrcSink::new(sink);
        if let Err(e) = io::copy(&mut body, &mut cs) {
            drop(body); // release the reader borrow before touching `reader` again
            if sink.is_cancelled() {
                cancelled = true;
                break;
            }
            acc.failures
                .push((i, entry.name().to_string(), format!("decode failed: {e}")));
            continue;
        }
        let (n, crc) = (cs.n, cs.crc.finalize());
        drop(body);
        record(&mut acc, i, &entry, n, crc, meta_final, sink);
    }

    Ok(acc.into_report(cancelled))
}

/// Verify a random-access archive across `workers` threads.
///
/// **One task per decode unit, not per entry.** Where the format groups entries into a shared unit
/// (`.cram` packs), every entry of a unit runs on the one task that owns it, so that unit is
/// decompressed exactly once. Merely *ordering* same-pack entries adjacently is not enough: rayon
/// still splits a cluster across workers, they all miss the shared cache at the same instant, they
/// all decompress the same pack, and `PackCache::insert` discards every result but one after the
/// CPU has been spent. That measured 2.31 decodes per pack on a 186-pack archive, 3446 MiB of
/// decompression to verify 1615 MiB.
///
/// Formats whose entries decode independently report no locality key and get one task each, which
/// is the ZIP behaviour unchanged. Unlike extraction there is no destination, so nothing has to be
/// grouped for safety, only for work.
fn verify_random_access(
    ra: &dyn RandomAccessReader,
    workers: usize,
    sink: &dyn ProgressSink,
) -> Result<VerifyReport> {
    let entries = ra.entries();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_of: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.is_dir() {
            continue;
        }
        match ra.locality_key(i) {
            // Shares a decode unit with others: join whichever task owns that unit.
            Some(k) => match group_of.get(&k) {
                Some(&g) => groups[g].push(i),
                None => {
                    group_of.insert(k, groups.len());
                    groups.push(vec![i]);
                }
            },
            // Decodes on its own (every ZIP entry, and a `.cram` entry with no chunks): its own task.
            None => groups.push(vec![i]),
        }
    }
    // Heaviest first, so the pool drains evenly instead of ending on one straggler.
    let groups = order_groups(
        groups,
        entries,
        |i| ra.locality_key(i),
        ra.coalesce_locality(),
    );

    let acc = Mutex::new(Acc::default());
    let pool = ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .build()
        .map_err(|e| ArchiveError::Backend(e.to_string()))?;

    // One decode of an entry, checksummed. Shared by both paths below so they cannot disagree about
    // what counts as a verified entry.
    let check = |i: usize, outcome: std::thread::Result<(Result<u64>, u64, u32)>| {
        let entry = &entries[i];
        let mut a = acc.lock().unwrap();
        match outcome {
            // Random-access entries carry authoritative metadata (`meta_final` is implicitly true).
            Ok((Ok(_), n, crc)) => record(&mut a, i, entry, n, crc, true, sink),
            // A cancelled pass fails every in-flight decode; those are not archive damage.
            Ok((Err(_), ..)) if sink.is_cancelled() => {}
            Ok((Err(e), ..)) => {
                a.failures
                    .push((i, entry.name().to_string(), format!("decode failed: {e}")))
            }
            Err(p) => a
                .failures
                .push((i, entry.name().to_string(), panic_message(p.as_ref()))),
        }
    };

    pool.install(|| {
        groups.par_iter().for_each(|group| {
            // A solid backend decodes the whole unit once and hands over each entry as it passes,
            // so verification costs the checksum buffer rather than a decoded block per worker.
            if ra.streams_units() {
                let served = Mutex::new(Vec::with_capacity(group.len()));
                let pass = catch_unwind(AssertUnwindSafe(|| {
                    ra.copy_unit(group, &mut |i, body| {
                        sink.wait_if_paused();
                        if sink.is_cancelled() {
                            return false;
                        }
                        served.lock().unwrap().push(i);
                        sink.on_entry_start(&entries[i]);
                        let outcome = catch_unwind(AssertUnwindSafe(|| {
                            let mut cs = CrcSink::new(sink);
                            let r = std::io::copy(body, &mut cs).map_err(ArchiveError::from);
                            (r, cs.n, cs.crc.finalize())
                        }));
                        check(i, outcome);
                        true
                    })
                }));
                let why = match pass {
                    Ok(Ok(())) => return,
                    _ if sink.is_cancelled() => return,
                    Ok(Err(e)) => e.to_string(),
                    Err(p) => panic_message(p.as_ref()),
                };
                // An entry the pass never reached is unverified, which is not the same as sound.
                let served = served.into_inner().unwrap();
                let mut a = acc.lock().unwrap();
                for &i in group {
                    if !served.contains(&i) {
                        a.failures.push((
                            i,
                            entries[i].name().to_string(),
                            format!("decode unit failed before this entry: {why}"),
                        ));
                    }
                }
                return;
            }
            for &i in group {
                sink.wait_if_paused();
                if sink.is_cancelled() {
                    return;
                }
                let entry = &entries[i];
                sink.on_entry_start(entry);
                // Isolate the decode: a panic inside a codec on a malformed or pathological stream
                // is one failed entry, not a dead process, which matters most for the GUI (it
                // verifies in-process). The lock is taken only AFTER the catch, so a panicking
                // decoder can never poison it and leave every later entry unwrapping an Err.
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    let mut cs = CrcSink::new(sink);
                    let r = ra.copy_entry(i, &mut cs);
                    (r, cs.n, cs.crc.finalize())
                }));
                check(i, outcome);
            }
        });
    });

    let acc = acc.into_inner().unwrap();
    Ok(acc.into_report(sink.is_cancelled()))
}
