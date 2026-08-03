//! The three-codec glue. `derive_plan` needs one representative `hw::Codec` to cost-model and a
//! count of independent decode units to fan out over; these two functions derive both from a
//! [`Format`] plus its entry list, so the orchestrator never hand-maps codecs at the call site.

use crate::format::{Codec as StreamCodec, Container, Format};
use crate::hw::Codec as PlanCodec;
use crate::model::Entry;

/// Whole-stream `format::Codec` → its `hw::Codec` cost class.
fn stream_to_plan(codec: StreamCodec) -> PlanCodec {
    match codec {
        StreamCodec::None => PlanCodec::Store,
        StreamCodec::Gzip => PlanCodec::Deflate,
        StreamCodec::Xz => PlanCodec::Lzma,
        StreamCodec::Zstd => PlanCodec::Zstd,
        StreamCodec::Bzip2 => PlanCodec::Bzip2,
        StreamCodec::Lz4 => PlanCodec::Lz4,
        StreamCodec::Brotli => PlanCodec::Brotli,
    }
}

/// The representative codec `derive_plan` should cost-model for this archive. ZIP/7z entries carry
/// their own per-entry method, so we use the dominant case (ZIP ≈ DEFLATE, 7z ≈ LZMA); Tar/Raw/Cram
/// read the wrapping stream codec. (`entries` is unused today but kept in the signature for when
/// ZIP/7z start reporting a real per-entry method mix.)
pub fn plan_codec(fmt: Format, _entries: &[Entry]) -> PlanCodec {
    match fmt.container {
        Container::Zip => PlanCodec::Deflate,
        Container::SevenZ => PlanCodec::Lzma,
        // RAR decode is CPU-heavy; LZMA is the closest cost class for planning.
        Container::Rar => PlanCodec::Lzma,
        // ISO extents are uncompressed → its `None` codec maps to `Store` (a raw copy).
        Container::Tar | Container::Raw | Container::Cram | Container::Iso => {
            stream_to_plan(fmt.codec)
        }
    }
}

/// How many independent decode units the plan can parallelize over, **from the entry list alone**.
/// ZIP = one per file entry (each independently seekable/decodable, the parallel fast path).
///
/// A backend that groups entries into shared units knows a number this function cannot see, and
/// answers it from [`RandomAccessReader::decode_units`](crate::reader::RandomAccessReader::decode_units);
/// callers must prefer that and fall back here. `.cram` reports its pack count that way. Answering
/// `1` for it here used to make the CPU-bound plan `min(1, cores)`, which is how `cram t` ended up
/// verifying on a single thread.
pub fn block_count(fmt: Format, entries: &[Entry]) -> usize {
    match fmt.container {
        // ZIP and ISO expose per-file random access → one independent unit per file.
        Container::Zip | Container::Iso => entries.iter().filter(|e| !e.is_dir()).count().max(1),
        // TODO(7z): independent unit = folder count, which needs the backend's folder map. Treat as
        // one stream until then.
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntryKind, EntryPath};

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            index: 0,
            path: EntryPath::from_raw(name).unwrap(),
            kind,
            size: 1,
            compressed_size: None,
            modified: None,
            unix_mode: None,
            crc32: None,
            encrypted: false,
        }
    }

    #[test]
    fn plan_codec_picks_representative() {
        assert_eq!(plan_codec(Format::zip(), &[]), PlanCodec::Deflate);
        assert_eq!(plan_codec(Format::sevenz(), &[]), PlanCodec::Lzma);
        assert_eq!(
            plan_codec(Format::tar(StreamCodec::Gzip), &[]),
            PlanCodec::Deflate
        );
        assert_eq!(
            plan_codec(Format::tar(StreamCodec::Xz), &[]),
            PlanCodec::Lzma
        );
        assert_eq!(
            plan_codec(Format::raw(StreamCodec::Zstd), &[]),
            PlanCodec::Zstd
        );
    }

    #[test]
    fn block_count_counts_zip_files_only() {
        let entries = [
            entry("a.txt", EntryKind::File),
            entry("dir/", EntryKind::Dir),
            entry("b.txt", EntryKind::File),
        ];
        assert_eq!(block_count(Format::zip(), &entries), 2);
        // A non-random-access container is one stream regardless of entry count.
        assert_eq!(block_count(Format::tar(StreamCodec::Gzip), &entries), 1);
    }
}
