//! The read side of the engine, the core every backend implements.
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
//! - **Random-access** ([`RandomAccessReader`]), the capability **ZIP, 7z, `.cram` and ISO 9660**
//!   offer (all seekable/individually-addressable; 7z through its block index and, where the archive
//!   carries LZMA2 dictionary resets, the segments inside a block).
//!   [`copy_entry`](RandomAccessReader::copy_entry) streams one entry into a
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

    /// The full member list (metadata only, no decode). Usually cheap: a central-directory or
    /// header scan. `Result` because encrypted-name archives can't produce it until the header
    /// password is in.
    ///
    /// **Not cheap for every backend** — see [`entries_are_cheap`](Self::entries_are_cheap).
    fn entries(&self) -> Result<&[Entry]>;

    /// Whether [`entries`](Self::entries) can be answered without decoding the archive.
    ///
    /// A compressed tar cannot: its headers are interleaved with the bodies, so "skipping" a member
    /// still decodes it, and building the list costs a **full pass**. An extraction that bought the
    /// list first therefore decoded the archive twice, and because both passes decode the same bytes
    /// the cost is close to an exact doubling, for every codec.
    ///
    /// A caller that wants the listing (`cram l`) should ask for it and pay. A caller that is about
    /// to stream every entry anyway should not, so the engine plans without it: for a tar the list
    /// contributes nothing to the plan in any case, since `block_count` returns 1 for the container
    /// and `plan_codec` reads only the codec.
    ///
    /// What one pass costs, so the saving has a scale: cram extracts the kernel tree's `.tar.bz2` in
    /// 3.14 s and its `.tar.xz` in 2.85 s — one machine, medians of 3, `/dev/shm` destination with
    /// the archive read into page cache first, 16 August 2026.
    fn entries_are_cheap(&self) -> bool {
        true
    }

    /// Pull the next member as a stream, or `None` at end of archive. The sequential path used for
    /// tar / raw / streaming-ZIP; the returned body borrows `self`.
    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>>;

    /// Random-access capability, `Some` for seekable containers (ZIP, 7z, `.cram`, ISO 9660). Its
    /// presence is what lets the orchestrator choose the tuned parallel per-entry path over the
    /// sequential one.
    fn as_random_access(&self) -> Option<&dyn RandomAccessReader> {
        None
    }
}

/// Per-entry random access (ZIP, 7z, `.cram`, ISO 9660). `Send + Sync` and safe to call
/// from many rayon workers at once, implementors open their own handle per call rather than
/// sharing a cursor, so many workers extract independently. Ranged 7z has landed: see
/// [`entry_splits`](Self::entry_splits), `formats::sevenz`, and the segment cutter in
/// `formats::lzma2seg`.
pub trait RandomAccessReader: Send + Sync {
    /// The full member list (already scanned at open).
    fn entries(&self) -> &[Entry];

    /// Decompress entry `index` and stream its uncompressed bytes into `out`, **safe to call
    /// concurrently** (opens its own file handle). This is the parallel hand-off point the rayon per-entry
    /// path fans out over; the engine passes a writer that owns the destination file plus the
    /// progress/cancel wrapper, so those concerns stay in one place. Returns bytes written.
    fn copy_entry(&self, index: usize, out: &mut dyn Write) -> Result<u64>;

    /// A random byte-range `[off, off+len)` of an entry's *uncompressed* stream; the mount /
    /// on-access primitive. For a non-seekable inner codec this may decode from the entry start.
    fn read_range(&self, index: usize, off: u64, len: u64) -> Result<Vec<u8>>;

    /// Where entry `index`'s bytes physically live, when the format stores many entries inside one
    /// shared decode unit. Entries sharing a key should be extracted together.
    ///
    /// `None`, the default, means entries are independent: ZIP, tar and RAR decode each member on
    /// its own, so any order costs the same. `.cram` returns the entry's first pack, because a pack
    /// is decompressed whole and holds chunks from many files.
    ///
    /// This is not a nicety. Extracting a 94,778-file `.cram` while scheduling purely by entry
    /// weight scattered concurrent workers across unrelated packs, thrashed the 32-slot pack cache,
    /// and re-decoded packs so many times that the anti-decompression-bomb budget tripped: `cram x`
    /// failed on 60,052 entries of an archive `cram t` verifies completely in 3.15 s. Ordering by
    /// this key means each shared unit is decoded about once, which fixes the throughput and the
    /// false positive together -- and makes the amplification attack the budget guards against
    /// structurally impossible, since no arrangement of chunks can force a re-decode of a unit that
    /// is visited once.
    ///
    /// 7z solid blocks have the same shape and now use it, both the block and, where the archive
    /// carries LZMA2 dictionary resets, each independently decodable segment inside one. Measured:
    /// clustering alone was not enough there either, and cost 110 CPU-seconds against 11 — see
    /// [`coalesce_locality`](Self::coalesce_locality).
    fn locality_key(&self, _index: usize) -> Option<u64> {
        None
    }

    /// Whether every entry sharing a [`locality_key`](Self::locality_key) should become ONE work
    /// item rather than many adjacent ones.
    ///
    /// Clustering alone only makes same-unit groups *adjacent* in the schedule. Rayon still steals
    /// across that list, so a worker hops between units and a backend that caches "the unit I am
    /// currently decoding" evicts on every hop. Measured on a 34-block 7z: entries stayed adjacent,
    /// workers scattered anyway, and decoding the archive cost **110 CPU-seconds instead of 11** —
    /// ten re-decodes of every block — for almost no wall-clock gain.
    ///
    /// Coalescing trades load-balancing granularity for that: 34 items instead of 41,305, each
    /// decoded exactly once. Right when a unit is expensive to decode and holds many entries; wrong
    /// when entries decode independently, which is why it is off by default.
    fn coalesce_locality(&self) -> bool {
        false
    }

    /// Whether this backend can serve a whole decode unit in ONE pass, streaming each entry's bytes
    /// as they emerge, so nothing is held. See [`copy_unit`](Self::copy_unit).
    fn streams_units(&self) -> bool {
        false
    }

    /// Where a *single* entry can be cut so its pieces decode independently, as byte ranges covering
    /// it in order. `None`, the default, means it has to be produced in one pass.
    ///
    /// This is the other half of [`decode_units`](Self::decode_units). That one says how wide the
    /// archive can fan out; this one says how wide **one entry** can, which is the number that
    /// matters when an archive holds very few of them. A 1 GB `.cram` of a single file spans sixty
    /// independent packs and still extracted on one thread, because the engine's unit of work is the
    /// entry and there was only ever one — 9.03 s against 7-Zip's 1.61 s, at 1.0 effective cores
    /// against 4.4.
    ///
    /// Ranges must be **aligned to the backend's own decode boundaries**. Cutting mid-unit would
    /// make neighbouring workers decode the same unit twice, turning a fan-out into extra work; the
    /// engine cannot know where those boundaries are, so the backend picks them.
    ///
    /// Only consulted for an entry large enough to be worth splitting, and only when the extraction
    /// has workers to spare. Returning `Some` with fewer than two ranges is the same as `None`.
    fn entry_splits(&self, _index: usize) -> Option<Vec<(u64, u64)>> {
        None
    }

    /// Serve `indices` in one pass over their decode unit, handing each entry's index and a reader
    /// over its uncompressed bytes to `visit`, in archive order. `visit` returns `false` to stop the
    /// pass (cancellation). Only called when [`streams_units`](Self::streams_units) is `true`.
    ///
    /// This exists because [`copy_entry`](Self::copy_entry) is the wrong shape for a solid format.
    /// It addresses one entry, so serving a block's entries through it means decoding the block and
    /// *keeping* it: 7z extraction of the benchmark corpus peaked at 1.8 GB of RSS that way, against
    /// 190 MB once the bytes were handed over as they emerged. Measured when this landed and not
    /// re-measured since. The 176 MB 7-Zip figure that used to sit beside it is dropped rather than
    /// carried forward: 7-Zip's measured peak across the four corpora of the 16 August 2026 run is
    /// 255 / 1176 / 2284 / 4877 MB, and none of those is near it. The bytes are already flowing past
    /// in the right order; the only reason to hold them was that the interface had no way to hand
    /// them over as they went by.
    ///
    /// `visit` is not required to read the body it is given. The implementor must drain whatever is
    /// left before moving on, since a solid stream has to be advanced past an entry to reach the
    /// next one — that is what makes "skip this entry" cost no memory rather than no time.
    ///
    /// The engine still owns every write-side decision. `visit` is where skip policy, file creation,
    /// progress and cleanup happen, exactly as they do on the per-entry path.
    fn copy_unit(
        &self,
        _indices: &[usize],
        _visit: &mut dyn FnMut(usize, &mut dyn Read) -> bool,
    ) -> Result<()> {
        Err(crate::error::ArchiveError::Backend(
            "this backend does not stream decode units".into(),
        ))
    }

    /// How many units of work can be decoded independently, when the backend knows a number the
    /// entry list cannot express. `.cram` returns its pack count; a format whose members decode on
    /// their own returns `None` and the planner counts entries instead.
    ///
    /// This is what `hw::derive_plan` fans out over, so a wrong answer here is a wrong thread count
    /// everywhere. `codec::plan::block_count` used to answer `1` for every container it had no rule
    /// for, `.cram` included, which made the CPU-bound plan `min(1, cores)` and put a 24-thread
    /// machine on one worker. Extraction hid it by landing in the write-bound branch instead, and
    /// `cram t`, which writes nothing and so is always CPU-bound, did not: it verified a 1.6 GB
    /// archive at 96% of one core while 7-Zip used three and a half.
    fn decode_units(&self) -> Option<usize> {
        None
    }
}
