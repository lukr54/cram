//! ZIP writer backend, the create counterpart to [`super::zip`]. Wraps the `zip` crate's
//! `ZipWriter` behind the incremental [`ArchiveWriter`] trait: `start_file` + stream the body,
//! `add_directory`, then `finish` writes the central directory.
//!
//! Encryption: **AES-256** (WinZip AE, via `with_aes_encryption`). The locked create fork also
//! offers a labeled-weak legacy ZipCrypto, but `zip` 8.x does not expose ZipCrypto *writing* in its
//! public API (`with_deprecated_encryption` is crate-private), so that path returns
//! [`ArchiveError::UnsupportedEncryption`] for now rather than silently downgrading the cipher.
//! ZIP cannot hide the file listing, so requesting [`HeaderMode::NamesToo`](crate::secret::HeaderMode)
//! for ZIP is **rejected** at create time rather than silently leaving every filename exposed.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{sync_channel, Receiver};
use std::time::{Instant, SystemTime};

use zip::result::ZipError;
use zip::write::{FileOptions, SimpleFileOptions};
use zip::{AesMode, CompressionMethod, ZipWriter as ZipCrateWriter};

use crate::error::{ArchiveError, Result};
use crate::format::Codec;
use crate::model::Entry;
use crate::probe;
use crate::secret::{HeaderMode, Secret, ZipCipher};
use crate::writer::{ArchiveWriter, CreateOptions, CreateReport, Level, WriteHint};

/// Files at/above 4 GiB (compressed or uncompressed) need the ZIP64 `large_file` flag.
const ZIP64_THRESHOLD: u64 = 0xFFFF_FFFF;

/// Largest entry the parallel path will hold in memory. Above this the entry is streamed on the
/// calling thread instead: one 500 MiB file buffered per worker is how a create turns into a swap
/// storm, and a handful of huge entries parallelise poorly anyway.
const PARALLEL_ENTRY_MAX: u64 = 64 << 20;

/// Ceiling on source bytes owned by in-flight jobs, and the real memory bound. Compression is
/// faster than the disk can feed it on a tree of small files, so without this the queue would run
/// ahead of the writer and the whole archive would be resident before the first byte landed.
/// Compressed output is held too, so peak residency is up to roughly twice this.
const PARALLEL_INFLIGHT_MAX: u64 = 128 << 20;

/// How many jobs may be outstanding, when the byte ceiling has not already stopped them.
///
/// This is deliberately far larger than the pool. It is not a memory bound -- `PARALLEL_INFLIGHT_MAX`
/// is -- it exists to hide head-of-line stalls: the writer thread blocks on the *oldest* job, and
/// entry durations on a real tree span three orders of magnitude, so a shallow queue leaves every
/// worker idle while the writer waits on one slow file. Measured on a 41,305-file tree, 16 threads:
/// depth 32 gave 11.24s, 128 gave 8.89s, 512 gave 7.81s, 2048 gave 6.68s, and 4096/8192 gave
/// nothing further. The knee is here.
const PARALLEL_DEPTH: usize = 2048;

/// Set to any value to compress ZIP entries on the calling thread, as this backend did before the
/// parallel path existed. The two produce byte-identical archives, so this is a diagnostic: if a
/// ZIP ever looks wrong, it says in one run whether the fan-out is implicated.
const ENV_SEQUENTIAL: &str = "CRAM_ZIP_SEQUENTIAL";

/// Overrides [`PARALLEL_ENTRY_MAX`], in bytes. Exists so the oversized-entry fallback can be tested
/// without writing a 64 MiB fixture.
const ENV_ENTRY_MAX: &str = "CRAM_ZIP_ENTRY_MAX";

/// Overrides [`PARALLEL_DEPTH`]. The knee is machine- and tree-shaped, so it is worth being able to
/// re-find it on hardware that is not this one.
const ENV_DEPTH: &str = "CRAM_ZIP_DEPTH";

fn map_zip_write(e: ZipError) -> ArchiveError {
    match e {
        ZipError::Io(io) => ArchiveError::Io(io),
        other => ArchiveError::Backend(format!("zip write: {other}")),
    }
}

/// Build the per-file options: compression method + level, plus AES-256 when a password is set.
/// Free function (not a `&self` method) so it borrows only the password field, leaving the writer
/// handle free to borrow mutably for `start_file`.
fn file_options(
    method: CompressionMethod,
    level: Option<i64>,
    pw: Option<&Secret>,
    large: bool,
    modified: Option<SystemTime>,
) -> FileOptions<'_, ()> {
    let mut opts = SimpleFileOptions::default().compression_method(method);
    // STORE takes no level; only compressing methods do.
    if method != CompressionMethod::Stored {
        opts = opts.compression_level(level);
    }
    // Feed zopfli in blocks the size it asks for. The zip crate defaults this to 32 KiB; zopfli's own
    // `new_buffered` uses `ZOPFLI_MASTER_BLOCK_SIZE`, 1,000,000, and its docs say large chunks are
    // "necessary for decent performance and good compression ratio".
    //
    // **Measured as a no-op today, and kept anyway.** Silesia came out byte-identical at 32 KiB and
    // at 1 MB, because the buffer never fills: `compress_one` hands the encoder a whole entry in one
    // `write_all`, and a `BufWriter` passes a write larger than its capacity straight through. So
    // zopfli already sees each entry entire, which is the case the buffer exists to approximate.
    // This matters the moment anything writes an entry in pieces — the streaming path for entries
    // over `PARALLEL_ENTRY_MAX` is one — and it is the difference between that path quietly losing
    // ratio and not.
    if matches!(level, Some(n) if n >= ZOPFLI_MIN_LEVEL) {
        opts = opts.with_zopfli_buffer(Some(ZOPFLI_BLOCK));
    }
    if large {
        opts = opts.large_file(true);
    }
    if let Some(dt) = zip_datetime(modified) {
        opts = opts.last_modified_time(dt);
    }
    match pw {
        Some(s) => opts.with_aes_encryption(AesMode::Aes256, s.expose()),
        None => opts,
    }
}

/// The entry's mtime as a ZIP `DateTime`, or `None` if absent or outside ZIP's DOS-time range (which
/// starts at 1980, an older or missing timestamp is simply not stored, leaving the crate default).
/// Sourced from the input file's mtime (UTC), so an identical input tree still yields an identical zip.
///
/// The input may be a mtime a *reader* surfaced from an untrusted archive (via `convert`), so it must
/// be bounded before `time::OffsetDateTime::from(SystemTime)`, that conversion **panics** for a time
/// beyond `time`'s ±9999-year range. A far-future or pre-epoch timestamp is dropped, not stored.
fn zip_datetime(modified: Option<SystemTime>) -> Option<zip::DateTime> {
    let t = modified?;
    // `duration_since` is `Err` for a pre-1970 time; DOS time can't hold those anyway.
    let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    const MAX_SANE_UNIX: u64 = 32_503_680_000; // ~year 3000, well inside `time`'s range, absurdly future
    if secs > MAX_SANE_UNIX {
        return None;
    }
    let odt = time::OffsetDateTime::from(t);
    zip::DateTime::from_date_and_time(
        odt.year() as u16,
        u8::from(odt.month()),
        odt.day(),
        odt.hour(),
        odt.minute(),
        odt.second(),
    )
    .ok()
}

/// The ZIP name for an entry: the normalized-safe relative path with forward slashes (ZIP uses `/`).
fn zip_name(entry: &Entry) -> String {
    entry.path.safe().to_string_lossy().replace('\\', "/")
}

/// One worker's finished entry: a complete single-entry ZIP held in memory. The bytes inside are
/// already deflated, so the writer thread hands them to `raw_copy_file` rather than compressing
/// again. Going through a real one-entry archive rather than raw buffers keeps every header field
/// the `zip` crate's own business, which is the difference between a fast path and a second,
/// subtly-different ZIP encoder to maintain.
struct DoneEntry {
    zip_bytes: Vec<u8>,
    in_bytes: u64,
    stored: bool,
}

/// An entry that has been accepted but not yet written, held in submission order.
///
/// Directories sit in the same queue rather than being written immediately. A directory is cheap,
/// but the walk yields it just before its files, so writing it straight through would mean draining
/// every in-flight job first and turning each directory into a barrier.
enum Pending {
    Dir {
        name: String,
        modified: Option<SystemTime>,
    },
    File {
        rx: Receiver<Result<DoneEntry>>,
        /// Charged against the in-flight ceiling until the entry is written.
        bytes: u64,
    },
}

pub struct ZipArchiveWriter {
    /// `Option` so `finish` can take ownership out of `&mut self`-shaped call sites.
    zw: Option<ZipCrateWriter<BufWriter<File>>>,
    method: CompressionMethod,
    level: Option<i64>,
    /// AES-256 password, or `None` for an unencrypted archive.
    aes_pw: Option<Secret>,
    entries: u64,
    in_bytes: u64,
    /// Entries the adaptive probe stored verbatim (incompressible), for the report.
    stored: u64,
    start: Instant,
    /// Whether to fan compression out across the rayon pool. Off for encrypted archives: an AES
    /// entry carries its own framing and extra field, and `raw_copy_file` copying that correctly is
    /// not something to assume without a test that proves it round-trips.
    parallel: bool,
    /// Mirrors the engine's `adaptive` flag. With `takes_paths` on, the engine stops probing
    /// store-vs-compress inline, so the decision has to be made here or every incompressible file
    /// would get DEFLATEd and the archive would change.
    adaptive: bool,
    /// Submitted, not yet written. Drained from the front, which is what preserves entry order.
    pending: VecDeque<Pending>,
    inflight_bytes: u64,
    /// Maximum queued jobs. Two per worker keeps the pool fed across the gaps where the writer
    /// thread is busy.
    depth: usize,
    /// Effective [`PARALLEL_ENTRY_MAX`], overridable via [`ENV_ENTRY_MAX`].
    entry_max: u64,
    /// Writer-thread time split, nanoseconds, printed under `CRAM_PROFILE`. `recv` is the writer
    /// waiting on a worker (so: not enough parallelism upstream); `parse` and `copy` are what the
    /// writer itself costs per entry, and together they are the pipeline's hard ceiling.
    prof_recv: u128,
    prof_parse: u128,
    prof_copy: u128,
}

impl ZipArchiveWriter {
    pub fn create(path: &Path, opts: &CreateOptions) -> Result<Self> {
        // Encryption fork: AES-256 supported; legacy ZipCrypto write is not exposed by `zip` 8.x.
        let aes_pw = match &opts.encrypt {
            None => None,
            Some(spec) => match spec.zip_cipher {
                ZipCipher::Aes256 => Some(spec.password.clone()),
                ZipCipher::LegacyZipCrypto => {
                    return Err(ArchiveError::UnsupportedEncryption);
                }
            },
        };

        // ZIP encrypts file *contents* but not the central-directory names. Honoring a "hide names"
        // request silently would expose every filename while the user believes they're hidden, so
        // refuse it here. .7z and .cram encrypt the listing and should be used instead.
        if let Some(spec) = &opts.encrypt {
            if spec.header == HeaderMode::NamesToo {
                return Err(ArchiveError::Backend(
                    "ZIP cannot hide file names, use .7z or .cram to encrypt the file listing"
                        .into(),
                ));
            }
        }

        // Codec: STORE only when explicitly asked for no compression; otherwise DEFLATE.
        let method = match opts.codec {
            Some(Codec::None) => CompressionMethod::Stored,
            _ => CompressionMethod::Deflated,
        };
        let level = deflate_level(opts.level);

        let file = File::create(path)?;
        // 1 MiB, not the default 8 KiB. Every entry's compressed bytes are memcpy'd through here by
        // `raw_copy_file`, so on a tree of tens of thousands of small files the default turns a
        // few-hundred-megabyte archive into tens of thousands of write syscalls.
        let zw = ZipCrateWriter::new(BufWriter::with_capacity(1 << 20, file));
        let parallel = aes_pw.is_none() && std::env::var_os(ENV_SEQUENTIAL).is_none();
        let entry_max = std::env::var(ENV_ENTRY_MAX)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(PARALLEL_ENTRY_MAX);
        Ok(Self {
            zw: Some(zw),
            method,
            level,
            aes_pw,
            entries: 0,
            in_bytes: 0,
            stored: 0,
            start: Instant::now(),
            parallel,
            // Same condition the engine uses to decide whether to probe at all.
            adaptive: opts.level == Level::Auto && opts.codec.is_none(),
            pending: VecDeque::new(),
            inflight_bytes: 0,
            depth: std::env::var(ENV_DEPTH)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(PARALLEL_DEPTH)
                .max(2),
            entry_max,
            prof_recv: 0,
            prof_parse: 0,
            prof_copy: 0,
        })
    }

    /// Write the oldest queued entry, blocking until its worker has finished. Returns `Ok(false)`
    /// when the queue was already empty.
    fn drain_one(&mut self) -> Result<bool> {
        let Some(item) = self.pending.pop_front() else {
            return Ok(false);
        };
        let zw = self
            .zw
            .as_mut()
            .ok_or_else(|| ArchiveError::Backend("zip writer already finished".into()))?;
        match item {
            Pending::Dir { name, modified } => {
                let mut opts = SimpleFileOptions::default();
                if let Some(dt) = zip_datetime(modified) {
                    opts = opts.last_modified_time(dt);
                }
                zw.add_directory(name, opts).map_err(map_zip_write)?;
                self.entries += 1;
            }
            Pending::File { rx, bytes } => {
                self.inflight_bytes = self.inflight_bytes.saturating_sub(bytes);
                // A worker that panicked drops its sender; report that rather than hanging or
                // silently omitting the file from the archive.
                let t_recv = Instant::now();
                let done = rx
                    .recv()
                    .map_err(|_| ArchiveError::Backend("zip: compression worker died".into()))??;
                self.prof_recv += t_recv.elapsed().as_nanos();
                let t_parse = Instant::now();
                let mut inner =
                    zip::ZipArchive::new(Cursor::new(done.zip_bytes)).map_err(map_zip_write)?;
                let file = inner.by_index_raw(0).map_err(map_zip_write)?;
                self.prof_parse += t_parse.elapsed().as_nanos();
                let t_copy = Instant::now();
                zw.raw_copy_file(file).map_err(map_zip_write)?;
                self.prof_copy += t_copy.elapsed().as_nanos();
                self.entries += 1;
                self.in_bytes += done.in_bytes;
                if done.stored {
                    self.stored += 1;
                }
            }
        }
        Ok(true)
    }

    /// Write queued entries until the queue is within both ceilings.
    fn drain_to_limits(&mut self) -> Result<()> {
        while self.pending.len() >= self.depth || self.inflight_bytes >= PARALLEL_INFLIGHT_MAX {
            if !self.drain_one()? {
                break;
            }
        }
        Ok(())
    }

    /// Write everything queued. Called before any path that must write directly to the underlying
    /// writer, and by `finish`.
    fn drain_all(&mut self) -> Result<()> {
        while self.drain_one()? {}
        Ok(())
    }

    /// Stream one entry straight into the archive on the calling thread. This is the original
    /// sequential writer, unchanged: it still serves `conv`, encrypted archives and any entry too
    /// large to buffer. Callers must have drained the queue first.
    fn add_file_direct(
        &mut self,
        entry: &Entry,
        body: &mut dyn io::Read,
        hint: WriteHint,
    ) -> Result<()> {
        // Adaptive store: an incompressible entry is written STORE even under a compressing level,
        // saving CPU and avoiding the slight growth DEFLATE adds to already-compressed data. When
        // the whole archive is already STORE (explicit `--store`), the hint is a no-op.
        let adaptive_store = hint.store && self.method != CompressionMethod::Stored;
        let method = if hint.store {
            CompressionMethod::Stored
        } else {
            self.method
        };
        // ZIP64 is needed when EITHER size crosses 4 GiB. The compressed stream can exceed the raw
        // size: AES-256 framing adds 28 bytes, and DEFLATE on incompressible data grows ~0.03% plus
        // block overhead, so a raw size just under the threshold (e.g. 0xFFFF_FFF0 stored+AES)
        // overflows the 32-bit compressed-size field mid-write and the zip crate hard-errors after
        // streaming the whole entry. Decide with a worst-case margin instead of the raw size alone.
        let large = entry.size.saturating_add(entry.size / 1000 + (64 << 10)) >= ZIP64_THRESHOLD;
        let opts = file_options(
            method,
            self.level,
            self.aes_pw.as_ref(),
            large,
            entry.modified,
        );
        let name = zip_name(entry);
        let zw = self
            .zw
            .as_mut()
            .ok_or_else(|| ArchiveError::Backend("zip writer already finished".into()))?;
        zw.start_file(name, opts).map_err(map_zip_write)?;
        let n = io::copy(body, zw)?;
        self.entries += 1;
        self.in_bytes += n;
        if adaptive_store {
            self.stored += 1;
        }
        Ok(())
    }
}

/// Core-time spent inside the workers, summed across threads. Printed under `CRAM_PROFILE`.
/// Compared against wall these say which phase is actually running wide, which is not something
/// the writer thread's own wait time can distinguish.
mod wprof {
    use std::sync::atomic::AtomicU64;
    pub static READ_NANOS: AtomicU64 = AtomicU64::new(0);
    pub static PROBE_NANOS: AtomicU64 = AtomicU64::new(0);
    pub static ZIP_NANOS: AtomicU64 = AtomicU64::new(0);
    pub static FILES: AtomicU64 = AtomicU64::new(0);
}

/// Compress one file into a complete single-entry ZIP, off the writer thread.
///
/// The store-vs-compress decision is made here from the bytes already in hand. It must match
/// `probe::classify_file` exactly, the same way the engine's inline probe does, or archives written
/// through this path would differ from ones written through `add_file`.
fn compress_one(
    name: String,
    path: PathBuf,
    modified: Option<SystemTime>,
    method: CompressionMethod,
    level: Option<i64>,
    adaptive: bool,
    hint: WriteHint,
) -> Result<DoneEntry> {
    let t_read = Instant::now();
    let data = std::fs::read(&path)
        .map_err(|e| ArchiveError::Backend(format!("{}: {e}", path.display())))?;
    wprof::READ_NANOS.fetch_add(t_read.elapsed().as_nanos() as u64, Relaxed);
    wprof::FILES.fetch_add(1, Relaxed);

    let t_probe = Instant::now();
    let mut hint = hint;
    if adaptive && !data.is_empty() {
        match probe::ext_only_verdict(&path) {
            Some(verdict) => {
                hint = WriteHint {
                    store: verdict.is_store(),
                }
            }
            None if data.len() as u64 >= probe::PROBE_MIN_SAMPLE => {
                let n = data.len().min(probe::PROBE_SAMPLE_BYTES as usize);
                hint = WriteHint {
                    store: probe::sample_verdict(&data[..n]).is_store(),
                };
            }
            None => {}
        }
    }
    let adaptive_store = hint.store && method != CompressionMethod::Stored;
    let entry_method = if hint.store {
        CompressionMethod::Stored
    } else {
        method
    };
    wprof::PROBE_NANOS.fetch_add(t_probe.elapsed().as_nanos() as u64, Relaxed);

    // No `large_file`: PARALLEL_ENTRY_MAX keeps every entry on this path far below 4 GiB, and the
    // caller streams anything bigger.
    let t_zip = Instant::now();
    let opts = file_options(entry_method, level, None, false, modified);
    let mut inner = ZipCrateWriter::new(Cursor::new(Vec::with_capacity(data.len() / 2 + 512)));
    inner.start_file(name, opts).map_err(map_zip_write)?;
    io::Write::write_all(&mut inner, &data)?;
    let cursor = inner.finish().map_err(map_zip_write)?;
    wprof::ZIP_NANOS.fetch_add(t_zip.elapsed().as_nanos() as u64, Relaxed);
    Ok(DoneEntry {
        zip_bytes: cursor.into_inner(),
        in_bytes: data.len() as u64,
        stored: adaptive_store,
    })
}

/// Map the abstract [`Level`] onto DEFLATE's 0–9 scale (`None` = the crate default, 6).
///
/// **Above 9 the zip crate switches encoder**, which is how [`Level::Tiny`] reaches zopfli: 0–9 go to
/// flate2, 10–264 to zopfli with `level - 9` iterations. 24 is therefore 15 iterations, which is both
/// the zip crate's own default when zopfli is the only encoder and the reference zopfli CLI's
/// default. The output is ordinary DEFLATE — every unzip reads it — just searched for much harder.
fn deflate_level(level: Level) -> Option<i64> {
    match level {
        Level::Auto | Level::Balanced => None,
        Level::Fastest => Some(1),
        Level::Best | Level::Cold => Some(9),
        Level::Tiny => Some(ZOPFLI_LEVEL),
        Level::Explicit(n) => Some((n as i64).clamp(0, 9)),
    }
}

/// 15 zopfli iterations. See [`deflate_level`].
const ZOPFLI_LEVEL: i64 = 24;

/// The level at or above which the zip crate switches to zopfli.
const ZOPFLI_MIN_LEVEL: i64 = 10;

/// `zopfli::util::ZOPFLI_MASTER_BLOCK_SIZE`, which is what zopfli's own `new_buffered` uses and what
/// its docs call necessary for a good ratio. Not re-exported by the zip crate, so it is repeated
/// here; if it ever diverges the cost is ratio, not correctness.
const ZOPFLI_BLOCK: usize = 1_000_000;

impl ArchiveWriter for ZipArchiveWriter {
    fn takes_paths(&self) -> bool {
        self.parallel
    }

    fn add_path(&mut self, entry: &Entry, path: &Path, hint: WriteHint) -> Result<()> {
        // Too big to hold in memory: drain so ordering is preserved, then stream it on this thread
        // exactly as the sequential path always did.
        if !self.parallel || entry.size > self.entry_max {
            self.drain_all()?;
            let file = File::open(path)
                .map_err(|e| ArchiveError::Backend(format!("{}: {e}", path.display())))?;
            // The probe has to happen here too. Turning `takes_paths` on stopped the engine doing
            // it inline, so without this an oversized incompressible entry arrives with the default
            // Compress hint and gets DEFLATEd where it used to be STOREd -- which silently grew a
            // 100 MB random file's archive by 15 KB and made this path's output differ from the
            // sequential writer's. Same order of checks as `probe::classify_file`, sampling from
            // the handle already open and handing the sampled bytes back so nothing is read twice.
            let mut head = Vec::new();
            let mut hint = hint;
            if self.adaptive && entry.size > 0 {
                match probe::ext_only_verdict(path) {
                    Some(verdict) => {
                        hint = WriteHint {
                            store: verdict.is_store(),
                        }
                    }
                    None if entry.size >= probe::PROBE_MIN_SAMPLE => {
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
            let mut body = Cursor::new(head).chain(file);
            return self.add_file_direct(entry, &mut body, hint);
        }

        self.drain_to_limits()?;

        let (tx, rx) = sync_channel(1);
        let name = zip_name(entry);
        let owned = path.to_path_buf();
        let (modified, method, level, adaptive) =
            (entry.modified, self.method, self.level, self.adaptive);
        // FIFO, not `rayon::spawn`. A plain spawn goes onto a worker's LIFO local queue, so the most
        // recently submitted entry runs first -- while the writer thread is blocked on the OLDEST
        // one, which is then the last to be picked up. The queue stays full, the workers stay busy,
        // and the pipeline still crawls, because the head of the line is always the least urgent
        // thing in it.
        rayon::spawn_fifo(move || {
            // The receiver is only dropped when the whole create is being torn down, so a failed
            // send means nobody is listening any more and there is nothing useful to do with it.
            let _ = tx.send(compress_one(
                name, owned, modified, method, level, adaptive, hint,
            ));
        });
        self.pending.push_back(Pending::File {
            rx,
            bytes: entry.size,
        });
        self.inflight_bytes = self.inflight_bytes.saturating_add(entry.size);
        Ok(())
    }

    fn add_file(&mut self, entry: &Entry, body: &mut dyn io::Read, hint: WriteHint) -> Result<()> {
        // Reached by `conv` (which streams out of a reader, with no path to hand over) and by the
        // oversized-entry fallback above. Either way the queue has to be flat first or this entry
        // would jump ahead of work already accepted.
        self.drain_all()?;
        self.add_file_direct(entry, body, hint)
    }
    fn add_dir(&mut self, entry: &Entry) -> Result<()> {
        // Queued rather than written, so it keeps its place among the files around it without
        // forcing the in-flight jobs to finish first.
        self.pending.push_back(Pending::Dir {
            name: zip_name(entry),
            modified: entry.modified,
        });
        self.drain_to_limits()
    }

    fn finish(mut self: Box<Self>) -> Result<CreateReport> {
        self.drain_all()?;
        if std::env::var_os("CRAM_PROFILE").is_some() {
            let ms = |n: u128| n as f64 / 1e6;
            let per = |n: u128| {
                if self.entries > 0 {
                    n as f64 / self.entries as f64 / 1e3
                } else {
                    0.0
                }
            };
            eprintln!("-- zip writer thread --------------------------------------------");
            eprintln!(
                "wait on worker  {:9.1} ms   {:6.1} us/entry",
                ms(self.prof_recv),
                per(self.prof_recv)
            );
            eprintln!(
                "parse 1-entry   {:9.1} ms   {:6.1} us/entry",
                ms(self.prof_parse),
                per(self.prof_parse)
            );
            eprintln!(
                "raw_copy_file   {:9.1} ms   {:6.1} us/entry",
                ms(self.prof_copy),
                per(self.prof_copy)
            );
            let files = wprof::FILES.load(Relaxed).max(1);
            let core = |a: &std::sync::atomic::AtomicU64| a.load(Relaxed) as f64 / 1e6;
            eprintln!(
                "-- zip workers (summed over threads) -- pool {} threads, queue depth {} --",
                rayon::current_num_threads(),
                self.depth
            );
            eprintln!(
                "read file       {:9.1} ms   {:6.1} us/file",
                core(&wprof::READ_NANOS),
                core(&wprof::READ_NANOS) * 1e3 / files as f64
            );
            eprintln!(
                "probe           {:9.1} ms   {:6.1} us/file",
                core(&wprof::PROBE_NANOS),
                core(&wprof::PROBE_NANOS) * 1e3 / files as f64
            );
            eprintln!(
                "build 1-entry   {:9.1} ms   {:6.1} us/file   ({} files)",
                core(&wprof::ZIP_NANOS),
                core(&wprof::ZIP_NANOS) * 1e3 / files as f64,
                files
            );
        }
        let zw = self
            .zw
            .take()
            .ok_or_else(|| ArchiveError::Backend("zip writer already finished".into()))?;
        let buf = zw.finish().map_err(map_zip_write)?;
        // Flush the BufWriter down to the file and measure the final archive size.
        let file = buf
            .into_inner()
            .map_err(|e| ArchiveError::Io(e.into_error()))?;
        let out_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(CreateReport {
            entries: self.entries,
            in_bytes: self.in_bytes,
            out_bytes,
            stored: self.stored,
            dedup_saved: 0,
            elapsed: self.start.elapsed(),
            // Filled in by the engine walk, which is the only thing that sees the source tree.
            skipped_links: Vec::new(),
        })
    }
}

#[cfg(test)]
mod mtime_guard_tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn zip_datetime_bounds_and_survives_hostile_input() {
        assert!(zip_datetime(None).is_none());
        // Pre-1980 (DOS epoch) has no representation -> dropped, not stored.
        assert!(zip_datetime(Some(UNIX_EPOCH)).is_none());
        // A far-future time (~year 5000) must be rejected BEFORE time::OffsetDateTime::from, which
        // panics beyond +/-9999 - the bound makes this return None, never crash.
        let huge = UNIX_EPOCH + Duration::from_secs(95_617_584_000);
        assert!(zip_datetime(Some(huge)).is_none());
        // A real 2020 timestamp converts.
        let real = UNIX_EPOCH + Duration::from_secs(1_577_934_246);
        assert!(zip_datetime(Some(real)).is_some());
    }

    /// `--tiny` has to clear the zip crate's encoder switch, which is a plain numeric threshold:
    /// 0–9 go to flate2, 10 and above to zopfli with `level - 9` iterations. Getting this wrong
    /// does not fail — it silently writes an ordinary level-9 archive and the flag does nothing.
    /// A level too low to reach zopfli fails the **build**, not a test somebody might not run.
    /// Getting it wrong is silent otherwise: `--tiny` would write an ordinary level-9 archive.
    const _TINY_REACHES_ZOPFLI: () = assert!(ZOPFLI_LEVEL >= ZOPFLI_MIN_LEVEL);

    #[test]
    fn tiny_asks_for_a_level_that_reaches_zopfli() {
        assert_eq!(deflate_level(Level::Tiny), Some(ZOPFLI_LEVEL));
        // Every other rung must stay on the fast encoder, or --small would silently become --tiny.
        for lvl in [
            Level::Auto,
            Level::Balanced,
            Level::Fastest,
            Level::Best,
            Level::Cold,
        ] {
            if let Some(n) = deflate_level(lvl) {
                assert!(
                    n < ZOPFLI_MIN_LEVEL,
                    "{lvl:?} maps to {n}, which would reach zopfli"
                );
            }
        }
    }

    /// The point of zopfli is a smaller archive that is still an ordinary `.zip`. Assert both: the
    /// bytes shrink against `--small`, and the result round-trips through the normal reader.
    #[test]
    fn tiny_writes_a_smaller_zip_that_still_reads_back() {
        use crate::model::{EntryKind, EntryPath};

        let dir = std::env::temp_dir().join(format!("cram-tiny-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Compressible but not trivially so, and big enough that the extra search has something to
        // find: a few hundred KiB of pseudo-English from a modest vocabulary.
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let vocab: Vec<String> = (0..600)
            .map(|_| {
                let n = 3 + (rng() % 8) as usize;
                (0..n)
                    .map(|_| (b'a' + (rng() % 26) as u8) as char)
                    .collect()
            })
            .collect();
        let mut body = Vec::with_capacity(400 << 10);
        while body.len() < (400 << 10) {
            body.extend_from_slice(vocab[(rng() % vocab.len() as u64) as usize].as_bytes());
            body.push(b' ');
        }

        let write = |level: Level, name: &str| -> u64 {
            let path = dir.join(name);
            let mut opts = CreateOptions {
                level,
                ..Default::default()
            };
            opts.total_bytes = Some(body.len() as u64);
            let mut w = Box::new(ZipArchiveWriter::create(&path, &opts).unwrap());
            let entry = Entry {
                index: 0,
                path: EntryPath::from_raw("doc.txt").unwrap(),
                kind: EntryKind::File,
                size: body.len() as u64,
                compressed_size: None,
                modified: None,
                unix_mode: None,
                crc32: None,
                encrypted: false,
            };
            w.add_file(&entry, &mut &body[..], WriteHint::default())
                .unwrap();
            w.finish().unwrap();
            std::fs::metadata(&path).unwrap().len()
        };

        let small = write(Level::Cold, "small.zip");
        let tiny = write(Level::Tiny, "tiny.zip");
        assert!(
            tiny < small,
            "zopfli must beat level 9, got {tiny} against {small} — if they are equal the level \
             never reached the zopfli encoder"
        );

        // Still an ordinary zip: read it back through the normal reader and compare the bytes.
        let f = std::fs::File::open(dir.join("tiny.zip")).unwrap();
        let mut zr = zip::ZipArchive::new(f).unwrap();
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut zr.by_index(0).unwrap(), &mut got).unwrap();
        assert_eq!(got, body, "the zopfli archive must decode to the original");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
