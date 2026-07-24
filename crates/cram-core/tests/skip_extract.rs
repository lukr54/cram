//! skip-already-correct: a second extract over a matching tree skips every entry (size + CRC),
//! while a destination file that was changed is re-extracted.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::Format;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;
use cram_core::ExtractOptions;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-skip-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_zip(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("data/sub")).unwrap();
    fs::write(dir.join("data/a.txt"), b"alpha ".repeat(400)).unwrap();
    fs::write(
        dir.join("data/blob.bin"),
        (0..30_000u32).map(|i| i as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(dir.join("data/sub/b.txt"), b"bravo ".repeat(300)).unwrap();
    let archive = dir.join("out.zip");
    engine::create::create(
        &archive,
        Format::zip(),
        &[dir.join("data")],
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create");
    archive
}

const SKIP: ExtractOptions = ExtractOptions {
    skip_existing: true,
};

#[test]
fn second_extract_skips_everything() {
    let dir = scratch("all");
    let archive = make_zip(&dir);
    let out = dir.join("out");

    // First extract writes all 3 files.
    let r1 = engine::extract(&archive, &out, Arc::new(NoPassword), SKIP, &NullSink).unwrap();
    assert_eq!(r1.extracted, 3);
    assert_eq!(r1.skipped, 0);

    // Second extract over the identical tree skips all 3 (size + CRC match).
    let r2 = engine::extract(&archive, &out, Arc::new(NoPassword), SKIP, &NullSink).unwrap();
    assert_eq!(r2.extracted, 0, "nothing should be re-written");
    assert_eq!(r2.skipped, 3, "all entries already correct");
    assert!(r2.failed.is_empty());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn changed_file_is_reextracted() {
    let dir = scratch("changed");
    let archive = make_zip(&dir);
    let out = dir.join("out");

    engine::extract(&archive, &out, Arc::new(NoPassword), SKIP, &NullSink).unwrap();

    // Corrupt one extracted file (same length so only the CRC differs, proves we check content,
    // not just size).
    let victim = out.join("data/a.txt");
    let mut bytes = fs::read(&victim).unwrap();
    bytes[0] ^= 0xFF;
    fs::write(&victim, &bytes).unwrap();

    let r = engine::extract(&archive, &out, Arc::new(NoPassword), SKIP, &NullSink).unwrap();
    assert_eq!(r.extracted, 1, "only the changed file is rewritten");
    assert_eq!(r.skipped, 2, "the two untouched files are skipped");

    // The victim is restored to the archived content.
    assert_eq!(fs::read(&victim).unwrap(), b"alpha ".repeat(400));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn without_skip_everything_is_rewritten() {
    let dir = scratch("noskip");
    let archive = make_zip(&dir);
    let out = dir.join("out");

    engine::extract(
        &archive,
        &out,
        Arc::new(NoPassword),
        ExtractOptions::default(),
        &NullSink,
    )
    .unwrap();
    // Default options (skip off) → the second pass rewrites all 3, skips none.
    let r = engine::extract(
        &archive,
        &out,
        Arc::new(NoPassword),
        ExtractOptions::default(),
        &NullSink,
    )
    .unwrap();
    assert_eq!(r.extracted, 3);
    assert_eq!(r.skipped, 0);

    let _ = fs::remove_dir_all(&dir);
}
