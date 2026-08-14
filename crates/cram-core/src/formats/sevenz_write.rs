//! 7z writer backend, the create counterpart to [`super::sevenz`], via `sevenz-rust2`'s encoder
//! (the `compress` feature). Uses `push_archive_entry` (one independently-decodable pack per entry,
//! *non-solid*): this both fits the incremental [`ArchiveWriter`] contract (stream one entry at a
//! time) and matches Cram's strategy of authoring parallel-extractable layouts.
//!
//! **Adaptive per-entry store:** because each entry is its own pack, the content-method chain can
//! change between entries. `push_archive_entry` records whatever `set_content_methods` holds at the
//! time of the call into that entry's folder, so an incompressible entry (per the probe's
//! [`WriteHint`]) is written with a COPY chain while the rest use LZMA2, heterogeneous folders in
//! one 7z are standard and both 7-Zip and our own reader handle them.
//!
//! Encryption is 7z's real strength and both create forks are honored:
//!   - **AES-256** content encryption (`AesEncoderOptions`), 7-Zip's own scheme: ONE random salt
//!     per archive (the KDF then runs once and is cached), a fresh random IV per entry, and the
//!     7-Zip-standard KDF work factor (`num_cycles_power = 19`, ≈524k SHA-256 rounds; the library
//!     default of 8 is ~256 rounds, far too weak against offline guessing).
//!   - **Header (name) encryption**, [`HeaderMode::NamesToo`] maps to `set_encrypt_header(true)`,
//!     so the file listing needs the password too; [`HeaderMode::ContentsOnly`] leaves names visible.
//!     `finish` installs a FRESH AES configuration before finalizing: the library encrypts the
//!     header with the last configuration it saw, which would otherwise reuse the last entry's IV;
//!     and an archive with no file entries would have no AES configuration at all, silently writing
//!     a NamesToo header in plaintext.
//!
//! The content-method chain is written AES-first, compressor-second (`vec![aes, lzma2]`): the last
//! method is applied to the data first, so bytes are compressed *then* encrypted.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;
use std::time::Instant;

use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
use sevenz_rust2::{
    prepare_pack, ArchiveEntry, ArchiveWriter as SzWriter, EncoderConfiguration, EncoderMethod,
    Error as SzError, Password, PreparedPack, SourceReader,
};

use crate::error::{ArchiveError, Result};
use crate::format::Codec;
use crate::model::Entry;
use crate::probe;
use crate::secret::{HeaderMode, Secret};
use crate::writer::{ArchiveWriter, CreateOptions, CreateReport, Level, WriteHint};

/// Raise this process's file-descriptor soft limit toward its hard limit, once.
///
/// A solid block holds one handle per entry, so the descriptor limit sets the block size, the block
/// size sets how many LZMA2 chunks a pack holds, and the chunk count sets how many threads can work
/// on it. On Linux that chain turns a default soft limit of 1024 into ~30 MB blocks, three or four
/// 8 MiB chunks, and three busy cores on a 24-core machine — measured at 105.4s and 2.54 cores.
///
/// The hard limit is typically 1,048,576, and the soft limit is a courtesy default rather than a
/// real constraint. Raising it is what `ripgrep`, `fd` and most file-heavy tools do.
///
/// Deliberately conservative, because this is a library and the rlimit is process-wide: it never
/// lowers anything, it does nothing when the soft limit is already generous, and it asks for at most
/// 65,536 rather than the hard limit — macOS in particular will refuse a request above
/// `kern.maxfilesperproc`. Failure is ignored; the adaptive flush in `open_for_block` still keeps
/// the writer correct at whatever limit is actually in force.
#[cfg(unix)]
fn raise_descriptor_limit() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    const WANT: libc::rlim_t = 65_536;
    ONCE.call_once(|| unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 || lim.rlim_cur >= WANT {
            return;
        }
        lim.rlim_cur = WANT.min(lim.rlim_max);
        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
    });
}

#[cfg(not(unix))]
fn raise_descriptor_limit() {}

/// Whether an open failed because the process (or the system) is out of file descriptors.
///
/// Matched on the raw code because `std::io::ErrorKind` has no stable variant for either: `EMFILE`
/// and `ENFILE` are 24 and 23 on Unix, and Windows reports `ERROR_TOO_MANY_OPEN_FILES` as 4.
fn out_of_descriptors(e: &io::Error) -> bool {
    match e.raw_os_error() {
        #[cfg(unix)]
        Some(24) | Some(23) => true,
        #[cfg(windows)]
        Some(4) => true,
        _ => false,
    }
}

fn map_sz(e: SzError) -> ArchiveError {
    match e {
        SzError::Io(io, _) | SzError::FileOpen(io, _) => ArchiveError::Io(io),
        SzError::PasswordRequired => ArchiveError::PasswordRequired,
        SzError::MaybeBadPassword(_) => ArchiveError::WrongPassword,
        other => ArchiveError::Backend(format!("7z write: {other}")),
    }
}

/// 7-Zip's standard AES-256 key-derivation work factor: 2^19 ≈ 524k SHA-256 rounds. The library
/// default is 8 (~256 rounds), which makes offline password guessing ~2000× cheaper. The decoder
/// side (ours and 7-Zip's) accepts up to 24.
const AES_CYCLES_POWER: u8 = 19;

/// Target size of one solid block, in source bytes.
///
/// Non-solid (one pack per entry) cost 23% of archive size against 7-Zip on a 41,305-file tree,
/// because every small file got its own LZMA2 dictionary and nothing was ever shared between them.
/// It also pinned create to one core: a pack is compressed inline into the output stream, so packs
/// cannot overlap.
///
/// 128 MiB, measured on a 1.19 GiB / 41,305-file tree at 8 threads with 8 MiB chunks:
///
/// | block | wall | peak RSS |
/// |---|---|---|
/// | 64 MiB | 104.9s | 786 MB |
/// | **128 MiB** | **96.9s** | 854 MB |
/// | 256 MiB | 96.5s | 968 MB |
///
/// 256 MiB buys nothing for the memory, and it costs on the other side of the trade. Two things get
/// worse as blocks grow: pulling a *single* file out means decompressing its block up to that file,
/// which is what `cram mount` and single-entry extraction pay; and full-archive extraction runs in
/// parallel ACROSS blocks, so too few blocks caps it. This tree is ~10 blocks at 128 MiB and ~5 at
/// 256 MiB — below the core count, where a bigger block would start buying create speed with
/// extract speed.
///
/// For context, 7-Zip's own default is far more solid than this, so 128 MiB remains the
/// random-access-friendly end of the choice rather than the aggressive one.
const SOLID_BLOCK_BYTES: u64 = 128 << 20;

/// Set to `0` to go back to one independently-decodable pack per entry: smaller random-access cost,
/// much larger archive, single-threaded create.
///
/// **Superseded by `CreateOptions::solid`, which `cram a --no-solid` sets.** Kept because it was
/// documented and someone may have it in a script, and because it is still the only way to reach
/// this from a caller that does not build its own `CreateOptions`. When set it wins, so an
/// environment that forces it keeps working; when unset the option decides, which is what a library
/// caller expects. A user-facing choice that changes archive layout should not have lived only in
/// an environment variable, and did for too long.
const ENV_SOLID: &str = "CRAM_7Z_SOLID";

/// Overrides [`SOLID_BLOCK_BYTES`], in bytes.
const ENV_BLOCK: &str = "CRAM_7Z_BLOCK";

/// Overrides the LZMA2 thread count, in place of [`lzma_threads`].
const ENV_THREADS: &str = "CRAM_7Z_THREADS";

/// Overrides the LZMA2 MT chunk size, in bytes. The library clamps it up to the dictionary.
const ENV_CHUNK: &str = "CRAM_7Z_CHUNK";

/// One entry's content, waiting for its block to be flushed: the probe's sample chained in front of
/// the still-open handle it was read from.
///
/// Three shapes were measured on a 41,305-file tree, and the differences between them are all about
/// how many times each byte gets read.
///
/// - **Buffered bytes** (read the whole entry up front): 127.8s. Correct, but it splits the work
///   into phases -- the disk runs with the compressor idle, then the reverse.
/// - **Handle, seeked back to zero after sampling**: 136.6s, the worst of the three. The probe
///   sample is larger than most files in a source tree, so rewinding re-reads nearly everything.
/// - **Sample chained in front of the handle** (this): one read per byte, and
///   `push_archive_entries` pulls from it while compressing, so the two overlap.
///
/// A bare path is not an option: `takes_paths` stops the engine probing inline, and a block must
/// know every entry's verdict before it can choose its single method chain.
///
/// `Bytes` exists only for `conv`, which supplies a reader with no file behind it.
enum BlockSource {
    Stream(io::Chain<Cursor<Vec<u8>>, File>),
    Bytes(Cursor<Vec<u8>>),
}

impl Read for BlockSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            BlockSource::Stream(s) => s.read(buf),
            BlockSource::Bytes(c) => c.read(buf),
        }
    }
}

/// Cap on entries per block, independent of [`SOLID_BLOCK_BYTES`]. A block holds one open handle per
/// entry, so a tree of 1 KB files would otherwise reach ~65,000 handles before the byte ceiling
/// noticed.
///
/// This is a backstop, not the real bound. [`SOLID_BLOCK_BYTES`] and the descriptor limit learned in
/// [`SevenZArchiveWriter::open_for_block`] are what normally decide a block.
///
/// It must be set high. A previous attempt at fixing descriptor exhaustion pinned it to 512, which
/// looked safe and quietly cost all the multi-threading: at ~30 KB mean file size a 512-entry block
/// is 15.5 MB, and LZMA2 MT needs a pack of at least two 8 MiB chunks — 16.8 MB — to be worth
/// starting. Blocks landed just under the threshold, MT switched off, and a 24-core machine finished
/// in the same 113s as an 8-core one. Worse, the margin was thin enough that a corpus with slightly
/// larger files would flip MT back on, so the same tool scaled on one tree and not on a similar one.
///
/// Letting the environment set the block size instead: Windows reaches the byte ceiling first
/// (~4,400 entries at 128 MiB), Linux learns its ~1,020 (≈30 MB blocks, comfortably over the MT
/// threshold), macOS learns ~250 — and at that point the blocks genuinely are too small to chunk, so
/// MT switching off is correct rather than accidental.
const SOLID_BLOCK_ENTRIES: usize = 8192;

/// LZMA2 dictionary size for a preset, in MiB (the xz preset table). Needed because encoder memory
/// is a multiple of the dictionary, and the dictionary grows 8× between the default level and
/// `--small`.
fn dict_mib(level: u32) -> u64 {
    match level {
        0 => 1,
        1 => 1,
        2 => 2,
        3 => 4,
        4 => 4,
        5 | 6 => 8,
        7 => 16,
        8 => 32,
        _ => 64,
    }
}

/// How many threads LZMA2 may use, bounded by memory as well as by cores.
///
/// Each encoder thread holds its own match-finder state, roughly 11× the dictionary. That is ~88 MB
/// per thread at the default level and ~700 MB at `--small`, so a fixed thread count that is fine
/// for one level will exhaust a small machine at another. Measured peak RSS with 16 threads at
/// level 6 was 997 MB, which is why this exists.
///
/// Physical cores, not logical: LZMA2's match finder is memory-bound enough that SMT siblings buy
/// very little, and halving the thread count halves the memory.
///
/// **Budgeted against TOTAL ram, not available ram.** Available memory is whatever else happens to
/// be running, so keying off it made the same machine produce different archives at different times
/// — and an archiver whose output and timings depend on whether a browser is open cannot be
/// benchmarked, by its users or by us. Total RAM is a property of the machine. A quarter of it is
/// conservative enough that the difference rarely bites, and when it does, swapping is the honest
/// signal rather than a silently different result.
fn lzma_threads(level: u32) -> u32 {
    let hw = crate::hw::HwProfile::detect();
    let per_thread = dict_mib(level).saturating_mul(11).max(1);
    let budget_mib = (hw.ram_total / (1024 * 1024)) / 4;
    let by_memory = (budget_mib / per_thread).max(1);
    let by_cores = hw.physical.max(1) as u64;
    by_memory.min(by_cores).clamp(1, 64) as u32
}

/// Map the abstract [`Level`] onto LZMA2's 0–9 scale (`Auto`/`Balanced` → 6).
fn lzma_level(level: Level) -> u32 {
    match level {
        Level::Auto | Level::Balanced => 6,
        Level::Fastest => 1,
        Level::Best | Level::Cold => 9,
        Level::Explicit(n) => n.clamp(0, 9),
    }
}

/// The 7z archive name for an entry: normalized-safe relative path, forward slashes.
fn arc_name(entry: &Entry) -> String {
    entry.path.safe().to_string_lossy().replace('\\', "/")
}

pub struct SevenZArchiveWriter {
    /// `Option` so `finish` can move the writer out.
    sz: Option<SzWriter<File>>,
    /// LZMA2 level for compressible entries.
    level: u32,
    /// Explicit `--store` (`opts.codec == Some(Codec::None)`): every entry is COPY regardless of hint.
    store_forced: bool,
    /// AES-256 password, or `None` for an unencrypted archive.
    aes_pw: Option<Secret>,
    /// One random KDF salt for the whole archive (7-Zip's model): the derived key is then computed
    /// once and cached by the library, while each entry still gets its own fresh random IV. A fresh
    /// salt per entry would be equivalent cryptographically but re-runs the ~524k-round KDF for
    /// every entry. Unused when `aes_pw` is `None`.
    aes_salt: [u8; 16],
    entries: u64,
    in_bytes: u64,
    /// Entries the adaptive probe stored verbatim (incompressible), for the report.
    stored: u64,
    start: Instant,
    /// Whether to group entries into solid blocks. Off restores one pack per entry.
    solid: bool,
    /// Mirrors the engine's `adaptive` flag. With `takes_paths` on the engine stops probing
    /// store-vs-compress inline, so the decision has to be made here instead.
    adaptive: bool,
    /// Target block size in source bytes.
    solid_max: u64,
    /// Threads for LZMA2's own chunked multi-threading, which only does anything inside a block
    /// large enough to hold more than one chunk -- which is exactly what solid blocks create.
    threads: u32,
    /// LZMA2 MT chunk size in bytes. Independent of the thread count on purpose: see
    /// [`Self::content_methods`].
    chunk: u64,
    /// The block being filled: entries, their unread sources, and total source bytes.
    block: Vec<(ArchiveEntry, BlockSource)>,
    block_bytes: u64,
    /// Packs written, for the CRAM_PROFILE block report.
    packs_written: usize,
    /// Packs compressing on the pool, in submission order. Drained from the front, which is what
    /// keeps the archive's packs in the order the walk produced them.
    pending_packs: VecDeque<Receiver<Result<PreparedPack>>>,
    /// How many packs may compress at once — the whole parallelism budget, since each pack now
    /// encodes single-threaded and concurrency comes from having several in flight.
    ///
    /// Also the multiplier on descriptor pressure: a pack holds one handle per entry until its
    /// worker finishes, so in-flight packs × entries-per-pack is what the limit actually sees.
    inflight_max: usize,
    /// Whether the open block is a COPY block. A pack has ONE method chain, so a change of
    /// store-ness closes the block. On real trees that is rare (157 of 41,305 entries on the test
    /// corpus); a corpus that alternated every entry would degenerate to one pack per entry, which
    /// is merely today's behaviour.
    block_store: bool,
    /// Directory entries, written after every file block.
    ///
    /// They cannot ride inside a solid pack: `push_archive_entries` asserts one reader per entry and
    /// a 7z directory has no stream. Writing them inline instead would close the open block every
    /// time the walk stepped into a new directory -- 2,434 times on the test corpus, which is no
    /// solid compression at all. Deferring them reorders the header, which 7z does not care about,
    /// and it applies directory mtimes after their contents are written rather than before, which is
    /// the order that actually preserves them.
    dirs: Vec<ArchiveEntry>,
}

impl SevenZArchiveWriter {
    pub fn create(path: &Path, opts: &CreateOptions) -> Result<Self> {
        raise_descriptor_limit();
        let mut sz = SzWriter::create(path).map_err(map_sz)?;

        let mut aes_salt = [0u8; 16];
        let aes_pw = match &opts.encrypt {
            None => {
                sz.set_encrypt_header(false); // no password → can't (and needn't) encrypt the header
                None
            }
            Some(spec) => {
                sz.set_encrypt_header(spec.header == HeaderMode::NamesToo);
                // One random salt for the whole archive (see the field docs). `new` generates a
                // cryptographically random salt; keep it, discard the rest of the throwaway.
                aes_salt = AesEncoderOptions::new(Password::new(spec.password.expose())).salt;
                Some(spec.password.clone())
            }
        };

        Ok(Self {
            sz: Some(sz),
            level: lzma_level(opts.level),
            store_forced: matches!(opts.codec, Some(Codec::None)),
            aes_pw,
            aes_salt,
            entries: 0,
            in_bytes: 0,
            stored: 0,
            start: Instant::now(),
            solid: std::env::var(ENV_SOLID)
                .map(|v| v != "0")
                .unwrap_or(opts.solid),
            adaptive: opts.level == Level::Auto && opts.codec.is_none(),
            solid_max: std::env::var(ENV_BLOCK)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(SOLID_BLOCK_BYTES)
                .max(1),
            threads: std::env::var(ENV_THREADS)
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or_else(|| lzma_threads(lzma_level(opts.level)))
                .clamp(1, 64),
            chunk: std::env::var(ENV_CHUNK)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or_else(|| dict_mib(lzma_level(opts.level)) << 20)
                .max(1),
            block: Vec::new(),
            block_bytes: 0,
            packs_written: 0,
            pending_packs: VecDeque::new(),
            inflight_max: std::env::var("CRAM_7Z_INFLIGHT")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or_else(|| lzma_threads(lzma_level(opts.level)) as usize)
                .max(1),
            block_store: false,
            dirs: Vec::new(),
        })
    }

    /// Open a source file for the open block, flushing the block first if the process has run out
    /// of descriptors.
    ///
    /// A solid block holds one handle per entry until it is written, so its size is bounded by the
    /// descriptor limit as well as by bytes and count — and that limit is not knowable from a
    /// constant. Windows lets a process hold millions; Linux defaults to a soft limit of **1024**,
    /// macOS to 256. A cap tuned on Windows made `.7z` creation fail outright on Linux for any tree
    /// over ~1000 files, in 0.15s, with `Too many open files`.
    ///
    /// Rather than guess the limit, react to it: on `EMFILE`/`ENFILE`, flush the block — which drops
    /// every handle it was holding — and retry once. That self-tunes to whatever the real limit is,
    /// needs no `getrlimit` binding, and behaves the same whether the limit is 256 or a million. A
    /// second failure is a real error and is reported as one.
    fn open_for_block(&mut self, path: &Path) -> Result<File> {
        match File::open(path) {
            Ok(f) => return Ok(f),
            // Anything that is not a descriptor shortage is a real error about this file.
            Err(e) if !out_of_descriptors(&e) => {
                return Err(ArchiveError::Backend(format!("{}: {e}", path.display())));
            }
            Err(_) => {}
        }

        // **Release until the open succeeds, rather than once.** A single drain was not enough and
        // could not have been: a block holds up to SOLID_BLOCK_ENTRIES handles and there are up to
        // `inflight_max` packs holding as many again, so the writer can want 8192 × (24 + 1) handles
        // on a 24-thread machine — around 205,000, against a raised soft limit of 65,536. Creating a
        // `.7z` of the 86,618-file kernel tree failed outright with `Too many open files`, on a tree
        // 7-Zip archives without complaint.
        //
        // Cheapest first: a finished pack costs only the wait for a worker that is probably already
        // done, while flushing the open block gives up solid compression across the entries in it.
        // Retry after every step, so a run that meets the limit early does the least work that gets
        // it past.
        //
        // Deliberately still remembers nothing. An earlier version ratcheted the entry cap down here
        // and made a momentary shortage permanent: a Defender scan briefly consumed handles mid-run,
        // the cap stuck at a few hundred entries, every later block fell under the size where LZMA2
        // MT is worth starting, and the archive took 160s instead of 86s with 105 packs instead of
        // 34. Meeting the limit again next block costs one failed open, which is cheaper than any
        // amount of remembering.
        while self.drain_one_pack()? {
            if let Ok(f) = File::open(path) {
                return Ok(f);
            }
        }
        if !self.block.is_empty() {
            // Flushing hands this block's handles to a worker, which keeps them until it is done,
            // so the flush alone can release nothing at all. Drain what it queued.
            self.flush_block()?;
            while self.drain_one_pack()? {
                if let Ok(f) = File::open(path) {
                    return Ok(f);
                }
            }
        }
        File::open(path).map_err(|e| ArchiveError::Backend(format!("{}: {e}", path.display())))
    }

    /// Hand the open block to the pool as one pack, and start a new one.
    ///
    /// The compression happens on a worker; only the append is ordered. `push_archive_entries`
    /// compresses straight into the output stream, so packs could never overlap and every thread had
    /// to come from LZMA2 chunking *within* one pack — capped at pack size ÷ dictionary, and costing
    /// ratio because a match cannot cross a chunk boundary. Whole packs in parallel lifts both
    /// limits at once, and needs `prepare_pack`, which this build patches into `sevenz-rust2`.
    fn flush_block(&mut self) -> Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.block);
        let block_bytes = self.block_bytes;
        self.block_bytes = 0;
        let methods = Arc::new(self.content_methods(self.block_store, block_bytes));

        let mut entries = Vec::with_capacity(batch.len());
        let mut readers = Vec::with_capacity(batch.len());
        for (entry, source) in batch {
            entries.push(entry);
            readers.push(SourceReader::from(source));
        }

        self.drain_packs_to_limit()?;
        let (tx, rx) = sync_channel(1);
        rayon::spawn_fifo(move || {
            // FIFO for the same reason as the ZIP writer: the appending thread blocks on the OLDEST
            // pack, and a LIFO queue runs that one last.
            let _ = tx.send(prepare_pack(methods, entries, readers).map_err(map_sz));
        });
        self.pending_packs.push_back(rx);
        Ok(())
    }

    /// Append the oldest finished pack. `Ok(false)` when nothing is in flight.
    fn drain_one_pack(&mut self) -> Result<bool> {
        let Some(rx) = self.pending_packs.pop_front() else {
            return Ok(false);
        };
        let pack = rx
            .recv()
            .map_err(|_| ArchiveError::Backend("7z: pack compression worker died".into()))??;
        self.writer()?.push_prepared_pack(pack).map_err(map_sz)?;
        self.packs_written += 1;
        Ok(true)
    }

    fn drain_packs_to_limit(&mut self) -> Result<()> {
        while self.pending_packs.len() >= self.inflight_max {
            if !self.drain_one_pack()? {
                break;
            }
        }
        Ok(())
    }

    fn drain_all_packs(&mut self) -> Result<()> {
        while self.drain_one_pack()? {}
        Ok(())
    }

    /// Queue one entry into the open block, flushing first if this entry cannot share it.
    fn push_into_block(
        &mut self,
        entry: &Entry,
        source: BlockSource,
        store: bool,
        adaptive_store: bool,
    ) -> Result<()> {
        // A pack carries one method chain, so COPY and LZMA2 entries cannot share a block.
        if !self.block.is_empty() && self.block_store != store {
            self.flush_block()?;
        }
        self.block_store = store;
        self.block
            .push((ArchiveEntry::new_file(&arc_name(entry)), source));
        self.block_bytes = self.block_bytes.saturating_add(entry.size);
        self.entries += 1;
        self.in_bytes += entry.size;
        if adaptive_store {
            self.stored += 1;
        }
        if self.block_bytes >= self.solid_max || self.block.len() >= SOLID_BLOCK_ENTRIES {
            self.flush_block()?;
        }
        Ok(())
    }

    /// Write one entry as its own independently-decodable pack, streaming it. The pre-solid
    /// behaviour, still used for `CRAM_7Z_SOLID=0`, for `conv`'s oversized entries, and for anything
    /// too large to sit in a block.
    fn add_file_direct(
        &mut self,
        entry: &Entry,
        body: &mut dyn io::Read,
        store: bool,
        adaptive_store: bool,
    ) -> Result<()> {
        let methods = self.content_methods(store, entry.size);
        let sz_entry = ArchiveEntry::new_file(&arc_name(entry));
        let w = self.writer()?;
        w.set_content_methods(methods);
        w.push_archive_entry(sz_entry, Some(body)).map_err(map_sz)?;
        self.entries += 1;
        self.in_bytes += entry.size;
        if adaptive_store {
            self.stored += 1;
        }
        Ok(())
    }

    fn writer(&mut self) -> Result<&mut SzWriter<File>> {
        self.sz
            .as_mut()
            .ok_or_else(|| ArchiveError::Backend("7z writer already finished".into()))
    }

    /// The content-method chain for one entry: COPY when storing, else LZMA2; wrapped in AES
    /// (applied last, so compress-then-encrypt) when the archive is encrypted.
    /// `bytes` is how much uncompressed data this pack will hold. It gates multi-threading: an
    /// LZMA2 MT encoder allocates a match-finder state per thread — about 88 MB each at the default
    /// dictionary — so asking for eight threads to compress a pack smaller than one chunk spends
    /// several hundred megabytes and all the setup to do work one thread would finish immediately.
    /// A 1,200-file / 245 KB fixture took **62.9s** in a debug build before this check, essentially
    /// all of it encoder setup across three tiny blocks.
    fn content_methods(&self, store: bool, bytes: u64) -> Vec<EncoderConfiguration> {
        let compress: EncoderConfiguration = if store {
            EncoderConfiguration::new(EncoderMethod::COPY)
        } else if self.solid {
            // Single-threaded on purpose: parallelism comes from compressing whole packs at once
            // now, and LZMA2's own chunked MT costs ratio because a match cannot cross a chunk
            // boundary. One uninterrupted stream per pack is both faster in aggregate and smaller.
            Lzma2Options::from_level(self.level).into()
        } else if self.threads > 1 && bytes >= self.chunk.saturating_mul(2) {
            // LZMA2's own multi-threading splits the input into independent chunks, each clamped to
            // at least the dictionary. That does nothing for a 27 KB entry in its own pack, which is
            // why non-solid create could never use more than one core; inside a 64 MiB block it is
            // several chunks and several threads. It costs some ratio -- matches no longer cross a
            // chunk boundary -- but the dictionary is still shared by the thousands of small files
            // *within* each chunk, which is where the solid win actually comes from.
            //
            // The chunk must be a FRACTION of the block. Passing the block size itself makes every
            // block exactly one chunk and therefore one thread, which is how the first version of
            // this got the solid size win and none of the speed.
            //
            // It should NOT be block ÷ threads either, which was the second version: that pins the
            // chunk count to the thread count, so every block ends with all threads draining a
            // partial tail before the next one can start. Defaulting to the dictionary size -- the
            // smallest the library will accept -- gives the most chunks a block can hold, so there
            // is still work to pick up while the stragglers finish.
            Lzma2Options::from_level_mt(self.level, self.threads, self.chunk).into()
        } else {
            Lzma2Options::from_level(self.level).into()
        };
        match &self.aes_pw {
            Some(pw) => {
                // `new` gives a fresh random IV each call (one per entry); pin the archive-wide
                // salt and the 7-Zip-standard KDF work factor over the library defaults.
                let mut aes = AesEncoderOptions::new(Password::new(pw.expose()));
                aes.salt = self.aes_salt;
                aes.num_cycles_power = AES_CYCLES_POWER;
                vec![aes.into(), compress]
            }
            None => vec![compress],
        }
    }
}

impl ArchiveWriter for SevenZArchiveWriter {
    fn takes_paths(&self) -> bool {
        self.solid
    }

    fn add_path(&mut self, entry: &Entry, path: &Path, hint: WriteHint) -> Result<()> {
        let file = self.open_for_block(path)?;

        // Turning `takes_paths` on stops the engine probing inline, and a block must know every
        // entry's verdict before it picks its single method chain. Same order of checks as
        // `probe::classify_file`, sampled from the handle that is about to be kept -- getting this
        // wrong took `sevenz_auto_stores_incompressible_per_entry` from 2 stored entries to 0, with
        // no symptom but a larger archive.
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
        // The sample goes back in front of the handle rather than being re-read. `head` is empty
        // whenever no sampling happened, and chaining an empty cursor costs nothing.
        let stream = Cursor::new(head).chain(file);

        let store = hint.store || self.store_forced;
        let adaptive_store = hint.store && !self.store_forced;
        // An oversized entry never joins a block -- it is the one thing that could defeat the
        // ceilings -- so flush what is open and stream it as its own pack.
        if entry.size > self.solid_max {
            self.flush_block()?;
            let mut stream = stream;
            return self.add_file_direct(entry, &mut stream, store, adaptive_store);
        }
        self.push_into_block(entry, BlockSource::Stream(stream), store, adaptive_store)
    }

    fn add_file(&mut self, entry: &Entry, body: &mut dyn io::Read, hint: WriteHint) -> Result<()> {
        let store = hint.store || self.store_forced;
        let adaptive_store = hint.store && !self.store_forced;
        if self.solid && entry.size <= self.solid_max {
            // `conv` supplies a reader with no file behind it, so these bytes must be held.
            let mut buf = Vec::with_capacity(entry.size.min(1 << 20) as usize);
            body.read_to_end(&mut buf)?;
            return self.push_into_block(
                entry,
                BlockSource::Bytes(Cursor::new(buf)),
                store,
                adaptive_store,
            );
        }
        if self.solid {
            self.flush_block()?;
        }
        self.add_file_direct(entry, body, store, adaptive_store)
    }

    fn add_dir(&mut self, entry: &Entry) -> Result<()> {
        let sz_entry = ArchiveEntry::new_directory(&arc_name(entry));
        if self.solid {
            // Deferred to `finish`; see the `dirs` field for why.
            self.dirs.push(sz_entry);
            self.entries += 1;
            return Ok(());
        }
        self.writer()?
            .push_archive_entry(sz_entry, None::<io::Empty>)
            .map_err(map_sz)?;
        self.entries += 1;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<CreateReport> {
        self.flush_block()?;
        self.drain_all_packs()?;
        if std::env::var_os("CRAM_PROFILE").is_some() {
            eprintln!(
                "-- 7z blocks --------------------------------------------------\n\
                 packs {}  entries/block max {}  solid_max {} MiB  chunk {} MiB  threads {}",
                self.packs_written,
                SOLID_BLOCK_ENTRIES,
                self.solid_max >> 20,
                self.chunk >> 20,
                self.threads
            );
        }
        let dirs = std::mem::take(&mut self.dirs);
        for dir in dirs {
            self.writer()?
                .push_archive_entry(dir, None::<io::Empty>)
                .map_err(map_sz)?;
        }
        let mut sz = self
            .sz
            .take()
            .ok_or_else(|| ArchiveError::Backend("7z writer already finished".into()))?;
        if self.aes_pw.is_some() {
            // Install a FRESH AES configuration for the header pass. The library encrypts the
            // header by cloning the AES entry of the *current* content-method chain, so without
            // this: (a) the header reuses the LAST entry's IV, and (b) an archive that never had
            // an `add_file` (empty, or directories only) has no AES configuration at all and a
            // NamesToo header is silently written in PLAINTEXT.
            sz.set_content_methods(self.content_methods(false, 0));
        }
        let file = sz.finish().map_err(ArchiveError::Io)?;
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
