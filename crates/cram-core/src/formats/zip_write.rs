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

use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::{self, BufWriter, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use zip::result::ZipError;
use zip::write::{FullFileOptions, PreparedZipFile, SimpleFileOptions, ZipFileBuilder};
use zip::{AesMode, CompressionMethod, ZipWriter as ZipCrateWriter};

use super::zip_time;
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
/// parallel path existed. For an unencrypted archive the two produce the same bytes, so this is a
/// diagnostic: if a ZIP ever looks wrong, it says in one run whether the fan-out is implicated. One
/// case is known to differ and is harmless: at `--tiny`, a file over 1,000,000 bytes (see
/// [`write_body`]). An encrypted archive never matches another, since each entry's salt is random.
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

/// Build the per-file options: compression method + level, the modification time, plus AES-256 when
/// a password is set. Free function (not a `&self` method) so it borrows only the password field,
/// leaving the writer handle free to borrow mutably for `start_file`.
fn file_options(
    method: CompressionMethod,
    level: Option<i64>,
    pw: Option<&Secret>,
    large: bool,
    modified: Option<SystemTime>,
) -> Result<FullFileOptions<'_, '_>> {
    let mut opts = SimpleFileOptions::default().compression_method(method);
    // STORE takes no level; only compressing methods do.
    if method != CompressionMethod::Stored {
        opts = opts.compression_level(level);
    }
    // Feed zopfli in blocks the size it asks for. The zip crate defaults this to 32 KiB; zopfli's own
    // `new_buffered` uses `ZOPFLI_MASTER_BLOCK_SIZE`, 1,000,000, and its docs say large chunks are
    // "necessary for decent performance and good compression ratio".
    //
    // **This governs the streaming path only, and that is not a defect.** A `BufWriter` passes any
    // write at least as large as its capacity straight through, so a caller that hands over a whole
    // entry bypasses the buffer entirely — which is what `compress_one` does, and why setting this
    // once measured byte-identical at 32 KiB and at 1 MB. Both settings were being bypassed. The
    // block size that path actually gets is chosen in `write_body`, deliberately; here it is what
    // the streaming path for entries over `PARALLEL_ENTRY_MAX` gets, where `io::copy` writes 8 KiB
    // at a time and the buffer does fill.
    if matches!(level, Some(n) if n >= ZOPFLI_MIN_LEVEL) {
        opts = opts.with_zopfli_buffer(Some(ZOPFLI_BLOCK));
    }
    if large {
        opts = opts.large_file(true);
    }
    if let Some(dt) = zip_time::dos_for_write(modified) {
        opts = opts.last_modified_time(dt);
    }
    let opts = match pw {
        Some(s) => opts.with_aes_encryption(AesMode::Aes256, s.expose()),
        None => opts,
    };
    with_ext_timestamp(opts.into_full_options(), modified)
}

/// Attach the Info-ZIP extended timestamp (`0x5455`, UTC seconds) to both the local and the central
/// header, when the time fits it. The DOS fields beside it hold local time; see [`zip_time`].
///
/// Every writer path goes through here: the streamed one, both shapes of entry the parallel path
/// builds (see [`DoneBody`]), and directories. The field reaches the central header on all of
/// them but one: an encrypted entry written through the parallel path has it in its local header
/// only, for the reason given on [`DoneBody::Archive`].
///
/// The mtime may have come from an untrusted archive through `convert`; [`zip_time`] bounds it
/// before any calendar conversion, and a time it cannot hold is dropped, not stored.
fn with_ext_timestamp<'k, 'n>(
    mut opts: FullFileOptions<'k, 'n>,
    modified: Option<SystemTime>,
) -> Result<FullFileOptions<'k, 'n>> {
    if let Some(payload) = zip_time::ext_timestamp_payload(modified) {
        opts.add_extra_field(zip_time::EXT_TIMESTAMP_ID, payload, false)
            .map_err(map_zip_write)?;
    }
    Ok(opts)
}

/// The ZIP name for an entry: the normalized-safe relative path with forward slashes (ZIP uses `/`).
fn zip_name(entry: &Entry) -> String {
    entry.path.safe().to_string_lossy().replace('\\', "/")
}

/// One worker's finished entry, already compressed, in one of two shapes. Either way the header
/// fields stay the `zip` crate's own business, which is the difference between a fast path and a
/// second, subtly-different ZIP encoder to maintain.
enum DoneBody {
    /// Compressed by a [`ZipFileBuilder`]. `add_prepared_file` writes both headers from the options
    /// the worker chose and copies the compressed bytes verbatim, so the `0x5455` timestamp lands in
    /// the local and the central header, and the headers are byte for byte what the streamed path
    /// writes. (So are the compressed bytes, with the one exception [`write_body`] describes.)
    /// `name` is the entry's name, kept beside the file because `zip` 9.0.5 does not expose it and
    /// the writer has to claim it.
    Prepared { name: String, file: PreparedZipFile },
    /// A complete one-entry ZIP held in memory, for an encrypted entry: a `ZipFileBuilder` refuses
    /// encryption, so the worker runs a whole `ZipWriter` and the writer thread splices the result
    /// in with `merge_archive`.
    ///
    /// **This shape loses the `0x5455` timestamp from the central header.** `merge_archive` copies
    /// the local header and data as written, so the field stays there, but it writes the central
    /// header from the fields it parsed, and `zip` 9 parses `0x5455` into a typed field that it
    /// then writes back as nothing. (`zip` 8 kept the extra-field bytes it read and wrote them
    /// back.) `raw_copy_file` is no way round it: it rebuilds both headers the same way and drops
    /// the field from the local one as well.
    ///
    /// An encrypted entry on this path is therefore dated by its DOS fields, like an entry from a
    /// tool that writes no timestamp field, with what that brings. Read back by this crate: local
    /// time in two-second steps; an hour late for a time in the hour that repeats when summer time
    /// ends; and for a file dated before 1980, which the DOS fields cannot hold, the time the
    /// archive was written. Extracted by 7-Zip: an hour off when the file's date and the day of
    /// extraction fall on different sides of a summer-time change. The streamed path has none of
    /// this. `encrypted_entries_keep_the_timestamp_in_the_local_header_only` pins the missing field
    /// and fails the day the crate writes it back, at which point these two paragraphs go.
    Archive(Vec<u8>),
}

struct DoneEntry {
    body: DoneBody,
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
    /// AES-256 password, or `None` for an unencrypted archive. Behind an `Arc` because the workers
    /// need it too: `Secret` wraps a `Zeroizing<String>` and is deliberately not `Clone`, so sharing
    /// one is the way to avoid N plaintext copies to zeroize.
    aes_pw: Option<Arc<Secret>>,
    entries: u64,
    in_bytes: u64,
    /// Entries the adaptive probe stored verbatim (incompressible), for the report.
    stored: u64,
    start: Instant,
    /// Whether to fan compression out across the rayon pool.
    ///
    /// This was off for encrypted archives, on the grounds that an AES entry carries its own framing
    /// and extra field and the copy into the main archive preserving that was not something to
    /// assume without a test. The test exists (`aes_survives_the_parallel_path`) and passes; it was
    /// written against `raw_copy_file` and still passes now that the copy is `merge_archive`, which
    /// copies the local header and data verbatim and keeps the parsed AES fields for the central
    /// header. Each entry still gets its own salt, because each worker runs its own `start_file`.
    /// Only encrypted entries are copied that way now; see [`DoneBody`].
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
    /// writer itself costs per entry, and together they are the pipeline's hard ceiling. `parse` is
    /// spent on encrypted entries only: the others arrive prepared, with nothing to parse.
    prof_recv: u128,
    prof_parse: u128,
    prof_copy: u128,
    /// Every name written so far, files and directories (with their trailing `/`).
    ///
    /// The zip crate refuses a duplicate name in `start_file`, `add_directory` and
    /// `add_prepared_file`, but `merge_archive`, which encrypted entries go through, does not check:
    /// a second entry under a name already written would replace the first in the central directory
    /// and leave its data unreachable. The check is made here for every path, so a duplicate is
    /// reported the same way whichever one the entry took.
    names: HashSet<Box<str>>,
}

impl ZipArchiveWriter {
    pub fn create(path: &Path, opts: &CreateOptions) -> Result<Self> {
        // Encryption fork: AES-256 supported; legacy ZipCrypto write is not exposed by `zip` 8.x.
        let aes_pw = match &opts.encrypt {
            None => None,
            Some(spec) => match spec.zip_cipher {
                ZipCipher::Aes256 => Some(Arc::new(spec.password.clone())),
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
        // 1 MiB, not the default 8 KiB. Every parallel entry's compressed bytes are memcpy'd through
        // here by `add_prepared_file` or `merge_archive` (streamed entries are written through it
        // too), so on a tree of tens of thousands of small files the default turns a
        // few-hundred-megabyte archive into tens of thousands of write syscalls.
        let zw = ZipCrateWriter::new(BufWriter::with_capacity(1 << 20, file));
        let parallel = std::env::var_os(ENV_SEQUENTIAL).is_none();
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
            names: HashSet::new(),
        })
    }

    /// Record `name` as written, or fail if it already was. Takes the set rather than `&mut self`
    /// so it can run while the zip writer is borrowed.
    fn claim_name(names: &mut HashSet<Box<str>>, name: &str) -> Result<()> {
        if names.insert(name.into()) {
            Ok(())
        } else {
            Err(ArchiveError::Backend(format!(
                "zip write: Duplicate filename: {name}"
            )))
        }
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
                if let Some(dt) = zip_time::dos_for_write(modified) {
                    opts = opts.last_modified_time(dt);
                }
                let opts = with_ext_timestamp(opts.into_full_options(), modified)?;
                // `add_directory` stores the name with a trailing `/`; claim it in that form.
                let stored = if name.ends_with('/') {
                    name.clone()
                } else {
                    format!("{name}/")
                };
                Self::claim_name(&mut self.names, &stored)?;
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
                match done.body {
                    DoneBody::Prepared { name, file } => {
                        Self::claim_name(&mut self.names, &name)?;
                        let t_copy = Instant::now();
                        zw.add_prepared_file(file).map_err(map_zip_write)?;
                        self.prof_copy += t_copy.elapsed().as_nanos();
                    }
                    DoneBody::Archive(zip_bytes) => {
                        let t_parse = Instant::now();
                        let inner =
                            zip::ZipArchive::new(Cursor::new(zip_bytes)).map_err(map_zip_write)?;
                        // A worker builds exactly one entry; anything else is a bug, and merging it
                        // would splice in whatever it holds.
                        let name = match (inner.len(), inner.name_for_index(0)) {
                            // zip 9 decodes the name on demand; a name our own worker wrote and the
                            // crate cannot decode is a bug, reported like the entry-count one below.
                            (1, Some(name)) => name.map_err(map_zip_write)?.into_owned(),
                            (n, _) => {
                                return Err(ArchiveError::Backend(format!(
                                    "zip: a compression worker produced {n} entries, expected 1"
                                )))
                            }
                        };
                        self.prof_parse += t_parse.elapsed().as_nanos();
                        Self::claim_name(&mut self.names, &name)?;
                        let t_copy = Instant::now();
                        zw.merge_archive(inner).map_err(map_zip_write)?;
                        self.prof_copy += t_copy.elapsed().as_nanos();
                    }
                }
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
    /// sequential writer, unchanged: it still serves `conv`, `CRAM_ZIP_SEQUENTIAL` and any entry too
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
            self.aes_pw.as_deref(),
            large,
            entry.modified,
        )?;
        let name = zip_name(entry);
        Self::claim_name(&mut self.names, &name)?;
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

/// What a worker needs that does not vary per entry: the codec settings and the password. Constant
/// for the whole archive, so it rides along as one clone per job rather than four more arguments.
#[derive(Clone)]
struct EntrySettings {
    method: CompressionMethod,
    level: Option<i64>,
    adaptive: bool,
    /// Cloning this is an `Arc` bump, not a copy of the plaintext.
    pw: Option<Arc<Secret>>,
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

/// Compress one file off the writer thread, into whichever [`DoneBody`] its encryption allows.
///
/// The store-vs-compress decision is made here from the bytes already in hand. It must match
/// `probe::classify_file` exactly, the same way the engine's inline probe does, or archives written
/// through this path would differ from ones written through `add_file`.
fn compress_one(
    name: String,
    path: PathBuf,
    modified: Option<SystemTime>,
    hint: WriteHint,
    set: EntrySettings,
) -> Result<DoneEntry> {
    let EntrySettings {
        method,
        level,
        adaptive,
        pw,
    } = set;
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
    // caller streams anything bigger. (`CRAM_ZIP_ENTRY_MAX` can raise that limit; it is a test
    // knob, and nothing bounds it.)
    let t_zip = Instant::now();
    let opts = file_options(entry_method, level, pw.as_deref(), false, modified)?;
    let body = if pw.is_some() {
        // Encrypted: a whole one-entry archive, because the builder below refuses encryption.
        let mut inner = ZipCrateWriter::new(Cursor::new(Vec::with_capacity(data.len() / 2 + 512)));
        inner.start_file(name, opts).map_err(map_zip_write)?;
        write_body(&mut inner, &data, entry_method, level, ZOPFLI_BLOCK)?;
        DoneBody::Archive(inner.finish().map_err(map_zip_write)?.into_inner())
    } else {
        let mut builder = ZipFileBuilder::new(&name, opts).map_err(map_zip_write)?;
        write_body(&mut builder, &data, entry_method, level, ZOPFLI_BLOCK)?;
        let file = builder.finish().map_err(map_zip_write)?;
        DoneBody::Prepared { name, file }
    };
    wprof::ZIP_NANOS.fetch_add(t_zip.elapsed().as_nanos() as u64, Relaxed);
    Ok(DoneEntry {
        body,
        in_bytes: data.len() as u64,
        stored: adaptive_store,
    })
}

/// Hand an entry's bytes to the zip encoder in the piece size that encoder actually wants.
///
/// For everything but zopfli this is one `write_all` and the piece size does not matter: flate2
/// buffers internally and picks its own block boundaries.
///
/// **Zopfli is different, and it cost 663,064 bytes on a single silesia entry.** Its
/// `DeflateEncoder` treats *each `write` call as one master block* — `write(buf)` takes the whole
/// slice as the chunk and compresses it as a unit — and `Options::maximum_block_splits` caps a
/// master block at 15 smart splits. So a 51 MB entry handed over in one `write_all` buys **16
/// Huffman trees for 51 MB**, while the reference zopfli CLI feeds 1,000,000 bytes at a time and
/// gets 15 splits per megabyte. On heterogeneous data that is the whole difference; on homogeneous
/// text it barely registers, which is the exact shape silesia showed — `mozilla` 3.64% behind
/// 7-Zip while prose and XML were already ahead of it.
///
/// **The zopfli buffer `file_options` asks for does not do this**, which is why setting it
/// measured as a no-op: `BufWriter` passes any write at least as large as its capacity straight
/// through to the inner writer, so the buffer is bypassed by precisely the writes that needed it.
/// Both settings under test were being bypassed, so neither was ever actually under test.
///
/// The streaming path for entries over [`PARALLEL_ENTRY_MAX`] never had this problem: `io::copy`
/// feeds it in 8 KiB pieces, so the buffer fills and flushes. It flushes every 999,424 bytes (122
/// pieces), not every 1,000,000, so the two paths cut a file over 1,000,000 bytes into different
/// master blocks, and at `--tiny` its compressed bytes differ: by 4 bytes on a 1,000,001-byte
/// file, not at all at 1,000,000. Both are valid DEFLATE of the same data, and 1.3.2 did the same.
/// `block` is the master-block size; production passes [`ZOPFLI_BLOCK`]. It is a parameter so the
/// regression test can demonstrate the effect on a buffer small enough to compress in a second.
fn write_body<W: io::Write>(
    zw: &mut W,
    data: &[u8],
    method: CompressionMethod,
    level: Option<i64>,
    block: usize,
) -> Result<()> {
    let zopfli =
        method != CompressionMethod::Stored && matches!(level, Some(n) if n >= ZOPFLI_MIN_LEVEL);
    if zopfli {
        for piece in data.chunks(block) {
            io::Write::write_all(zw, piece)?;
        }
    } else {
        io::Write::write_all(zw, data)?;
    }
    Ok(())
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
        let modified = entry.modified;
        let set = EntrySettings {
            method: self.method,
            level: self.level,
            adaptive: self.adaptive,
            pw: self.aes_pw.clone(),
        };
        // FIFO, not `rayon::spawn`. A plain spawn goes onto a worker's LIFO local queue, so the most
        // recently submitted entry runs first -- while the writer thread is blocked on the OLDEST
        // one, which is then the last to be picked up. The queue stays full, the workers stay busy,
        // and the pipeline still crawls, because the head of the line is always the least urgent
        // thing in it.
        rayon::spawn_fifo(move || {
            // The receiver is only dropped when the whole create is being torn down, so a failed
            // send means nobody is listening any more and there is nothing useful to do with it.
            let _ = tx.send(compress_one(name, owned, modified, hint, set));
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
                "append entry    {:9.1} ms   {:6.1} us/entry",
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
                "compress entry  {:9.1} ms   {:6.1} us/file   ({} files)",
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
        use super::zip_time::dos_for_write;
        assert!(dos_for_write(None).is_none());
        // Pre-1980 (DOS epoch) has no representation -> dropped, not stored.
        assert!(dos_for_write(Some(UNIX_EPOCH)).is_none());
        // A far-future time (~year 5000) must be rejected BEFORE any calendar conversion, which
        // panics beyond +/-9999 - the bound makes this return None, never crash.
        let huge = UNIX_EPOCH + Duration::from_secs(95_617_584_000);
        assert!(dos_for_write(Some(huge)).is_none());
        // A real 2020 timestamp converts.
        let real = UNIX_EPOCH + Duration::from_secs(1_577_934_246);
        assert!(dos_for_write(Some(real)).is_some());
    }

    /// One entry's headers as they sit in the file: the name, the method and the DOS (time, date)
    /// of the local and the central header, each header's extra-field bytes, and where the
    /// entry's data starts.
    struct RawHeaders {
        name: String,
        local_method: u16,
        central_method: u16,
        local_dos: (u16, u16),
        central_dos: (u16, u16),
        local_extra: Vec<u8>,
        central_extra: Vec<u8>,
        data_start: usize,
    }

    fn le16(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }

    fn le32(b: &[u8], at: usize) -> usize {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
    }

    /// Walk the central directory of an archive this writer produced (no ZIP64, no comment).
    fn raw_headers(zip: &[u8]) -> Vec<RawHeaders> {
        let eocd = zip
            .windows(4)
            .rposition(|w| w == b"PK\x05\x06")
            .expect("end of central directory");
        let count = le16(zip, eocd + 10);
        let mut p = le32(zip, eocd + 16);
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(&zip[p..p + 4], b"PK\x01\x02", "central header at {p}");
            let (n, m, k) = (
                usize::from(le16(zip, p + 28)),
                usize::from(le16(zip, p + 30)),
                usize::from(le16(zip, p + 32)),
            );
            let q = le32(zip, p + 42);
            assert_eq!(&zip[q..q + 4], b"PK\x03\x04", "local header at {q}");
            let (ln, lm) = (
                usize::from(le16(zip, q + 26)),
                usize::from(le16(zip, q + 28)),
            );
            out.push(RawHeaders {
                name: String::from_utf8(zip[p + 46..p + 46 + n].to_vec()).unwrap(),
                local_method: le16(zip, q + 8),
                central_method: le16(zip, p + 10),
                local_dos: (le16(zip, q + 10), le16(zip, q + 12)),
                central_dos: (le16(zip, p + 12), le16(zip, p + 14)),
                local_extra: zip[q + 30 + ln..q + 30 + ln + lm].to_vec(),
                central_extra: zip[p + 46 + n..p + 46 + n + m].to_vec(),
                data_start: q + 30 + ln + lm,
            });
            p += 46 + n + m + k;
        }
        out
    }

    /// The payload of extra field `id`, if present.
    fn extra_field(extra: &[u8], id: u16) -> Option<Vec<u8>> {
        let mut i = 0;
        while i + 4 <= extra.len() {
            let len = usize::from(le16(extra, i + 2));
            if le16(extra, i) == id {
                return Some(extra[i + 4..i + 4 + len].to_vec());
            }
            i += 4 + len;
        }
        None
    }

    /// The DOS (time, date) for `unix` in this machine's zone. The offset comes from
    /// `localtime::utc_offset_at`, the same function the writer uses, so this cannot catch a wrong
    /// offset; the `localtime` tests check that function against values fixed in the test. The
    /// conversion from offset to DOS fields is done here, apart from `zip_time`.
    fn expected_dos(unix: u64) -> (u16, u16) {
        let utc = i64::try_from(unix).unwrap();
        let off = i64::from(crate::localtime::utc_offset_at(utc).unwrap_or(0));
        let t = time::OffsetDateTime::from_unix_timestamp(utc + off).unwrap();
        let time =
            (u16::from(t.hour()) << 11) | (u16::from(t.minute()) << 5) | u16::from(t.second() / 2);
        let date = ((u16::try_from(t.year()).unwrap() - 1980) << 9)
            | (u16::from(u8::from(t.month())) << 5)
            | u16::from(t.day());
        (time, date)
    }

    /// The fix, end to end, on both writer paths: every entry carries `0x5455` with the UTC
    /// seconds in its local and its central header, the DOS fields hold the local wall time, and
    /// reading the archive back gives the exact time that went in.
    ///
    /// The parallel path is the one at risk, and it has lost the field twice. It used to build each
    /// entry as a one-entry archive and copy it into the main one: `raw_copy_file` rewrote the
    /// local header and dropped the extra field, and under `zip` 9 `merge_archive` drops it from
    /// the central header (this test caught that one). An unencrypted entry now arrives as a
    /// `PreparedZipFile`, whose headers are written from the options the worker chose. Encrypted
    /// entries are the exception: `encrypted_entries_keep_the_timestamp_in_the_local_header_only`.
    #[test]
    fn mtime_survives_both_writer_paths() {
        use crate::model::{EntryKind, EntryPath};
        use crate::reader::RandomAccessReader;

        const WINTER: u64 = 1_577_934_246; // 2020-01-02T03:04:06Z
        const SUMMER: u64 = 1_593_604_800; // 2020-07-01T12:00:00Z
        let dir = std::env::temp_dir().join(format!("cram-zipmtime-w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = [("w.txt", WINTER), ("s.txt", SUMMER)];
        for (name, _) in files {
            std::fs::write(dir.join(name), format!("{name} body\n").repeat(40)).unwrap();
        }
        let entry = |i: usize, name: &str, kind: EntryKind, secs: u64, size: u64| Entry {
            index: i,
            path: EntryPath::from_raw(name).unwrap(),
            kind,
            size,
            compressed_size: None,
            modified: Some(UNIX_EPOCH + Duration::from_secs(secs)),
            unix_mode: None,
            crc32: None,
            encrypted: false,
        };

        let mut archives = Vec::new();
        for parallel in [true, false] {
            let path = dir.join(if parallel { "par.zip" } else { "seq.zip" });
            let mut w =
                Box::new(ZipArchiveWriter::create(&path, &CreateOptions::default()).unwrap());
            w.parallel = parallel;
            w.add_dir(&entry(0, "d", EntryKind::Dir, SUMMER, 0))
                .unwrap();
            for (i, (name, secs)) in files.iter().enumerate() {
                let src = dir.join(name);
                let size = std::fs::metadata(&src).unwrap().len();
                let e = entry(i + 1, name, EntryKind::File, *secs, size);
                if parallel {
                    w.add_path(&e, &src, WriteHint::default()).unwrap();
                } else {
                    let mut body = std::fs::File::open(&src).unwrap();
                    w.add_file(&e, &mut body, WriteHint::default()).unwrap();
                }
            }
            w.finish().unwrap();
            let bytes = std::fs::read(&path).unwrap();

            let want = [("d/", SUMMER), ("w.txt", WINTER), ("s.txt", SUMMER)];
            let headers = raw_headers(&bytes);
            assert_eq!(headers.len(), want.len());
            for (h, (name, secs)) in headers.iter().zip(want) {
                let tag = format!("parallel={parallel} {name}");
                assert_eq!(h.name, name, "{tag}: order");
                let mut ut = vec![0x01];
                ut.extend_from_slice(&(secs as u32).to_le_bytes());
                assert_eq!(
                    extra_field(&h.local_extra, 0x5455),
                    Some(ut.clone()),
                    "{tag}: 0x5455 missing or wrong in the local header"
                );
                assert_eq!(
                    extra_field(&h.central_extra, 0x5455),
                    Some(ut),
                    "{tag}: 0x5455 missing or wrong in the central header"
                );
                assert_eq!(h.local_dos, expected_dos(secs), "{tag}: local DOS fields");
                assert_eq!(
                    h.central_dos,
                    expected_dos(secs),
                    "{tag}: central DOS fields"
                );
            }

            let r = super::super::zip::ZipReader::open(&path, Arc::new(crate::secret::NoPassword))
                .unwrap();
            for (e, (name, secs)) in RandomAccessReader::entries(&r).iter().zip(want) {
                assert_eq!(
                    e.modified,
                    Some(UNIX_EPOCH + Duration::from_secs(secs)),
                    "parallel={parallel} {name}: time read back"
                );
            }
            archives.push(bytes);
        }
        assert_eq!(
            archives[0], archives[1],
            "the parallel and the streamed writer must produce the same bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `merge_archive` does not refuse a name already in the archive the way `raw_copy_file` did,
    /// so the writer has to. Without the check the second entry silently replaces the first in the
    /// central directory.
    ///
    /// Both kinds of archive, because they reach the main archive differently: an unencrypted entry
    /// goes through `add_prepared_file`, which checks for itself, and an encrypted one through
    /// `merge_archive`, where the writer's own check is the only one.
    #[test]
    fn duplicate_names_are_still_refused_on_the_parallel_path() {
        use crate::model::{EntryKind, EntryPath};

        let dir = std::env::temp_dir().join(format!("cram-zipdup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("same.txt");
        std::fs::write(&src, b"one name, twice").unwrap();
        let e = Entry {
            index: 0,
            path: EntryPath::from_raw("same.txt").unwrap(),
            kind: EntryKind::File,
            size: 15,
            compressed_size: None,
            modified: None,
            unix_mode: None,
            crc32: None,
            encrypted: false,
        };
        for encrypted in [false, true] {
            let opts = if encrypted {
                aes_options()
            } else {
                CreateOptions::default()
            };
            let mut w = Box::new(ZipArchiveWriter::create(&dir.join("dup.zip"), &opts).unwrap());
            assert!(w.parallel, "this test is about the parallel path");
            w.add_path(&e, &src, WriteHint::default()).unwrap();
            w.add_path(&e, &src, WriteHint::default()).unwrap();
            let err = match w.finish() {
                Ok(_) => panic!("encrypted={encrypted}: a duplicate name was written"),
                Err(e) => e.to_string(),
            };
            assert!(
                err.contains("Duplicate filename"),
                "encrypted={encrypted}: unexpected error: {err}"
            );
            // The crate's own refusal, which an unencrypted entry would fall back on, prints the
            // name as `EscapeAscii { .. }`. The writer's check comes first and names the file.
            assert!(
                err.contains("same.txt"),
                "encrypted={encrypted}: the error does not name the file: {err}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AES-256 create options with a fixed password, for the tests that need an encrypted archive.
    fn aes_options() -> CreateOptions {
        use crate::secret::{EncryptSpec, ZipCipher};
        CreateOptions {
            encrypt: Some(EncryptSpec {
                password: Secret::new(AES_TEST_PASSWORD),
                zip_cipher: ZipCipher::Aes256,
                header: HeaderMode::default(),
            }),
            ..Default::default()
        }
    }

    const AES_TEST_PASSWORD: &str = "correct horse battery staple";

    /// The one place the fix does not reach, pinned so that it cannot widen or quietly go away.
    ///
    /// An encrypted entry written through the parallel path is spliced in with `merge_archive`,
    /// and `zip` 9 writes a merged entry's central header without the `0x5455` field (see
    /// [`DoneBody::Archive`]). What such an entry does carry: the field in its local header, and
    /// the local wall time in the DOS fields of both headers. What it loses: this reader, which
    /// reads central headers, dates it by the DOS fields. The four files are the cases that hold
    /// in any zone: two whole even seconds away from a zone change come back exact, an odd second
    /// comes back one second early, and a 1975 date, which the DOS fields cannot hold, comes back
    /// as the time of writing. A time in the hour that repeats when summer time ends comes back an
    /// hour late; that needs a zone with summer time and is not asserted here.
    ///
    /// The streamed path is checked beside it: with the same password it writes the field to both
    /// headers and gives all four times back exact, which is what the parallel path would do if
    /// the crate let it.
    ///
    /// **If the assertion on the central header fails, the crate has started writing the field
    /// back.** That is good news: drop the exception from `DoneBody::Archive`, the changelog and
    /// this test, and assert the field in both headers here as `mtime_survives_both_writer_paths`
    /// does.
    #[test]
    fn encrypted_entries_keep_the_timestamp_in_the_local_header_only() {
        use crate::model::{EntryKind, EntryPath};
        use crate::reader::RandomAccessReader;

        /// What the DOS fields give back for a time, which is all the parallel path has.
        #[derive(Clone, Copy, PartialEq)]
        enum Dos {
            /// A whole even second the fields can hold.
            Exact,
            /// An odd second: the fields count in twos and the writer rounds down.
            SecondEarly,
            /// Before 1980: the writer sets no DOS time and the crate stamps the time of writing.
            TimeOfWriting,
        }

        const WINTER: u64 = 1_577_934_246; // 2020-01-02T03:04:06Z
        const SUMMER: u64 = 1_593_604_800; // 2020-07-01T12:00:00Z
        const ODD: u64 = 1_748_779_201; // 2025-06-01T12:00:01Z
        const EARLY: u64 = 157_766_400; // 1975-01-01T00:00:00Z
        const AFTER_2020: u64 = 1_600_000_000;
        let dir = std::env::temp_dir().join(format!("cram-zipmtime-aes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = [
            ("w.txt", WINTER, Dos::Exact),
            ("s.txt", SUMMER, Dos::Exact),
            ("o.txt", ODD, Dos::SecondEarly),
            ("e.txt", EARLY, Dos::TimeOfWriting),
        ];
        for (name, _, _) in files {
            std::fs::write(dir.join(name), format!("{name} body\n").repeat(40)).unwrap();
        }

        for parallel in [true, false] {
            let path = dir.join(if parallel { "par.zip" } else { "seq.zip" });
            let mut w = Box::new(ZipArchiveWriter::create(&path, &aes_options()).unwrap());
            w.parallel = parallel;
            for (i, (name, secs, _)) in files.iter().enumerate() {
                let src = dir.join(name);
                let e = Entry {
                    index: i,
                    path: EntryPath::from_raw(name).unwrap(),
                    kind: EntryKind::File,
                    size: std::fs::metadata(&src).unwrap().len(),
                    compressed_size: None,
                    modified: Some(UNIX_EPOCH + Duration::from_secs(*secs)),
                    unix_mode: None,
                    crc32: None,
                    encrypted: false,
                };
                if parallel {
                    w.add_path(&e, &src, WriteHint::default()).unwrap();
                } else {
                    let mut body = std::fs::File::open(&src).unwrap();
                    w.add_file(&e, &mut body, WriteHint::default()).unwrap();
                }
            }
            w.finish().unwrap();
            let bytes = std::fs::read(&path).unwrap();

            let headers = raw_headers(&bytes);
            assert_eq!(headers.len(), files.len());
            for (h, (name, secs, dos)) in headers.iter().zip(files) {
                let tag = format!("parallel={parallel} {name}");
                assert_eq!(h.name, name, "{tag}: order");
                let mut ut = vec![0x01];
                ut.extend_from_slice(&(secs as u32).to_le_bytes());
                assert_eq!(
                    extra_field(&h.local_extra, 0x5455),
                    Some(ut.clone()),
                    "{tag}: 0x5455 missing or wrong in the local header"
                );
                assert_eq!(
                    extra_field(&h.central_extra, 0x5455),
                    if parallel { None } else { Some(ut) },
                    "{tag}: 0x5455 in the central header (see this test's comment if it is \
                     the parallel one that changed)"
                );
                // A time the DOS fields cannot hold has no expected fields to compare with.
                if dos != Dos::TimeOfWriting {
                    assert_eq!(h.local_dos, expected_dos(secs), "{tag}: local DOS fields");
                    assert_eq!(
                        h.central_dos,
                        expected_dos(secs),
                        "{tag}: central DOS fields"
                    );
                }
            }

            let r = super::super::zip::ZipReader::open(&path, Arc::new(crate::secret::NoPassword))
                .unwrap();
            for (e, (name, secs, dos)) in RandomAccessReader::entries(&r).iter().zip(files) {
                let tag = format!("parallel={parallel} {name}: time read back");
                assert!(e.encrypted, "parallel={parallel} {name}: not encrypted");
                let exact = Some(UNIX_EPOCH + Duration::from_secs(secs));
                match (parallel, dos) {
                    // The streamed path has the field in the central header: every time is exact.
                    (false, _) | (true, Dos::Exact) => assert_eq!(e.modified, exact, "{tag}"),
                    (true, Dos::SecondEarly) => assert_eq!(
                        e.modified,
                        Some(UNIX_EPOCH + Duration::from_secs(secs - 1)),
                        "{tag}"
                    ),
                    (true, Dos::TimeOfWriting) => assert!(
                        e.modified > Some(UNIX_EPOCH + Duration::from_secs(AFTER_2020)),
                        "{tag}: expected the time of writing, got {:?}",
                        e.modified
                    ),
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The test the parallel path was waiting on.
    ///
    /// Encrypted archives were forced onto the sequential writer because an AES entry carries its
    /// own framing — a 0x9901 extra field holding the vendor version, key strength and the *real*
    /// compression method, with method 99 in the header — and the parallel path does not compress
    /// into the archive directly. It builds a complete one-entry zip in memory and copies it into
    /// the main archive (`raw_copy_file` when this test was written, `merge_archive` now), so the
    /// question is whether that reproduces the framing or quietly drops it.
    ///
    /// It reproduces it. This test is what turns that from a reading of the crate into a fact, and
    /// it must stay: the failure it guards against is an archive that *looks* fine and cannot be
    /// decrypted.
    ///
    /// Several entries on purpose — each gets its own salt from its own `start_file`, and a bug that
    /// shared or reused one would still round-trip a single-entry archive.
    #[test]
    fn aes_survives_the_parallel_path() {
        use crate::model::{EntryKind, EntryPath};
        use crate::secret::{EncryptSpec, ZipCipher};

        let dir = std::env::temp_dir().join(format!("cram-aespar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();

        // Bodies of different sizes and compressibility, so both the DEFLATE and the adaptive-store
        // arms are exercised under encryption.
        let bodies: Vec<Vec<u8>> = vec![
            b"the same short line over and over, the same short line over and over".to_vec(),
            (0..200_000u32).map(|i| (i / 977) as u8).collect(),
            {
                let mut seed = 0xC0FF_EE00_1234_5678u64;
                (0..150_000)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed as u8
                    })
                    .collect()
            },
        ];
        for (i, b) in bodies.iter().enumerate() {
            std::fs::write(src.join(format!("f{i}.bin")), b).unwrap();
        }

        let archive = dir.join("enc.zip");
        let pw = "correct horse battery staple";
        let opts = CreateOptions {
            level: Level::Auto,
            encrypt: Some(EncryptSpec {
                password: Secret::new(pw),
                zip_cipher: ZipCipher::Aes256,
                header: HeaderMode::default(),
            }),
            ..Default::default()
        };
        let mut w = Box::new(ZipArchiveWriter::create(&archive, &opts).unwrap());
        assert!(
            w.parallel,
            "this test is meaningless unless the parallel path is the one being used"
        );
        assert!(w.takes_paths(), "the parallel path is entered via add_path");

        for (i, b) in bodies.iter().enumerate() {
            let entry = Entry {
                index: i,
                path: EntryPath::from_raw(&format!("f{i}.bin")).unwrap(),
                kind: EntryKind::File,
                size: b.len() as u64,
                compressed_size: None,
                modified: None,
                unix_mode: None,
                crc32: None,
                encrypted: false,
            };
            w.add_path(&entry, &src.join(format!("f{i}.bin")), WriteHint::default())
                .unwrap();
        }
        w.finish().unwrap();

        // The headers as they sit in the file, read without the zip crate. An AES entry states
        // method 99 in both headers and carries the 0x9901 field in both; the real method lives
        // in that field. A read-back through the crate alone proves none of this: under `zip` 8,
        // `merge_archive` wrote the inner method into the central header, the crate's own reader
        // took it, and 7-Zip refused the archive.
        let raw = std::fs::read(&archive).unwrap();
        let headers = raw_headers(&raw);
        assert_eq!(headers.len(), bodies.len(), "raw entry count");
        let mut salts = std::collections::HashSet::new();
        for h in &headers {
            assert_eq!(h.local_method, 99, "{}: local header method", h.name);
            assert_eq!(h.central_method, 99, "{}: central header method", h.name);
            assert!(
                extra_field(&h.local_extra, 0x9901).is_some(),
                "{}: no AES field in the local header",
                h.name
            );
            assert!(
                extra_field(&h.central_extra, 0x9901).is_some(),
                "{}: no AES field in the central header",
                h.name
            );
            // AES-256: the entry's data starts with its 16-byte salt.
            salts.insert(raw[h.data_start..h.data_start + 16].to_vec());
        }
        assert_eq!(salts.len(), bodies.len(), "every entry needs its own salt");

        // Read it back the way any reader would, with the password.
        let f = std::fs::File::open(&archive).unwrap();
        let mut za = zip::ZipArchive::new(f).unwrap();
        assert_eq!(za.len(), bodies.len(), "entry count");
        for (i, want) in bodies.iter().enumerate() {
            let mut e = za.by_index_decrypt(i, pw.as_bytes()).unwrap();
            assert!(e.encrypted(), "entry {i} came out unencrypted");
            let mut got = Vec::new();
            std::io::Read::read_to_end(&mut e, &mut got).unwrap();
            assert_eq!(&got, want, "entry {i} did not round-trip");
        }

        // And the wrong password must fail rather than yielding garbage.
        let f2 = std::fs::File::open(&archive).unwrap();
        let mut za2 = zip::ZipArchive::new(f2).unwrap();
        assert!(
            za2.by_index_decrypt(0, b"wrong password").is_err(),
            "a wrong password must be rejected, not silently decoded"
        );

        let _ = std::fs::remove_dir_all(&dir);
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

    /// The master-block regression, pinned.
    ///
    /// Zopfli compresses one master block per `write` call and splits each into at most 15 blocks,
    /// so handing it a whole large entry costs Huffman trees the reference CLI would have spent.
    /// It is invisible on homogeneous input and expensive on mixed input, which is why silesia's
    /// prose entries looked fine while `mozilla` was 3.64% behind 7-Zip.
    ///
    /// **What this asserts is the piece size, not a ratio**, and that is deliberate. The ratio win
    /// only appears once 15 splits is genuinely too few for the data, which takes megabytes and a
    /// minute of CPU; it is measured on silesia and recorded in `FINDINGS-b1b-tiny-vs-zopfli.md`
    /// rather than asserted here. Two earlier versions of this test tried to assert bytes and both
    /// were worthless: at 256 KiB the splitter has budget to spare, so forcing boundaries every
    /// 32 KiB measured 190 bytes *worse* than one block. Chunking is not good in itself — it is
    /// good exactly when the cap binds, and a unit test cannot afford to reach that point.
    ///
    /// So: one write per master block when zopfli is the encoder, one write otherwise.
    #[test]
    fn zopfli_gets_the_entry_in_master_blocks() {
        /// Records the size of every `write` it is handed.
        struct Sizes(Vec<usize>);
        impl io::Write for Sizes {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.push(buf.len());
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let body = vec![0u8; 2 * ZOPFLI_BLOCK + 12_345];
        let sizes = |method, level| {
            let mut w = Sizes(Vec::new());
            write_body(&mut w, &body, method, level, ZOPFLI_BLOCK).unwrap();
            w.0
        };

        assert_eq!(
            sizes(CompressionMethod::Deflated, Some(ZOPFLI_LEVEL)),
            vec![ZOPFLI_BLOCK, ZOPFLI_BLOCK, 12_345],
            "zopfli must see one master block per write, or a large entry becomes one block with \
             15 splits and the Huffman trees it needed are never built"
        );

        // Every other encoder buffers internally and picks its own boundaries; splitting the write
        // would buy nothing and is not done.
        assert_eq!(
            sizes(CompressionMethod::Deflated, Some(9)),
            vec![body.len()],
            "flate2 levels must not be chunked"
        );
        assert_eq!(
            sizes(CompressionMethod::Stored, Some(ZOPFLI_LEVEL)),
            vec![body.len()],
            "a stored entry has no encoder to feed in blocks"
        );
    }
}
