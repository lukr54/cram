//! The orchestrator: the piece that consumes [`hw::derive_plan`]. It sniffs the format, opens a
//! reader, derives the plan from the machine profile + the archive's codec/block shape, and
//! dispatches, the parallel per-entry path when the format is random-access (ZIP), otherwise the
//! sequential path (RAR/tar/7z/raw).

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use crate::codec::plan::{block_count, plan_codec};
use crate::error::{ArchiveError, Report, Result};
use crate::hw::{self, HwProfile, Op, Rates, Topology};
use crate::progress::ProgressSink;
use crate::secret::PasswordProvider;
use crate::{formats, sniff};

/// Diagnostic counters read back by the `.cram` writer when `CRAM_PROFILE` is set.
///
/// Create's per-file costs are invisible to the format writer: opening the source, walking the tree
/// and the store-vs-compress probe all happen out here, before or between the `add_file` calls the
/// writer can time. On a 94k-file tree that turned out to be about half of create's wall clock and
/// it was reaching the profile only as an unexplained residual.
///
/// Diagnostics only. Nothing reads these except the profile print, they are `Relaxed`, and they are
/// process-global rather than per-create, which is fine for one CLI invocation and would need
/// revisiting if a caller ever ran two creates at once.
pub mod prof {
    use std::sync::atomic::AtomicU64;
    /// Time in `File::open` on source files, and how many were opened.
    pub static OPEN_NANOS: AtomicU64 = AtomicU64::new(0);
    pub static OPEN_COUNT: AtomicU64 = AtomicU64::new(0);
    /// Time walking the inputs into the entry list.
    pub static WALK_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Time in the adaptive store-vs-compress probe, which reads every file before create starts.
    pub static PROBE_NANOS: AtomicU64 = AtomicU64::new(0);
}

pub mod convert;
pub mod create;
pub mod dedup;
pub mod estimate;
pub mod parallel;
pub mod reclaim;
pub mod sequential;
pub mod skip;
pub mod stream;
pub mod unwind;
pub mod verify;

/// Knobs for an extraction job (fixed for the whole job). Grows as Phase-1+ features land.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtractOptions {
    /// Skip an entry when its destination already matches by size + CRC32 (formats that carry a
    /// CRC, ZIP/7z). See [`skip`]. Off by default (always overwrite).
    pub skip_existing: bool,
}

/// What happened to one entry during extraction, a written file (with its byte count) or a
/// skip-already-correct hit. Feeds the [`Report`] tally in both engine paths.
pub(crate) enum EntryOutcome {
    Wrote(u64),
    Skipped,
}

/// Restore an entry's recorded modification time onto its extracted path. Best-effort: a filesystem
/// that can't set times, or a missing timestamp, is silently ignored; never a reason to fail an
/// otherwise-good extraction. Uses `filetime` (not `File::set_modified`) because it sets the time
/// **by path**, which is the only portable way to stamp a *directory* on Windows (`File::open` on a
/// dir there needs `FILE_FLAG_BACKUP_SEMANTICS`, which std does not pass).
///
/// Directory times must be applied only **after** every child is written, creating a child updates
/// the parent's mtime, so both engine paths collect dir times and flush them in a final pass.
pub(crate) fn restore_mtime(path: &Path, modified: Option<SystemTime>) {
    if let Some(t) = modified {
        let ft = filetime::FileTime::from_system_time(t);
        let _ = filetime::set_file_mtime(path, ft);
    }
}

/// Sibling staging path for an in-progress create/convert. Same directory → same volume, so the
/// final `rename` over the destination is atomic. Writers `File::create` their target immediately,
/// so building directly at the destination would destroy any PRE-EXISTING archive there the moment
/// a create begins, and a create that then failed would leave the user with neither the old archive
/// nor a new one. The destination is only touched by the final rename, once a whole archive exists.
pub(crate) fn staging_path(dest: &Path) -> std::path::PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".cram-partial");
    std::path::PathBuf::from(s)
}

/// Rates + write-wall for planning: the cached profile if present, else a quick in-memory calibrate
/// with a bus/media-derived wall estimate. (Mirrors the `calibrate` bin's resolution.)
fn rates_and_wall(hw: &HwProfile, dest: &Path) -> (Rates, f64) {
    let default_wall = hw
        .work_drive
        .as_ref()
        .map(|d| d.default_wall_mibs())
        .unwrap_or(250.0);
    // Two separate questions, and they expire separately. The codec rates belong to the CPU and are
    // reusable everywhere; the wall belongs to THIS destination and a figure measured on another
    // volume says nothing about it.
    let cached = hw::load_profile(Some(dest));
    if let Some((r, Some(w))) = &cached {
        if r.deflate_dec > 0.0 {
            return (*r, *w);
        }
    }
    // Calibrate ONCE and persist it: the full micro-bench costs ~8s on a modern desktop, so without
    // a stored result *every single extract* would pay for it again, synchronously, and then throw
    // the measurement away. A smaller sample than the standalone `calibrate` binary uses: this runs
    // inline in front of a real extract, so it trades a little precision for latency. Repeated-median
    // sampling keeps it stable at this size, and it is paid once per machine rather than per extract.
    let rates = match &cached {
        Some((r, _)) if r.deflate_dec > 0.0 => *r,
        _ => hw::calibrate(24),
    };
    // Measure the write ceiling of the drive we are about to write to, rather than planning against
    // a bus-table guess forever. Bounded and quick; skipped when the destination is short on space,
    // in which case we fall back to the guess and record nothing.
    let measured = probe_wall_if_safe(dest);
    let key = probe_dir(dest).and_then(hw::volume_key).zip(measured);
    let _ = hw::save_profile(&rates, key);
    (rates, measured.unwrap_or(default_wall))
}

/// Make sure a machine profile exists, doing the measuring NOW rather than in front of the user's
/// first extract.
///
/// Calibration costs about ten seconds on a cold machine. Paid synchronously at the start of the
/// first extraction it is indistinguishable from a freeze: the app sits there having drawn no
/// progress at all, because there is no progress to draw until it finishes. A GUI can call this on
/// a background thread at startup instead; by the time anyone extracts anything, [`rates_and_wall`]
/// finds a cached profile and returns immediately. Cheap and idempotent when a profile is already
/// present.
pub fn warm_profile(dest: &Path) {
    let hw = HwProfile::detect_for(dest);
    let _ = rates_and_wall(&hw, dest);
}

/// One bounded, real write probe on `dest`'s drive. Returns `None` (and measures nothing) unless
/// there is comfortable headroom, a calibration must never be the thing that fills someone's disk.
/// The directory a write probe would run in, and whose volume a measurement belongs to. `dest` may
/// not exist yet, in which case its parent is where the bytes will land.
fn probe_dir(dest: &Path) -> Option<&Path> {
    if dest.is_dir() {
        Some(dest)
    } else {
        dest.parent()
    }
}

fn probe_wall_if_safe(dest: &Path) -> Option<f64> {
    const PROBE_MIB: usize = 512; // 4 x 128 MiB windows -> a median has something to work with
    const REQUIRED_FREE_MIB: u64 = 4096;
    let dir = probe_dir(dest).unwrap_or(Path::new("."));
    if hw::free_space_mib(dir)? < REQUIRED_FREE_MIB {
        return None;
    }
    let w = hw::measure_write_wall(dir, PROBE_MIB).ok()?;
    // The sustained figure is what planning cares about; fall back to burst if the probe was too
    // short to see a cliff.
    let mibs = if w.sustained_mibs > 0.0 {
        w.sustained_mibs
    } else {
        w.burst_mibs
    };
    (mibs > 0.0).then_some(mibs)
}

/// Extract `path` into `dest`. `pw` supplies passwords for encrypted entries; `sink` receives
/// progress and can request cancellation.
pub fn extract(
    path: &Path,
    dest: &Path,
    pw: Arc<dyn PasswordProvider>,
    opts: ExtractOptions,
    sink: &dyn ProgressSink,
) -> Result<Report> {
    let fmt = sniff::sniff_path(path)?;
    let mut reader = formats::open(path, fmt, pw)?;

    // Derive the plan from the archive's codec/block shape (entries borrow ends with this block).
    let plan = {
        let units = reader.as_random_access().and_then(|ra| ra.decode_units());
        let entries = reader.entries()?;
        // Profile the DESTINATION drive, not the process's current directory: the plan has to
        // describe the disk the bytes actually land on, which for an extraction onto another disk
        // is not the one Cram happens to have been run from.
        let hw = HwProfile::detect_for(dest);
        let (rates, wall) = rates_and_wall(&hw, dest);
        let units = units.unwrap_or_else(|| block_count(fmt, entries));
        let mut plan = hw::derive_plan(
            Op::Extract,
            plan_codec(fmt, entries),
            // The backend's own count where it has one (`.cram` packs); the entry list otherwise.
            units,
            &hw,
            Topology::SameDrive,
            &rates,
            wall,
        );
        // `CRAM_WORKERS=n` forces the pool width, for measuring what the plan is worth.
        //
        // There was no way to ask "what would this cost on four cores": `taskset` narrows which CPUs
        // the process may use without narrowing `hw.physical`, so the plan still asks for twenty-four
        // workers and simply gets them descheduled. That measures contention rather than the count.
        // Deliberately not a CLI flag; the planner's whole thesis is that it decides this better than
        // a person can, and an override belongs beside `CRAM_PROFILE` as a diagnostic. It is echoed
        // in the profile line below so it can never be quietly in effect during a benchmark.
        let forced = std::env::var("CRAM_WORKERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0);
        if let Some(n) = forced {
            plan.workers = n;
            plan.writers = plan.writers.min(n);
        }
        // Why this extraction ran the way it did, on request. Every input to the decision, because
        // the useless answer is the one that says what was chosen without saying what decided it.
        // Working out why 7z extraction used two cores on twelve and twelve on twenty-four took an
        // afternoon of bisecting with `taskset`; it is one line with this.
        if std::env::var_os("CRAM_PROFILE").is_some() {
            eprintln!(
                "cram: extract plan {:?} {:?} workers={} writers={} units={} \
                 logical={} physical={} decode={:.1} MiB/s wall={:.1} MiB/s random_access={} ({})",
                plan.bottleneck,
                plan.shape,
                plan.workers,
                plan.writers,
                units,
                hw.logical,
                hw.physical,
                rates.decode_rate(plan_codec(fmt, entries)),
                wall,
                reader.as_random_access().is_some(),
                plan.note,
            );
            if let Some(n) = forced {
                eprintln!(
                    "cram: FORCED workers={n} via CRAM_WORKERS (the plan did not choose this)"
                );
            }
        }
        plan
    };

    // Cancelling should leave the destination as it was found, so the engines record what they
    // bring into existence and nothing else. See `unwind`: a file that was already there is never
    // ours to remove, even when this run overwrote it.
    let created = unwind::CreatedLog::default();

    // Random-access (ZIP) → tuned parallel per-entry; everything else → sequential stream.
    let out = if reader.as_random_access().is_some() {
        let ra = reader.as_random_access().unwrap();
        parallel::run(ra, dest, plan.workers, opts.skip_existing, sink, &created)
    } else {
        sequential::run(reader.as_mut(), dest, opts.skip_existing, sink, &created)
    };

    // Only on cancellation. A run that failed part-way leaves its output alone: the user did not ask
    // for it to stop, and what landed may be the more useful half of a bad archive.
    let cancelled =
        matches!(out, Err(ArchiveError::Cancelled)) || matches!(&out, Ok(r) if r.cancelled);
    if cancelled {
        created.unwind();
    }
    out
}

/// A `Write` shared by both engine paths: reports written bytes to the sink and aborts when
/// cancellation is requested, so a long entry stops mid-stream.
pub(crate) struct ProgressWriter<'a, W: Write> {
    inner: W,
    sink: &'a dyn ProgressSink,
}

impl<'a, W: Write> ProgressWriter<'a, W> {
    pub(crate) fn new(inner: W, sink: &'a dyn ProgressSink) -> Self {
        Self { inner, sink }
    }

    /// The cancellation sentinel. Deliberately NOT `ErrorKind::Interrupted`: `io::copy` writes
    /// through `write_all`, which silently *retries* `Interrupted`; with a one-way cancel latch
    /// that spins forever at 100% CPU. `Other` propagates instead, unwinding the copy at once.
    fn cancelled_err() -> io::Error {
        io::Error::other("cancelled")
    }
}

impl<W: Write> Write for ProgressWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.sink.is_cancelled() {
            return Err(Self::cancelled_err());
        }
        let n = self.inner.write(buf)?;
        self.sink.on_bytes(n as u64);
        Ok(n)
    }
    /// Bail on cancel before entering the retry loop, belt-and-braces against `write_all`'s
    /// `Interrupted` retry semantics regardless of the sentinel kind chosen above.
    fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            if self.sink.is_cancelled() {
                return Err(Self::cancelled_err());
            }
            match self.inner.write(buf) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ))
                }
                Ok(n) => {
                    self.sink.on_bytes(n as u64);
                    buf = &buf[n..];
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
