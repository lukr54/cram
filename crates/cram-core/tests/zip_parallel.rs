//! The parallel ZIP create path must be invisible in the output.
//!
//! Entries are compressed across the rayon pool and appended in submission order, so nothing about
//! the archive should depend on how the work was divided. These tests pin that down three ways:
//! repeated builds match, a forced-sequential build matches, and a build that pushes every entry
//! down the oversized-entry fallback matches too. Encrypted archives are outside them: an AES salt
//! is random, so two builds never match.
//!
//! The fallback case is here because it regressed once during development. Turning `takes_paths`
//! on stopped the engine probing store-vs-compress inline; the parallel path picked the probe up
//! but the oversized fallback did not, so a large incompressible entry was DEFLATEd where it had
//! been STOREd. Nothing failed — the archive was simply 15 KB bigger and different from what the
//! sequential writer produced.
//!
//! `CRAM_ZIP_SEQUENTIAL` and `CRAM_ZIP_ENTRY_MAX` are process-wide, and each integration test file
//! is its own binary, so the env-var work is kept inside a single test to avoid racing the others
//! in this file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::Format;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn scratch(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("cram-zippar-{}-{}-{}", std::process::id(), tag, n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A tree that exercises both sides of the adaptive probe: text that DEFLATEs well, and
/// pseudo-random bytes that must be detected as incompressible and STOREd. Nested directories are
/// included because they share the pending queue with files and must keep their place in it.
fn build_tree(root: &Path) -> PathBuf {
    let proj = root.join("proj");
    fs::create_dir_all(proj.join("src/inner")).unwrap();
    fs::create_dir_all(proj.join("blobs")).unwrap();

    fs::write(proj.join("README.md"), b"# zip parallel\n".repeat(200)).unwrap();
    // An empty file and a non-ASCII name. The parallel path hands the zip crate a prepared file
    // where the sequential writer streams into it, two routes through the crate that have to write
    // the same headers. These are the entries where they could part ways with no data to show it:
    // an empty DEFLATE stream, and the flag that marks a UTF-8 name.
    fs::write(proj.join("empty.txt"), b"").unwrap();
    fs::write(
        proj.join("src/caf\u{e9}-\u{65e5}\u{672c}.txt"),
        b"non-ASCII name\n".repeat(50),
    )
    .unwrap();
    for i in 0..12u32 {
        let text = format!("line {i} of a very compressible file\n").repeat(300);
        fs::write(proj.join(format!("src/f{i}.txt")), text).unwrap();
        fs::write(
            proj.join(format!("src/inner/g{i}.txt")),
            b"nested\n".repeat(500),
        )
        .unwrap();
    }

    // xorshift32: incompressible enough for the probe to call STORE, and identical every run so
    // the archives can be compared byte for byte.
    let mut state = 0x1234_5678u32;
    for i in 0..4u32 {
        let mut blob = Vec::with_capacity(300_000);
        while blob.len() < 300_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            blob.extend_from_slice(&state.to_le_bytes());
        }
        fs::write(proj.join(format!("blobs/b{i}.bin")), &blob).unwrap();
    }
    proj
}

fn make_zip(src: &Path, out: &Path) {
    engine::create::create(
        out,
        Format::zip(),
        &[src.to_path_buf()],
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();
}

#[test]
fn parallel_zip_matches_sequential_and_itself() {
    let dir = scratch("equiv");
    let src = build_tree(&dir);

    let a = dir.join("a.zip");
    let b = dir.join("b.zip");
    make_zip(&src, &a);
    make_zip(&src, &b);
    let bytes_a = fs::read(&a).unwrap();
    assert_eq!(
        bytes_a,
        fs::read(&b).unwrap(),
        "two parallel builds of the same tree differ"
    );
    assert!(
        bytes_a.len() > 1000,
        "fixture produced a suspiciously tiny archive"
    );

    // The two entries with no data to tell them apart have to be in the archive, under their own
    // names. Three byte-equal archives would not notice if every route dropped or mangled them
    // alike.
    let out = dir.join("extracted");
    let rep = engine::extract(
        &a,
        &out,
        Arc::new(NoPassword),
        Default::default(),
        &NullSink,
    )
    .expect("extract");
    assert!(
        rep.failed.is_empty(),
        "unexpected failures: {:?}",
        rep.failed
    );
    assert_eq!(
        fs::read(out.join("proj/empty.txt")).expect("empty.txt was extracted"),
        b"",
        "empty.txt"
    );
    assert_eq!(
        fs::read(out.join("proj/src/caf\u{e9}-\u{65e5}\u{672c}.txt"))
            .expect("the file with a non-ASCII name was extracted"),
        b"non-ASCII name\n".repeat(50),
        "the file with a non-ASCII name"
    );

    // Forced sequential: the pre-parallel writer, which must agree byte for byte.
    let seq = dir.join("seq.zip");
    std::env::set_var("CRAM_ZIP_SEQUENTIAL", "1");
    make_zip(&src, &seq);
    std::env::remove_var("CRAM_ZIP_SEQUENTIAL");
    assert_eq!(
        bytes_a,
        fs::read(&seq).unwrap(),
        "parallel build differs from the sequential writer"
    );

    // Every entry over the buffering threshold, so all of them take the oversized fallback. This is
    // the path that lost the adaptive-store probe.
    let fallback = dir.join("fallback.zip");
    std::env::set_var("CRAM_ZIP_ENTRY_MAX", "1");
    make_zip(&src, &fallback);
    std::env::remove_var("CRAM_ZIP_ENTRY_MAX");
    assert_eq!(
        bytes_a,
        fs::read(&fallback).unwrap(),
        "oversized-entry fallback differs from the parallel path"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The incompressible fixtures must actually be reaching STORE, or the test above would be
/// comparing three archives that all made the same wrong decision.
#[test]
fn incompressible_entries_are_stored() {
    let dir = scratch("stored");
    let src = build_tree(&dir);
    let out = dir.join("s.zip");

    let report = engine::create::create(
        &out,
        Format::zip(),
        std::slice::from_ref(&src),
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();

    assert_eq!(
        report.stored, 4,
        "expected the 4 pseudo-random blobs to be stored, got {}",
        report.stored
    );
    let _ = fs::remove_dir_all(&dir);
}
