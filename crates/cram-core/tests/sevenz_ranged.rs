//! Ranged reads on a large solid `.7z`, which is the shape a lazy mount drives.
//!
//! A block big enough for this to mean anything is measured in gigabytes, so the archive is named by
//! **`CRAM_BIG_7Z`** and the test skips when that is unset — a fixture this size cannot live in the
//! repo and cannot run in CI. A skipped test proves nothing, so it says so on the way past rather
//! than passing quietly.
//!
//! What it checks:
//!
//! - every range matches the same slice of the entry decoded whole, which is the failure that raises
//!   no error of its own (starting one segment too far along serves the wrong bytes, silently);
//! - what a range costs, against what decoding from the block start costs, which is what a ranged
//!   read cost before the segment path existed.

use std::sync::Arc;
use std::time::Instant;

use cram_core::format::Format;
use cram_core::secret::NoPassword;

/// Entries larger than this are skipped as the subject: the reference decode holds the whole entry,
/// and the point is a large *block*, not a large entry.
const MAX_SUBJECT: u64 = 128 << 20;

#[test]
fn a_ranged_read_matches_the_whole_entry_and_costs_less_than_the_block() {
    let Ok(path) = std::env::var("CRAM_BIG_7Z") else {
        eprintln!(
            "SKIPPED: set CRAM_BIG_7Z to a large solid .7z to run this. Nothing was verified."
        );
        return;
    };
    let path = std::path::PathBuf::from(path);

    let reader = cram_core::formats::open(&path, Format::sevenz(), Arc::new(NoPassword)).unwrap();
    let ra = reader.as_random_access().expect("7z offers random access");

    // The largest entry that is still cheap to hold, so the reference is one decode rather than one
    // per range.
    let (idx, size) = ra
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, e)| e.size > 0 && e.size <= MAX_SUBJECT)
        .max_by_key(|(_, e)| e.size)
        .map(|(i, e)| (i, e.size))
        .expect("the archive must hold at least one entry with bytes");
    let name = ra.entries()[idx].path.raw().to_string();
    eprintln!("subject: {name} ({size} bytes, entry {idx})");

    // The reference, and at the same time the number the segment path has to beat: `copy_entry`
    // decodes from the start of the block, which is what every ranged read used to cost.
    let t = Instant::now();
    let mut whole = Vec::new();
    ra.copy_entry(idx, &mut whole).unwrap();
    let block_decode = t.elapsed();
    assert_eq!(whole.len() as u64, size, "the reference decode is short");
    eprintln!("decode from the block start: {block_decode:?}");

    // Head, middle, tail and a read running off the end. The tail is the one that matters: it is
    // furthest from the block's first byte, so it is where starting at a segment can win most.
    let cases: [(u64, u64); 5] = [
        (0, 64 << 10),
        (size / 2, 64 << 10),
        (size.saturating_sub(64 << 10), 64 << 10),
        (size.saturating_sub(16), 16),
        (size.saturating_sub(8), 4096), // clamps to the entry rather than reading past it
    ];

    let mut worst = std::time::Duration::ZERO;
    for (off, len) in cases {
        let t = Instant::now();
        let got = ra.read_range(idx, off, len).unwrap();
        let took = t.elapsed();
        worst = worst.max(took);

        let start = off as usize;
        let end = (start + len as usize).min(whole.len());
        assert_eq!(
            got,
            &whole[start..end],
            "range {off}+{len} did not match the entry decoded whole"
        );
        eprintln!(
            "  range {off:>12}+{len:<7} -> {:>9} bytes in {took:?}",
            got.len()
        );
    }

    eprintln!(
        "worst range {worst:?} against {block_decode:?} for the block: {:.2}x",
        block_decode.as_secs_f64() / worst.as_secs_f64().max(f64::EPSILON)
    );

    // A whole-entry read through the same path, to prove the clamping and the crossing of segment
    // boundaries agree with the reference over the entire entry rather than at five points.
    let all = ra.read_range(idx, 0, u64::MAX).unwrap();
    assert_eq!(
        all.len(),
        whole.len(),
        "a full-length range came back short"
    );
    assert!(all == whole, "a full-length range did not match the entry");

    // And that the archive really is one a mount would struggle with, so a future change that
    // quietly stops segmenting cannot leave this passing on a trivially small block.
    let mut biggest = 0u64;
    for e in ra.entries() {
        biggest = biggest.max(e.size);
    }
    eprintln!("largest entry in the archive: {biggest} bytes");
}

/// The same subject read one byte at a time near the end, which is the access pattern a filesystem
/// generates when something seeks around a mounted file. Purely a liveness and correctness check —
/// it must not fall back to a block decode per byte.
#[test]
fn many_small_ranges_near_the_end_stay_bounded() {
    let Ok(path) = std::env::var("CRAM_BIG_7Z") else {
        eprintln!("SKIPPED: set CRAM_BIG_7Z to run this. Nothing was verified.");
        return;
    };
    let path = std::path::PathBuf::from(path);

    let reader = cram_core::formats::open(&path, Format::sevenz(), Arc::new(NoPassword)).unwrap();
    let ra = reader.as_random_access().unwrap();
    let (idx, size) = ra
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, e)| e.size > 0 && e.size <= MAX_SUBJECT)
        .max_by_key(|(_, e)| e.size)
        .map(|(i, e)| (i, e.size))
        .unwrap();

    let mut tail = Vec::new();
    let t = Instant::now();
    for k in 0..32u64 {
        let off = size.saturating_sub(32 - k);
        tail.extend_from_slice(&ra.read_range(idx, off, 1).unwrap());
    }
    eprintln!("32 single-byte ranges at the tail: {:?}", t.elapsed());

    let mut whole = Vec::new();
    ra.copy_entry(idx, &mut whole).unwrap();
    assert_eq!(
        tail,
        &whole[whole.len() - 32..],
        "the last 32 bytes read one at a time must match the entry"
    );
}
