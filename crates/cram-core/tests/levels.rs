//! `--store` must actually store, and the expensive transform must not run when it wasn't asked for.
//!
//! Both of these were broken and neither was noticed, because both fail *quietly*: the archive is
//! correct, extraction round-trips, nothing errors. Only the clock and the byte count say anything
//! is wrong, and no test was watching either.
//!
//! `--store` arrives as `CreateOptions::codec = Some(Codec::None)`. For a container that wraps a
//! stream that means "no wrapper", and `.cram` does not wrap a stream -- it compresses per pack --
//! so nothing consulted the field at all. On a 5.15 GB mixed corpus `--store` ran for 53.52 s and
//! returned ratio 0.635, which is a full XZ pass wearing the name of the opposite instruction.
//!
//! Lossless JPEG recompression had the same shape of bug from the other direction: it ran at every
//! level including `--store`, costing roughly four times the create time for about 1.4% of the
//! archive on anything that isn't a photo library.

use std::fs;
use std::path::{Path, PathBuf};

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::writer::{CreateOptions, Level};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-levels-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Text that any codec flattens to nearly nothing, so "did it compress?" is unambiguous rather than
/// a judgement call about a few percent.
fn compressible_tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    let body = "the same line over and over again, which LZMA eats alive\n".repeat(40_000);
    for i in 0..4 {
        fs::write(src.join(format!("f{i}.txt")), &body).unwrap();
    }
    src
}

fn create(arc: &Path, src: &Path, opts: CreateOptions) -> u64 {
    engine::create::create(
        arc,
        Format::cram(Codec::None),
        std::slice::from_ref(&src.to_path_buf()),
        opts,
        &NullSink,
    )
    .unwrap_or_else(|e| panic!("create: {e}"));
    fs::metadata(arc).unwrap().len()
}

#[test]
fn store_level_does_not_compress() {
    let dir = scratch("store");
    let src = compressible_tree(&dir);
    let raw: u64 = fs::read_dir(&src)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();

    let packed = create(
        &dir.join("packed.cram"),
        &src,
        CreateOptions {
            level: Level::Balanced,
            ..Default::default()
        },
    );
    let stored = create(
        &dir.join("stored.cram"),
        &src,
        CreateOptions {
            level: Level::Balanced,
            codec: Some(Codec::None), // this is what `--store` sets
            ..Default::default()
        },
    );

    // Dedup still runs -- the four files are identical, so one copy is kept either way. What must
    // NOT happen is that copy also getting compressed.
    let one_copy = raw / 4;
    assert!(
        stored >= one_copy,
        "--store compressed anyway: {stored} bytes for {one_copy} bytes of unique input"
    );
    assert!(
        packed < one_copy / 4,
        "the compressing path should still compress: {packed} vs {one_copy}"
    );
}

/// Storing and extracting must still round-trip exactly, or the fix above traded one silent bug for
/// a much worse one.
#[test]
fn stored_archive_round_trips() {
    let dir = scratch("roundtrip");
    let src = compressible_tree(&dir);
    let arc = dir.join("out.cram");
    create(
        &arc,
        &src,
        CreateOptions {
            codec: Some(Codec::None),
            ..Default::default()
        },
    );

    let out = dir.join("x");
    fs::create_dir_all(&out).unwrap();
    let rep = engine::extract(
        &arc,
        &out,
        std::sync::Arc::new(cram_core::secret::NoPassword),
        Default::default(),
        &NullSink,
    )
    .expect("extract");
    assert!(rep.failed.is_empty(), "failures: {:?}", rep.failed);

    let original = fs::read(src.join("f0.txt")).unwrap();
    let restored = fs::read(out.join("src/f0.txt"))
        .or_else(|_| fs::read(out.join("f0.txt")))
        .expect("extracted file not found");
    assert_eq!(original, restored, "stored bytes must come back identical");
}

/// A JPEG is only recompressed when that was asked for. The writer patches the header to the
/// transform version the first time it stores one transformed, so the version byte is the witness.
#[test]
fn jpeg_is_only_transformed_when_requested() {
    const VERSION_PLAIN: u8 = 1;
    let dir = scratch("jpeg");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("photo.jpg"),
        include_bytes!("../tests/data/sample.jpg"),
    )
    .unwrap();

    for (tag, opts, want_transform) in [
        (
            "off",
            CreateOptions {
                recompress_images: false,
                ..Default::default()
            },
            false,
        ),
        (
            "store wins over an explicit yes",
            CreateOptions {
                recompress_images: true,
                codec: Some(Codec::None),
                ..Default::default()
            },
            false,
        ),
        (
            "on",
            CreateOptions {
                recompress_images: true,
                ..Default::default()
            },
            true,
        ),
    ] {
        let arc = dir.join(format!("{}.cram", tag.replace(' ', "-")));
        create(&arc, &src, opts);
        let head = fs::read(&arc).unwrap();
        let transformed = head[6] != VERSION_PLAIN;
        assert_eq!(
            transformed, want_transform,
            "{tag}: header version byte says transformed={transformed}, expected {want_transform}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// **`--cold` searches pre-filters, and what it finds has to decode.**
///
/// The level tries a BCJ or delta filter ahead of LZMA on every pack and keeps whichever came out
/// smallest. That is safe only because an xz block header records its own filter chain, so
/// `XzReader` puts it back without being told; nothing in the reader, the format or `cram-extract`
/// knows this level exists. If that ever stops being true the archive still lists, still tests, and
/// hands back mangled bytes, so the check that matters is byte-for-byte equality after a round trip.
///
/// The tree deliberately contains real machine code -- this test binary -- because the filters only
/// engage on the data they were built for, and a corpus of text would exercise none of them. On
/// Silesia the x86 filter took `ooffice` down 14.1% while making `mozilla` 0.9% larger, which is why
/// it is searched per pack rather than applied.
#[test]
fn cold_round_trips_through_its_filters() {
    use std::sync::Arc;
    let dir = scratch("cold");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();

    // Real executable bytes, so the BCJ candidates have something to match on.
    let exe = fs::read(std::env::current_exe().unwrap()).unwrap();
    fs::write(src.join("code.bin"), &exe[..exe.len().min(2_000_000)]).unwrap();
    fs::write(
        src.join("prose.txt"),
        "the quick brown fox jumps over the lazy dog, repeatedly and at length\n".repeat(3_000),
    )
    .unwrap();
    // Word-aligned numeric data, which is what the delta and lp/pb candidates are for.
    let mut table = Vec::new();
    for i in 0..80_000u32 {
        table.extend_from_slice(&(i.wrapping_mul(2_654_435_761)).to_le_bytes());
    }
    fs::write(src.join("table.bin"), &table).unwrap();

    let arc = dir.join("cold.cram");
    let cold = create(
        &arc,
        &src,
        CreateOptions {
            level: Level::Cold,
            ..Default::default()
        },
    );
    let raw: u64 = fs::read_dir(&src)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(cold < raw, "--cold produced {cold} bytes from {raw}");

    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    cram_core::engine::extract(
        &arc,
        &out,
        Arc::new(cram_core::secret::NoPassword),
        Default::default(),
        &cram_core::progress::NullSink,
    )
    .unwrap_or_else(|e| panic!("extract a --cold archive: {e}"));

    for name in ["code.bin", "prose.txt", "table.bin"] {
        assert_eq!(
            fs::read(src.join(name)).unwrap(),
            fs::read(out.join("src").join(name)).unwrap(),
            "{name} did not survive --cold; a pre-filter is not being reversed on read"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}
