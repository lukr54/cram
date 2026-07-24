//! The adaptive `Level::Auto` probe. Proves that already-compressed files are stored
//! verbatim (not run through the codec) while genuinely compressible files are compressed, on both
//! random-access backends (ZIP and 7z), and that either archive still round-trips byte-for-byte.
//!
//! ZIP verification is exact: cram's reader reports each entry's `compressed_size`, so a STORED
//! entry reads back `compressed_size == size` and a DEFLATED entry reads back strictly smaller.
//! The 7z reader doesn't expose a per-entry compressed size (it assumes solid blocks), so there we
//! assert the writer's own `stored` count plus a full round-trip (external COPY-vs-LZMA2 proof is
//! done with the real 7-Zip CLI, outside the test suite).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::format::Format;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;
use cram_core::{engine, formats};

/// A fresh scratch dir under the OS temp dir, wiped if it somehow already exists.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-auto-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Deterministic high-entropy bytes (xorshift) — genuinely incompressible, no RNG dep.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Three files: compressible text, an incompressible blob detected by EXTENSION (`.jpg`), and an
/// incompressible blob detected by CONTENT SAMPLE (unknown `.dat`). Returns (name, bytes) each.
fn make_sources(root: &Path) -> Vec<(String, Vec<u8>)> {
    let text = b"the quick brown fox jumps over the lazy dog\n".repeat(400);
    let by_ext = noise(48 * 1024); // .jpg → store via extension
    let by_sample = noise(48 * 1024); // .dat → store via content sample (high entropy)

    fs::write(root.join("readme.txt"), &text).unwrap();
    fs::write(root.join("photo.jpg"), &by_ext).unwrap();
    fs::write(root.join("random.dat"), &by_sample).unwrap();

    vec![
        ("readme.txt".into(), text),
        ("photo.jpg".into(), by_ext),
        ("random.dat".into(), by_sample),
    ]
}

fn inputs(root: &Path, sources: &[(String, Vec<u8>)]) -> Vec<PathBuf> {
    sources.iter().map(|(n, _)| root.join(n)).collect()
}

fn assert_round_trips(archive: &Path, dir: &Path, sources: &[(String, Vec<u8>)]) {
    let out = dir.join("extracted");
    let rep = engine::extract(
        archive,
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
    for (name, content) in sources {
        let got = fs::read(out.join(name)).unwrap_or_else(|e| panic!("missing {name}: {e}"));
        assert_eq!(&got, content, "content mismatch for {name}");
    }
}

#[test]
fn zip_auto_stores_incompressible_per_entry() {
    let dir = scratch("zip");
    let sources = make_sources(&dir);
    let archive = dir.join("out.zip");

    // Default options == Level::Auto, no forced codec → the probe runs.
    let report = engine::create::create(
        &archive,
        Format::zip(),
        &inputs(&dir, &sources),
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create");

    // The two incompressible files (one by extension, one by sample) were stored; text was not.
    assert_eq!(
        report.stored, 2,
        "both incompressible entries should be auto-stored"
    );

    // Exact per-entry method check via cram's own reader: STORED ⇒ compressed_size == size.
    let reader = formats::open(&archive, Format::zip(), Arc::new(NoPassword)).unwrap();
    for e in reader.entries().unwrap() {
        let csize = e.compressed_size.expect("zip reports compressed size");
        match e.name() {
            "readme.txt" => assert!(
                csize < e.size,
                "text must be DEFLATED (compressed {csize} < {})",
                e.size
            ),
            "photo.jpg" | "random.dat" => assert_eq!(
                csize,
                e.size,
                "{} must be STORED (compressed == uncompressed)",
                e.name()
            ),
            other => panic!("unexpected entry {other}"),
        }
    }
    drop(reader);

    assert_round_trips(&archive, &dir, &sources);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sevenz_auto_stores_incompressible_per_entry() {
    let dir = scratch("7z");
    let sources = make_sources(&dir);
    let archive = dir.join("out.7z");

    let report = engine::create::create(
        &archive,
        Format::sevenz(),
        &inputs(&dir, &sources),
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create");

    // The 7z writer swapped in a COPY chain for the two incompressible entries (per-entry, via
    // set_content_methods between pushes); LZMA2 for the text.
    assert_eq!(
        report.stored, 2,
        "both incompressible entries should be auto-stored"
    );

    // Heterogeneous COPY/LZMA2 folders in one 7z must still extract byte-for-byte.
    assert_round_trips(&archive, &dir, &sources);
    let _ = fs::remove_dir_all(&dir);
}
