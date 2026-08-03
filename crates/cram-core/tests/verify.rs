//! `cram test` (integrity verification without extraction). Proves the happy path across
//! formats AND that a real single-byte corruption of stored data is actually caught, the point
//! of the feature (a verify that always says "ok" would be worse than none).

use std::path::PathBuf;
use std::sync::Arc;

use cram_core::engine::{self, verify};
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::{CreateOptions, Level};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-verify-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/d")).unwrap();
    dir
}

fn build_src(dir: &std::path::Path) -> PathBuf {
    let src = dir.join("src/d");
    std::fs::write(src.join("a.txt"), b"verify-corruption-canary ".repeat(200)).unwrap();
    std::fs::write(
        src.join("b.bin"),
        (0..4000u32).map(|i| i as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    std::fs::write(src.join("empty.txt"), b"").unwrap();
    dir.join("src/d")
}

#[test]
fn verify_passes_clean_archives_across_formats() {
    let dir = scratch("clean");
    let src = build_src(&dir);

    for (name, fmt) in [
        ("out.zip", Format::zip()),
        ("out.tar", Format::tar(Codec::None)),
        ("out.7z", Format::sevenz()),
        ("out.cram", Format::cram(Codec::None)),
    ] {
        let arc = dir.join(name);
        engine::create::create(
            &arc,
            fmt,
            std::slice::from_ref(&src),
            CreateOptions::default(),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("create {name}: {e}"));

        let rep = verify::verify(&arc, Arc::new(NoPassword), &NullSink)
            .unwrap_or_else(|e| panic!("verify {name}: {e}"));
        assert!(
            rep.ok(),
            "{name}: expected clean, got failures {:?}",
            rep.failures
        );
        // Two non-empty files + one empty file = 3 checked entries.
        assert_eq!(rep.checked, 3, "{name}: checked count");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `.cram` must report its **pack count** as the number of independently decodable units, because
/// that number is what `hw::derive_plan` fans out over.
///
/// It used to report one, from a `block_count` arm that had no rule for the container. One decode
/// unit makes the CPU-bound plan `min(1, cores)`, so `cram t` verified a 1.6 GB archive on a single
/// thread of a 24-thread machine (18.1 s at 96% of one core, against 7-Zip's 1.55 s at 354%).
/// Extraction never showed it because a write-bound plan takes a different branch.
///
/// The archive here is deliberately larger than one 8 MiB pack, and filled with a non-repeating
/// sequence so neither chunk dedup nor compression can collapse it back into a single pack.
#[test]
fn cram_reports_its_pack_count_as_decode_units() {
    let dir = scratch("units");
    let src = dir.join("src/d");
    // ~24 MiB of unique bytes from a cheap LCG: dedup finds no repeated chunk and STORE keeps it
    // all, so this must land in at least three packs.
    let mut state: u32 = 0x1234_5678;
    let blob: Vec<u8> = (0..24 * 1024 * 1024)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    std::fs::write(src.join("big.bin"), &blob).unwrap();

    let arc = dir.join("packs.cram");
    engine::create::create(
        &arc,
        Format::cram(Codec::None),
        std::slice::from_ref(&src),
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();

    let reader =
        cram_core::formats::open(&arc, Format::cram(Codec::None), Arc::new(NoPassword)).unwrap();
    let ra = reader.as_random_access().expect("cram is random-access");
    let units = ra.decode_units().expect("cram knows its pack count");
    assert!(
        units > 1,
        "a 24 MiB archive spans several 8 MiB packs, got {units} decode unit(s); \
         reporting 1 here is what pinned verify to a single thread"
    );
    // And a file entry really does name the pack it lives in, which is what clusters the work.
    // Directory entries hold no chunks and correctly report no key, so ask a file.
    let first_file = ra
        .entries()
        .iter()
        .position(|e| !e.is_dir())
        .expect("the archive holds a file");
    assert!(
        ra.locality_key(first_file).is_some(),
        "a cram file entry must name the pack it lives in"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What `cram t` prints must not depend on which worker finished first. Failures are collected off
/// a rayon pool in completion order and sorted back into archive order before they are reported, so
/// the same damaged archive gives the same list every run.
///
/// Sizes here are chosen so the pool's largest-first ordering does **not** match archive order: the
/// last entry is the heaviest and gets scheduled first.
#[test]
fn parallel_verify_reports_failures_in_archive_order() {
    let dir = scratch("order");
    let src = dir.join("src/d");
    let sizes = [500usize, 400, 300, 200, 100, 900];
    for (i, &n) in sizes.iter().enumerate() {
        // Each file carries its own marker so a single byte of it can be flipped by name.
        let mut body = format!("MARK{i}-").into_bytes();
        body.resize(n, b'.');
        std::fs::write(src.join(format!("f{i}.txt")), &body).unwrap();
    }

    let arc = dir.join("order.zip");
    let opts = CreateOptions {
        level: Level::Explicit(0),
        codec: Some(Codec::None), // STORE, so content bytes sit verbatim in the archive
        ..Default::default()
    };
    engine::create::create(&arc, Format::zip(), &[src], opts, &NullSink).unwrap();

    // Damage three entries, including the one the scheduler runs first.
    let mut bytes = std::fs::read(&arc).unwrap();
    for i in [0usize, 3, 5] {
        let needle = format!("MARK{i}-").into_bytes();
        let pos = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap_or_else(|| panic!("marker for f{i} present in stored data"));
        bytes[pos] ^= 0xFF;
    }
    std::fs::write(&arc, &bytes).unwrap();

    let rep = verify::verify(&arc, Arc::new(NoPassword), &NullSink).unwrap();
    let names: Vec<&str> = rep.failures.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.len(), 3, "three damaged entries: {:?}", rep.failures);
    assert!(
        names[0].ends_with("f0.txt")
            && names[1].ends_with("f3.txt")
            && names[2].ends_with("f5.txt"),
        "failures must be reported in archive order, got {names:?}"
    );
    assert_eq!(rep.checked, 3, "the three intact entries still verify");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_catches_single_byte_corruption_of_stored_data() {
    let dir = scratch("corrupt");
    let src = build_src(&dir);

    // A STORE zip keeps file bytes verbatim, so we can flip one content byte and expect a CRC
    // mismatch (the central directory, and thus archive open, is untouched).
    let arc = dir.join("store.zip");
    let opts = CreateOptions {
        level: Level::Explicit(0),
        codec: Some(Codec::None), // STORE
        ..Default::default()
    };
    engine::create::create(&arc, Format::zip(), &[src], opts, &NullSink).unwrap();

    // Corrupt the first byte of the stored "canary" run (present only in a.txt's data).
    let mut bytes = std::fs::read(&arc).unwrap();
    let needle = b"canary";
    let pos = bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("canary marker present in stored data");
    bytes[pos] ^= 0xFF;
    std::fs::write(&arc, &bytes).unwrap();

    let rep = verify::verify(&arc, Arc::new(NoPassword), &NullSink).unwrap();
    assert!(!rep.ok(), "corruption must be detected");
    assert_eq!(
        rep.failures.len(),
        1,
        "exactly a.txt should fail: {:?}",
        rep.failures
    );
    assert!(
        rep.failures[0].0.contains("a.txt"),
        "the failing entry should be a.txt, got {:?}",
        rep.failures
    );
    // `checked` counts only entries that PASSED (b.bin + empty.txt = 2), so `checked + failures`
    // equals the 3 files examined, no double-counting of the failed entry (the reported total is
    // what the CLI prints as "N of TOTAL entries bad").
    assert_eq!(
        rep.checked, 2,
        "only the two intact files should be counted verified"
    );
    assert_eq!(
        rep.checked as usize + rep.failures.len(),
        3,
        "checked + failures must equal the number of files examined"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
