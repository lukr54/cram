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

/// Varied text and a word-aligned table, small enough to stay one pack at every level, so the only
/// thing that separates two archives of it is how that one pack was compressed.
fn mixed_tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    let mut prose = String::new();
    for i in 0..4_000u32 {
        prose.push_str(&format!(
            "entry {i}: the quick brown fox number {} jumps over {} lazy dogs near gate {}\n",
            i.wrapping_mul(7_919) % 1_000,
            i % 37,
            i.wrapping_mul(31) % 211
        ));
    }
    fs::write(src.join("prose.txt"), prose).unwrap();
    let mut table = Vec::new();
    for i in 0..20_000u32 {
        table.extend_from_slice(&(i.wrapping_mul(2_654_435_761) >> 12).to_le_bytes());
    }
    fs::write(src.join("table.bin"), &table).unwrap();
    src
}

/// **`--tiny` on a `.cram` is `--small`, byte for byte.**
///
/// `.cram` has no slower encoder for `--tiny` to reach for, so the level is documented to behave as
/// `--small` there. It did not: the pack policy gave the filter search to `Level::Cold` alone, and
/// `Level::Tiny` fell through to the single-attempt path. On a `zstd-c` build, which is what the
/// releases are, that wrote `--tiny` packs as zstd with no XZ candidate at all, and the archive came
/// out larger than `--small`'s.
///
/// The control is `Explicit(9)`: the single-attempt path at the same preset, which is the path
/// `--tiny` used to take. It has to differ from `--small` on this input, or the equality below could
/// hold with the bug still in place. Runs on every feature set; on `zstd-c` the control is zstd.
#[test]
fn tiny_cram_is_byte_identical_to_cold() {
    let dir = scratch("tiny");
    let src = mixed_tree(&dir);
    let build = |level: Level, name: &str| {
        let arc = dir.join(name);
        create(
            &arc,
            &src,
            CreateOptions {
                level,
                ..Default::default()
            },
        );
        fs::read(&arc).unwrap()
    };
    let cold = build(Level::Cold, "cold.cram");
    let tiny = build(Level::Tiny, "tiny.cram");
    let single = build(Level::Explicit(9), "single.cram");

    assert_ne!(
        cold, single,
        "control: the single-attempt path must give a different archive from --small on this input"
    );
    assert!(
        tiny == cold,
        "--tiny wrote a different .cram from --small ({} vs {} bytes); --tiny matching the \
         single-attempt control: {}",
        tiny.len(),
        cold.len(),
        tiny == single
    );
    let _ = fs::remove_dir_all(&dir);
}

/// **On a `zstd-c` build, `--tiny` must not write its packs as single-attempt zstd.**
///
/// That is the shape the bug took on the released binaries. A pack's plaintext begins with its
/// codec's own magic, so the witness is which magic the archive carries: the XZ stream header for
/// `--tiny`, and the zstd frame header for the single-attempt control, which shows the check can see
/// a zstd pack when there is one.
#[cfg(feature = "zstd-c")]
#[test]
fn tiny_cram_packs_are_not_single_attempt_zstd() {
    const XZ_MAGIC: &[u8] = &[0xFD, b'7', b'z', b'X', b'Z', 0x00];
    const ZSTD_MAGIC: &[u8] = &[0x28, 0xB5, 0x2F, 0xFD];
    let has = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);

    let dir = scratch("tiny-zstd");
    let src = mixed_tree(&dir);
    let tiny_arc = dir.join("tiny.cram");
    let single_arc = dir.join("single.cram");
    for (arc, level) in [(&tiny_arc, Level::Tiny), (&single_arc, Level::Explicit(9))] {
        create(
            arc,
            &src,
            CreateOptions {
                level,
                ..Default::default()
            },
        );
    }
    let tiny = fs::read(&tiny_arc).unwrap();
    let single = fs::read(&single_arc).unwrap();

    assert!(
        has(&single, ZSTD_MAGIC),
        "control: the single-attempt path on a zstd-c build writes a zstd pack"
    );
    assert!(
        has(&tiny, XZ_MAGIC) && !has(&tiny, ZSTD_MAGIC),
        "--tiny wrote a zstd pack: it skipped the --small candidate screen"
    );
    let _ = fs::remove_dir_all(&dir);
}
