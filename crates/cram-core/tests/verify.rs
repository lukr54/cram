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
