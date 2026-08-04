//! Reproducibility guarantee for the native `.cram` writer: an **unencrypted** archive built from the
//! same logical inputs is **byte-for-byte identical**, regardless of where the sources live on disk,
//! how many times it is built, or how the parallel pack compressor batches the work. This is what lets
//! a `.cram` be content-addressed, cached, and verified against a published checksum.
//!
//! The format stores no timestamps and no absolute paths; the create walk sorts directory children;
//! chunking (FastCDC), dedup (BLAKE3), pack assembly, and the order-preserving parallel compression
//! are all deterministic, so determinism is a property of the design, and these tests pin it down.
//!
//! Encrypted archives are intentionally NOT reproducible: each carries a fresh random Argon2 salt and
//! per-blob AES-GCM nonces (reusing a GCM nonce would be catastrophic), so two encrypted builds of the
//! same input MUST differ. The final test asserts that too, so "reproducible" is never misread as
//! "encryption is deterministic."

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::{EncryptSpec, Secret};
use cram_core::writer::CreateOptions;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn scratch(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("cram-repro-{}-{}-{}", std::process::id(), tag, n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a fixed logical tree under `root/proj`. The same bytes and layout every time. Includes
/// nested dirs, a file duplicated across two paths (exercises cross-file dedup), and (when `extra` is
/// set) a few small distinct files so the index carries a mix of new and deduped chunks.
///
/// This fixture is small, well under one ~8 MiB pack, so it does NOT exercise the
/// multi-pack parallel-compression path (that is covered separately by
/// [`unencrypted_multipack_build_is_byte_identical`]). It pins down the size-independent parts of
/// determinism (sorted walk, chunking, dedup, single-pack compression, index, trailer, path
/// independence) while staying fast even in a debug build, where the pure-Rust XZ compressor is
/// ~15× slower than release.
fn build_tree(root: &Path, extra: bool) -> PathBuf {
    let proj = root.join("proj");
    fs::create_dir_all(proj.join("src/inner")).unwrap();
    fs::create_dir_all(proj.join("assets")).unwrap();

    fs::write(proj.join("README.md"), b"# proj\nreproducible build test\n").unwrap();

    // Two byte-identical files at different paths → must dedup to the same chunks.
    let shared = b"the same content in two places ".repeat(5000);
    fs::write(proj.join("src/a.txt"), &shared).unwrap();
    fs::write(proj.join("assets/copy.txt"), &shared).unwrap();

    if extra {
        // Distinct, low-dedup data across several files → multiple chunks and (with the shared file)
        // a mix of new and deduped chunks, all serialized into the index in a deterministic order.
        for f in 0..3u32 {
            let mut v = String::with_capacity(90_000);
            let mut i = 0u64;
            while v.len() < 90_000 {
                v.push_str(&format!("blob {f} row {i} :: reproducible-build-fixture\n"));
                i += 1;
            }
            fs::write(
                proj.join("src/inner").join(format!("blob{f}.txt")),
                v.as_bytes(),
            )
            .unwrap();
        }
    }

    proj
}

fn create_cram(proj: &Path, out: &Path, opts: CreateOptions) {
    engine::create::create(
        out,
        Format::cram(Codec::None),
        &[proj.to_path_buf()],
        opts,
        &NullSink,
    )
    .expect("create .cram");
}

#[test]
fn unencrypted_cram_is_byte_identical_across_paths_and_runs() {
    // Same logical tree in two DIFFERENT parent directories (different absolute paths), each rooted
    // at "proj". A reproducible archive must not leak the absolute source path or the on-disk order.
    let dir_a = scratch("a");
    let dir_b = scratch("b");
    let proj_a = build_tree(&dir_a, true);
    let proj_b = build_tree(&dir_b, true);

    let arc_a1 = dir_a.join("a1.cram");
    let arc_a2 = dir_a.join("a2.cram"); // rebuild from the SAME sources
    let arc_b1 = dir_b.join("b1.cram");
    create_cram(&proj_a, &arc_a1, CreateOptions::default());
    create_cram(&proj_a, &arc_a2, CreateOptions::default());
    create_cram(&proj_b, &arc_b1, CreateOptions::default());

    let a1 = fs::read(&arc_a1).unwrap();
    let a2 = fs::read(&arc_a2).unwrap();
    let b1 = fs::read(&arc_b1).unwrap();

    assert!(!a1.is_empty(), "archive is non-empty");
    assert_eq!(
        a1, a2,
        "rebuilding from identical sources is byte-identical"
    );
    assert_eq!(
        a1, b1,
        "the archive is independent of the absolute source path"
    );

    let _ = fs::remove_dir_all(&dir_a);
    let _ = fs::remove_dir_all(&dir_b);
}

/// exercises the MULTI-PACK parallel-compression path: 36 MB of INCOMPRESSIBLE data so the writer
/// crosses the raw pack boundary several times and `flush_batch` runs on multi-element batches. Two
/// builds must be byte-identical; this is what would catch a regression making pack write-order
/// depend on thread/batch completion order.
///
/// The pack count is asserted **directly**, from the reader, rather than inferred from the archive
/// being bigger than some multiple of the pack size. That proxy was silently invalidated the moment
/// the default pack target moved from 8 to 16 MiB: an 18 MB fixture went from three packs to two,
/// and the `> 12 MiB` size check it rested on would have passed just as happily on a single 16 MiB
/// pack. A test that names a property should assert that property.
///
/// Marked `#[ignore]` for the cost of generating and chunking 36 MB twice, which is slow in a debug
/// build, not for the compressor: the fixture is incompressible, so the probe stores each pack raw
/// and XZ is never entered. Run it with `cargo test -- --ignored`.
#[test]
#[ignore = "heavy: 36 MB fixture; run with --ignored"]
fn unencrypted_multipack_build_is_byte_identical() {
    let dir = scratch("mp");
    let proj = dir.join("proj");
    fs::create_dir_all(proj.join("d")).unwrap();
    for f in 0..6u32 {
        // Deterministic xorshift byte stream, incompressible, so each pack is stored raw and the
        // archive is ~= the input size (a genuine multi-pack layout, not a heavily-compressed blob).
        let mut v = Vec::with_capacity(6_000_000);
        let mut x = 0x9E37_79B9u32 ^ f.wrapping_mul(0x0100_0193);
        for _ in 0..6_000_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push((x >> 24) as u8);
        }
        fs::write(proj.join("d").join(format!("part{f}.bin")), &v).unwrap();
    }

    let a = dir.join("mp_a.cram");
    let b = dir.join("mp_b.cram");
    create_cram(&proj, &a, CreateOptions::default());
    create_cram(&proj, &b, CreateOptions::default());
    let (ba, bb) = (fs::read(&a).unwrap(), fs::read(&b).unwrap());
    // Ask the archive how many packs it has rather than guessing from its size.
    let reader = cram_core::formats::open(
        &a,
        Format::cram(Codec::None),
        std::sync::Arc::new(cram_core::secret::NoPassword),
    )
    .unwrap();
    let packs = reader
        .as_random_access()
        .expect("cram is random-access")
        .decode_units()
        .expect("cram reports its pack count");
    drop(reader);
    assert!(
        packs >= 2,
        "fixture must span several packs to exercise multi-element batches, got {packs}"
    );
    assert_eq!(
        ba, bb,
        "multi-pack builds must be byte-identical (order-preserving parallel compression)"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn encrypted_cram_is_intentionally_not_reproducible() {
    // Encryption must inject randomness (a fresh salt + nonces), so two encrypted builds of the same
    // input differ. If these ever became equal, the salt/nonce would be static; a security defect.
    let dir = scratch("enc");
    let proj = build_tree(&dir, false);
    let opts = || CreateOptions {
        encrypt: Some(EncryptSpec::new(Secret::new("repro-pw"))),
        ..Default::default()
    };
    let e1 = dir.join("e1.cram");
    let e2 = dir.join("e2.cram");
    create_cram(&proj, &e1, opts());
    create_cram(&proj, &e2, opts());

    let b1 = fs::read(&e1).unwrap();
    let b2 = fs::read(&e2).unwrap();
    assert_ne!(
        b1, b2,
        "encrypted archives must differ (random salt + GCM nonces)"
    );
    // Same overall size, though; only the random/keyed bytes differ, not the structure.
    assert_eq!(
        b1.len(),
        b2.len(),
        "only the random material differs, not the layout size"
    );

    let _ = fs::remove_dir_all(&dir);
}
