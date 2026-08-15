//! The write side of the engine, one [`ArchiveWriter`] every *creatable* container implements
//! (ZIP, 7z, tar-family, `.cram`, never RAR: creating RAR is forbidden by the UnRAR license, and
//! [`Format::is_writable`](crate::format::Format::is_writable) already returns false for it).
//!
//! The trait is **incremental** (`add_file` / `add_dir` … `finish`) rather than one batch call, so
//! the engine can feed entries as it walks the source tree, a dedup writer can chunk each body as
//! it arrives, and progress flows naturally. Creation knobs, level, codec, encryption, threads;
//! are fixed for the whole archive and passed once at construction (`formats::create`), captured in
//! [`CreateOptions`]; the writer holds the [`EncryptSpec`] and pulls no password lazily (the user
//! supplied it when they chose to encrypt).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Result;
use crate::format::Codec;
use crate::model::Entry;
use crate::secret::EncryptSpec;

/// One file/dir to place into a new archive: where it is on disk and the name it takes inside.
#[derive(Debug, Clone)]
pub struct SourceEntry {
    pub disk_path: PathBuf,
    /// Name stored in the archive (a relative path; still re-validated on write).
    pub archive_name: String,
}

/// Per-entry compression hint from the adaptive probe ([`crate::probe`]). Backends that select a
/// method per entry (ZIP, and 7z in its non-solid per-entry mode) honor `store`; whole-stream
/// backends (tar) ignore it (they compress the concatenated stream as a whole).
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteHint {
    /// This entry looks already-compressed (media/archive/high-entropy) → store it verbatim
    /// instead of running the codec over it.
    pub store: bool,
}

/// Compression effort. `Auto` lets the adaptive `probe` choose codec+level+block layout from the
/// data; the rest map onto each codec's own scale at write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    #[default]
    Auto,
    Fastest,
    Balanced,
    Best,
    /// The smallest archive reachable in a reasonable time, with speed **explicitly secondary**.
    ///
    /// Where [`Best`](Self::Best) makes one good attempt per pack, this searches: LZMA's extreme
    /// parameters, a wider window, and a set of pre-filters and coder parameters tried per pack with
    /// the smallest result kept. The search is worth having because the answer is genuinely
    /// content-dependent -- the x86 BCJ filter took Silesia's `ooffice` down 14.1% and made
    /// `mozilla` 0.9% *larger* -- so it cannot be a default, only a candidate.
    ///
    /// Only `.cram` distinguishes this from `Best`; every other container treats the two alike.
    Cold,
    /// Smaller than [`Cold`](Self::Cold) where a slower encoder exists for the same format, at a cost
    /// in time that is not proportionate and is not meant to be.
    ///
    /// Today that means exactly one thing: `.zip` is written with **zopfli** instead of the ordinary
    /// DEFLATE encoder. Zopfli emits a bit-identical-format DEFLATE stream that every unzip on earth
    /// already reads — it simply searches much harder for it, so nothing about the archive is
    /// unusual except that it is smaller.
    ///
    /// It is a **separate rung rather than part of `Cold`** because the trade is different in kind.
    /// `Cold` is slower for a better answer; this is slower for a *slightly* better answer, and the
    /// multiple is large enough that folding it into `--small` would make that flag mean something
    /// people would stop using. Every container without a slower encoder to reach for treats this
    /// exactly as `Cold`.
    Tiny,
    /// Raw codec level (meaning is codec-specific); clamped to the codec's valid range.
    Explicit(u32),
}

/// Whole-archive creation settings, fixed at construction.
///
/// `Default` is hand-written rather than derived because [`recompress_images`](Self::recompress_images)
/// must default to **on**, which a derive cannot express for a `bool`.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub level: Level,
    /// `None` = no encryption. `Some` carries the password + the two locked forks (ZIP cipher,
    /// header mode).
    pub encrypt: Option<EncryptSpec>,
    /// Force a codec; `None` = the container default / adaptive choice.
    pub codec: Option<Codec>,
    /// Solid compression (7z): pack members into one stream for a better ratio at the cost of
    /// random access. The adaptive engine may still author multi-block layouts so *our own*
    /// extraction parallelizes.
    ///
    /// **Defaults to `true`, and did in behaviour long before it did here.** The 7z writer read an
    /// environment variable and ignored this field entirely, so the struct said `false` while every
    /// archive came out solid. A caller reading `CreateOptions` was told the opposite of what it
    /// would get.
    pub solid: bool,
    /// Worker threads; `None` = derive from [`hw::derive_plan`](crate::hw).
    pub threads: Option<usize>,
    /// Total uncompressed bytes about to be written, when the caller has already counted them.
    ///
    /// A hint, not a contract: it is allowed to be absent, stale or wrong, and no backend may depend
    /// on it for correctness. It exists because **brotli picks its hash table from this number and
    /// nothing else**. `CompressorWriter::new` leaves `size_hint` at 0, and brotli's `ChooseHasher`
    /// only reaches H6 (15 bucket bits, 5-byte hash) when the hint exceeds 4 MiB — below that a
    /// quality-6 stream gets H5 with **14** bucket bits. A 16K-bucket table saturates on a large
    /// input, so a `.tar.br` came out 16.1% larger than `brotli -q 6` on 203 MiB and 23.55% larger on
    /// 2.1 GB. The `brotli` CLI sets the hint from the file size; we had no way to.
    pub total_bytes: Option<u64>,
    /// Losslessly recompress JPEGs when writing `.cram` (**on by default**).
    ///
    /// A JPEG's entropy coding is redone with a stronger coder and the original file is reconstructed
    /// byte-for-byte on extract, worth ~23% on real photos, where zip and 7z manage ~0% because the
    /// data is already entropy-coded. Every candidate is verified to round-trip before it is stored,
    /// and anything that fails is kept verbatim, so turning this off only costs space. Ignored by
    /// containers other than `.cram`.
    pub recompress_images: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            level: Level::default(),
            encrypt: None,
            codec: None,
            solid: true,
            threads: None,
            total_bytes: None,
            recompress_images: true,
        }
    }
}

/// Outcome of a creation job, carries the ratio inputs and any dedup win the GUI/CLI reports.
#[derive(Debug, Clone, Default)]
pub struct CreateReport {
    pub entries: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    /// Entries the adaptive probe stored verbatim because they were already-compressed
    /// (incompressible). 0 when the level forces store-all, or for whole-stream backends.
    pub stored: u64,
    /// Bytes eliminated by cross-file dedup (`.cram` only; 0 for classic containers).
    pub dedup_saved: u64,
    pub elapsed: Duration,
    /// Archive names of symbolic links the walk found and did **not** archive.
    ///
    /// The `.cram` v1 index has nowhere to record a link target (`EntryMeta` carries `is_dir`, name,
    /// size, `mode` and chunk ids, and `mode` is specified as permission bits), so a symlink cannot
    /// be represented and is left out. Every caller that reports a successful create **must** report
    /// this too when it is non-empty: an archive that quietly contains less than the tree it was
    /// made from is the failure a backup tool cannot afford, and `cram t` will call such an archive
    /// perfectly clean because by its own index it is.
    ///
    /// Empty for every archive without symlinks, which is the overwhelming majority.
    pub skipped_links: Vec<String>,
}

/// Builds an archive incrementally from on-disk sources. Only writable containers implement this.
pub trait ArchiveWriter: Send {
    /// Add one file, streaming its uncompressed bytes from `body`. `hint` carries the adaptive
    /// probe's per-entry decision (store the already-compressed verbatim); backends that can't
    /// vary the method per entry ignore it.
    fn add_file(&mut self, entry: &Entry, body: &mut dyn Read, hint: WriteHint) -> Result<()>;

    /// Add a directory entry (no body).
    fn add_dir(&mut self, entry: &Entry) -> Result<()>;

    /// Finalize the archive (write the central directory / footer index) and report totals.
    fn finish(self: Box<Self>) -> Result<CreateReport>;

    /// Whether [`add_path`](Self::add_path) reads the file itself, on its own schedule.
    ///
    /// The engine asks once, before the loop, and when the answer is yes it hands over the path
    /// instead of opening the file, sampling it for a [`WriteHint`] and streaming it. Only `.cram`
    /// says yes.
    fn takes_paths(&self) -> bool {
        false
    }

    /// Add one file by path rather than by reader.
    ///
    /// The default opens it and calls [`add_file`](Self::add_file), which is exactly what the engine
    /// did inline before this existed, so a backend that does not override it behaves as it always
    /// did and `takes_paths` returning false costs nothing.
    ///
    /// `.cram` overrides it. Its per-file cost is FastCDC boundary search, BLAKE3 and the optional
    /// Lepton pass, all three pure functions of that one file's bytes, so it hands the path to a
    /// worker pool and returns before any of the work is done. Everything order-dependent -- which
    /// chunk is the first occurrence of its hash, what id it gets, which pack it lands in -- still
    /// happens on one thread in entry order, so the archive is unchanged.
    fn add_path(&mut self, entry: &Entry, path: &Path, hint: WriteHint) -> Result<()> {
        let mut file = std::fs::File::open(path)
            .map_err(|e| crate::error::ArchiveError::Backend(format!("{}: {e}", path.display())))?;
        self.add_file(entry, &mut file, hint)
    }
}
