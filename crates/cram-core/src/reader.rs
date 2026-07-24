//! The read side of the engine, the spine every backend implements.
//!
//! Backends stay *dumb*: they only yield entry metadata and entry *bodies*. All the write-loop
//! machinery, output paths, overwrite/skip policy, progress, cancellation, and the parallel
//! scheduler, lives in the `engine` layer, so it's written once and every format inherits it.
//!
//! Two shapes:
//!
//! - **Sequential** ([`ArchiveReader::next_entry`]), a front-to-back stream of `(Entry, body)`.
//!   Works over a plain `Read`, so it covers tar, raw single-stream codecs, and *streaming* ZIP
//!   (extract-while-download). `EntryStream::meta_final = false` marks optimistic metadata (a ZIP
//!   local header / data-descriptor whose CRC/size is only confirmed by the trailing record).
//! - **Random-access** ([`RandomAccessReader`]), the capability **ZIP and `.cram`** offer (both
//!   seekable/individually-addressable). [`copy_entry`](RandomAccessReader::copy_entry) streams one entry into a
//!   caller-supplied writer from its own file handle, so the parallel rayon extraction path fans out
//!   *and* the engine keeps owning file-creation / overwrite policy / progress;
//!   [`read_range`](RandomAccessReader::read_range) serves a byte-range of an entry's *uncompressed*
//!   stream, the ProjFS mount / on-access primitive.
//!
//! Why `copy_entry(&mut dyn Write)` and not `entry_reader() -> Box<dyn Read>`: the `zip` crate's
//! per-entry reader borrows its `ZipArchive`, so handing back an owned streaming reader would force
//! either buffering the whole entry in RAM (fatal for a few-huge-entries archive × N workers) or a
//! self-referential wrapper. Injecting the writer sidesteps both, streams, and still handles
//! encryption and every compression method through the backend's own decoder.
//!
//! **Passwords** are not threaded through these methods. A concrete reader captures an
//! `Arc<dyn PasswordProvider>` at open time (see `formats::open`) and pulls the password lazily the
//! first time it must decrypt, header up front for encrypted-name archives, per entry otherwise.

use std::io::{Read, Write};

use crate::error::Result;
use crate::format::Format;
use crate::model::Entry;

/// One streamed member: its metadata plus a reader over its uncompressed bytes.
pub struct EntryStream<'a> {
    pub entry: Entry,
    /// Uncompressed bytes of this entry. Borrows the reader, so it must be drained before the next
    /// `next_entry` call.
    pub body: Box<dyn Read + 'a>,
    /// `true` when `entry`'s size/CRC are authoritative now; `false` for optimistic streaming
    /// metadata that the engine must reconcile against the trailing record once the tail arrives.
    pub meta_final: bool,
}

/// A parsed archive. Used single-threaded (on the calling thread); cross-thread *parallel* reads go
/// through [`RandomAccessReader`] (which is `Send + Sync`), never this trait, so this stays
/// unbounded, letting backends over a non-`Send` C handle (RAR/UnRAR) implement it.
pub trait ArchiveReader {
    /// The detected container × codec.
    fn format(&self) -> Format;

    /// The full member list (metadata only, no decode). Cheap: a central-directory / header scan.
    /// `Result` because encrypted-name archives can't produce it until the header password is in.
    fn entries(&self) -> Result<&[Entry]>;

    /// Pull the next member as a stream, or `None` at end of archive. The sequential path used for
    /// tar / raw / streaming-ZIP; the returned body borrows `self`.
    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>>;

    /// Random-access capability, `Some` for seekable containers (ZIP and `.cram`). Its presence is
    /// what lets the orchestrator choose the tuned parallel per-entry path over the sequential one.
    fn as_random_access(&self) -> Option<&dyn RandomAccessReader> {
        None
    }
}

/// Per-entry random access (ZIP and `.cram`; later ranged 7z). `Send + Sync` and safe to call
/// from many rayon workers at once, implementors open their own handle per call rather than
/// sharing a cursor, so many workers extract independently.
pub trait RandomAccessReader: Send + Sync {
    /// The full member list (already scanned at open).
    fn entries(&self) -> &[Entry];

    /// Decompress entry `index` and stream its uncompressed bytes into `out`, **safe to call
    /// concurrently** (opens its own file handle). This is the parallel seam the rayon per-entry
    /// path fans out over; the engine passes a writer that owns the destination file plus the
    /// progress/cancel wrapper, so those concerns stay in one place. Returns bytes written.
    fn copy_entry(&self, index: usize, out: &mut dyn Write) -> Result<u64>;

    /// A random byte-range `[off, off+len)` of an entry's *uncompressed* stream; the mount /
    /// on-access primitive. For a non-seekable inner codec this may decode from the entry start.
    fn read_range(&self, index: usize, off: u64, len: u64) -> Result<Vec<u8>>;
}
