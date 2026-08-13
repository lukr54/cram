//! A single large `.cram` entry must extract across many workers, and come out byte-identical.
//!
//! The engine's unit of work is the entry, so an archive holding one file used one thread however
//! many cores were free: enwik9 took 9.03 s at 1.0 effective cores against 7-Zip's 1.61 s at 4.4.
//! A `.cram` entry is a list of chunks and every chunk names its pack, so the pack boundaries are
//! places the entry can be cut into pieces that decode independently.
//!
//! What these tests pin is the part that would break silently. A cut in the wrong place still
//! produces correct output — it just makes two workers decode the same pack, turning a fan-out into
//! extra work, and nothing would fail.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;
use cram_core::{engine, formats};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-big-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Compressible but not trivially so, and varied enough that the chunker cuts it into many chunks
/// across several packs rather than deduplicating it down to nothing.
fn varied(len: usize) -> Vec<u8> {
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let words = [
        "the",
        "quick",
        "brown",
        "fox",
        "jumps",
        "over",
        "lazy",
        "dog",
        "lorem",
        "ipsum",
        "dolor",
        "sit",
        "amet",
        "consectetur",
    ];
    let mut out = Vec::with_capacity(len + 32);
    while out.len() < len {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.extend_from_slice(words[(s % words.len() as u64) as usize].as_bytes());
        out.push(b' ');
        if s.is_multiple_of(97) {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out.truncate(len);
    out
}

/// 96 MiB in one file: over the 32 MiB floor below which splitting is refused, and big enough to
/// span several packs.
const BIG: usize = 96 << 20;

fn one_big_archive(dir: &Path) -> (PathBuf, Vec<u8>) {
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    let body = varied(BIG);
    fs::write(src.join("big.bin"), &body).unwrap();
    let arc = dir.join("big.cram");
    engine::create::create(
        &arc,
        cram_core::format::Format::cram(cram_core::format::Codec::None),
        &[src.join("big.bin")],
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();
    (arc, body)
}

#[test]
fn a_single_large_entry_is_offered_as_several_independent_pieces() {
    let dir = scratch("splits");
    let (arc, body) = one_big_archive(&dir);

    let reader = formats::open(
        &arc,
        cram_core::format::Format::cram(cram_core::format::Codec::None),
        Arc::new(NoPassword),
    )
    .unwrap();
    let ra = reader.as_random_access().expect(".cram is random access");
    let idx = ra
        .entries()
        .iter()
        .position(|e| e.size as usize == BIG)
        .expect("the big entry is in the archive");

    let splits = ra
        .entry_splits(idx)
        .expect("an entry this size spanning several packs must be splittable");
    assert!(
        splits.len() > 1,
        "a single range is the same as no split at all"
    );

    // The ranges must tile the entry exactly: contiguous, in order, covering every byte once. A gap
    // loses data and an overlap writes it twice, and both would survive the size check that follows
    // an extraction only if they cancelled out.
    let mut at = 0u64;
    for (off, len) in &splits {
        assert_eq!(*off, at, "ranges must be contiguous and in order");
        assert!(*len > 0, "an empty range is not a piece of work");
        at += len;
    }
    assert_eq!(at, body.len() as u64, "the ranges must cover the entry");

    // And each range must read back as exactly the bytes at that offset.
    for (off, len) in &splits {
        let got = ra.read_range(idx, *off, *len).unwrap();
        assert_eq!(got.len() as u64, *len, "short read for range at {off}");
        let want = &body[*off as usize..(*off + *len) as usize];
        assert!(got == want, "range at {off} decoded the wrong bytes");
    }

    let _ = fs::remove_dir_all(&dir);
}

/// The point of the whole exercise: extraction through the engine, which is what decides to split,
/// must produce the same file it always did.
#[test]
fn extracting_a_single_large_entry_is_byte_identical() {
    let dir = scratch("extract");
    let (arc, body) = one_big_archive(&dir);
    let out = dir.join("out");

    let report = engine::extract(
        &arc,
        &out,
        Arc::new(NoPassword),
        Default::default(),
        &NullSink,
    )
    .unwrap();
    assert!(report.failed.is_empty(), "failures: {:?}", report.failed);

    let got = fs::read(out.join("big.bin")).unwrap();
    assert_eq!(got.len(), body.len(), "extracted length differs");
    assert!(got == body, "extracted bytes differ from the original");

    let _ = fs::remove_dir_all(&dir);
}

/// Small entries must not be split: below the floor the scheduling costs more than the decode saves,
/// and an archive of many small files would otherwise nest a fan-out inside every one of them.
#[test]
fn a_small_entry_is_not_split() {
    let dir = scratch("small");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("small.bin"), varied(1 << 20)).unwrap();
    let arc = dir.join("small.cram");
    engine::create::create(
        &arc,
        cram_core::format::Format::cram(cram_core::format::Codec::None),
        &[src.join("small.bin")],
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();

    let reader = formats::open(
        &arc,
        cram_core::format::Format::cram(cram_core::format::Codec::None),
        Arc::new(NoPassword),
    )
    .unwrap();
    let ra = reader.as_random_access().unwrap();
    for i in 0..ra.entries().len() {
        assert!(
            ra.entry_splits(i).is_none(),
            "a 1 MiB entry is below the floor and must not be split"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}
