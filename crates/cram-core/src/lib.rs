//! Cram core engine — the format-agnostic read/write/verify machinery every Cram tool builds on.
//!
//! The design is a thin **spine** of two traits ([`reader`], [`writer`]) that each container backend
//! implements, with all the shared machinery — output-path safety, overwrite/skip policy, progress,
//! cancellation, and the adaptive parallel scheduler — living once in [`engine`] so every format
//! inherits it. Adding a format is implementing the spine, not re-plumbing the engine.
//!
//! ## The layers
//!
//! - [`format`](mod@format) / [`sniff`] — `Format = Container × Codec`; magic-byte detection (extension only as a
//!   tiebreaker). Compound formats like `.tar.gz` compose a container with a whole-stream codec.
//! - [`model`] — the unified [`Entry`] metadata every backend yields, and the centralized zip-slip
//!   guard ([`EntryPath`]): the one place archive names are normalized so no backend can escape the
//!   output directory.
//! - [`reader`] — [`ArchiveReader`] (sequential `next_entry` stream) plus the [`RandomAccessReader`]
//!   capability (`copy_entry` = the parallel per-entry seam; `read_range` = the mount primitive).
//! - [`writer`] — the incremental [`ArchiveWriter`] (`add_file` / `add_dir` / `finish`) and its
//!   [`CreateOptions`] / [`CreateReport`].
//! - [`formats`] — the concrete backends: ZIP, 7z, tar-family, ISO 9660, RAR (read-only), raw
//!   single-stream codecs, and the native `.cram` dedup format. `formats::open` / `formats::create`
//!   map a `Format` to the right backend.
//! - [`codec`] — the byte-transform layer (`decode_stream`) plus the three-codec `plan` glue that
//!   bridges the on-disk codec to the cost model.
//! - [`hw`] — adaptive parallelism: hardware auto-detect, calibration, and a per-job `derive_plan`
//!   that sizes the worker pool from the machine profile and the archive's block shape.
//! - [`engine`] — the orchestrator: [`extract`], [`create`](engine::create), [`convert`](engine::convert),
//!   and [`verify`](engine::verify), dispatching random-access formats to the tuned parallel path and
//!   everything else to the sequential stream.
//! - [`secret`] — password handling: zeroized [`Secret`], lazy [`PasswordProvider`], [`EncryptSpec`].
//! - [`probe`] — the adaptive content classifier (store-vs-compress) that feeds create.
//! - [`source`] / `net` — the `ByteSource` abstraction and its rdm-backed download implementation
//!   (the `net` module is behind the `download` feature) for extract-while-downloading.
//!
//! The `.cram` container format is specified normatively in `docs/CRAM_FORMAT.md`; the ProjFS mount,
//! Reed-Solomon recovery sidecar, and ed25519 signing live in the sibling `cram-mount`,
//! `cram-recovery`, and `cram-sign` crates, and the unified `cram` CLI in `cram-cli`.

pub mod codec;
pub mod engine;
pub mod error;
pub mod format;
pub mod formats;
pub mod hw;
pub mod model;
pub mod probe;
pub mod progress;
pub mod reader;
pub mod secret;
pub mod sniff;
pub mod source;
pub mod writer;

/// rdm-backed download source for extract-while-download (`download` feature).
#[cfg(feature = "download")]
pub mod net;

pub use engine::estimate::{estimate_dedup, DedupEstimate};
pub use engine::{extract, ExtractOptions};
pub use error::{ArchiveError, Report, Result};
pub use format::{Codec, Container, Format};
pub use model::{Entry, EntryKind, EntryPath};
pub use probe::{Compressibility, ProbeSummary};
pub use progress::{CountingReader, CountingWriter, Progress, ProgressSink};
pub use reader::{ArchiveReader, EntryStream, RandomAccessReader};
pub use secret::{EncryptSpec, HeaderMode, PasswordProvider, PasswordRequest, Secret, ZipCipher};
pub use source::{BufferSource, ByteSource, SourceReader, SourceStatus};
pub use writer::{ArchiveWriter, CreateOptions, CreateReport, Level, SourceEntry, WriteHint};
