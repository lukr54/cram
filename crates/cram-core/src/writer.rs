//! The write side of the engine — one [`ArchiveWriter`] every *creatable* container implements
//! (ZIP, 7z, tar-family, `.cram` — never RAR: creating RAR is forbidden by the UnRAR license, and
//! [`Format::is_writable`](crate::format::Format::is_writable) already returns false for it).
//!
//! The trait is **incremental** (`add_file` / `add_dir` … `finish`) rather than one batch call, so
//! the engine can feed entries as it walks the source tree, a dedup writer can chunk each body as
//! it arrives, and progress flows naturally. Creation knobs — level, codec, encryption, threads —
//! are fixed for the whole archive and passed once at construction (`formats::create`), captured in
//! [`CreateOptions`]; the writer holds the [`EncryptSpec`] and pulls no password lazily (the user
//! supplied it when they chose to encrypt).

use std::io::Read;
use std::path::PathBuf;
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
    /// Raw codec level (meaning is codec-specific); clamped to the codec's valid range.
    Explicit(u32),
}

/// Whole-archive creation settings, fixed at construction.
#[derive(Debug, Clone, Default)]
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
    pub solid: bool,
    /// Worker threads; `None` = derive from [`hw::derive_plan`](crate::hw).
    pub threads: Option<usize>,
}

/// Outcome of a creation job — carries the ratio inputs and any dedup win the GUI/CLI reports.
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
}
