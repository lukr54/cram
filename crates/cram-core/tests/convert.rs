//! `cram convert`, re-exporting a source archive into another container preserves every file's bytes
//! (the interop escape hatch: a `.cram` is never a dead end). Builds one `.cram`, converts it to ZIP /
//! tar / 7z through the public engine, extracts each, and checks the bytes round-trip exactly.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;
use cram_core::{formats, sniff};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-conv-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A small tree rooted at `data`; returns (archive-relative path, bytes). Mixed sizes exercise both
/// the small-entry and multi-KiB paths through the writers.
fn make_src(root: &Path) -> Vec<(String, Vec<u8>)> {
    fs::create_dir_all(root.join("data/sub")).unwrap();
    let readme = b"convert round-trip readme ".repeat(40);
    let nested = b"nested convert body ".repeat(9);
    let blob: Vec<u8> = (0..37_000u32)
        .map(|i| (i.wrapping_mul(2_246_822_519) >> 12) as u8)
        .collect();
    fs::write(root.join("data/readme.txt"), &readme).unwrap();
    fs::write(root.join("data/sub/nested.txt"), &nested).unwrap();
    fs::write(root.join("data/blob.bin"), &blob).unwrap();
    vec![
        ("data/readme.txt".into(), readme),
        ("data/sub/nested.txt".into(), nested),
        ("data/blob.bin".into(), blob),
    ]
}

#[test]
fn convert_cram_to_each_format_preserves_content() {
    let dir = scratch("each");
    let sources = make_src(&dir);

    // Build the source .cram.
    let base = dir.join("base.cram");
    engine::create::create(
        &base,
        Format::cram(Codec::None),
        &[dir.join("data")],
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create base .cram");
    let src_fmt = sniff::sniff_path(&base).unwrap();

    for (name, dst_fmt) in [
        ("out.zip", Format::zip()),
        ("out.tar", Format::tar(Codec::None)),
        ("out.7z", Format::sevenz()),
    ] {
        let dst = dir.join(name);
        engine::convert::convert(
            &base,
            src_fmt,
            &dst,
            dst_fmt,
            &CreateOptions::default(),
            Arc::new(NoPassword),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("convert to {name}: {e}"));

        // Extract the converted archive and compare every file to the original bytes.
        let out = dir.join(format!("x_{}", name.replace('.', "_")));
        engine::extract(
            &dst,
            &out,
            Arc::new(NoPassword),
            Default::default(),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("extract {name}: {e}"));
        for (rel, content) in &sources {
            let got =
                fs::read(out.join(rel)).unwrap_or_else(|e| panic!("{name}: missing {rel}: {e}"));
            assert_eq!(&got, content, "{name}: content mismatch for {rel}");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A real JPEG, the one the `.cram` recompression tests use.
const SAMPLE_JPEG: &[u8] = include_bytes!("../tests/data/sample.jpg");

/// Header byte 6 of a `.cram`: 1 for a plain archive, 2 once any entry is stored transformed.
const VERSION_PLAIN: u8 = 1;
const VERSION_XFORM: u8 = 2;

/// Build `<dir>/<tag>.cram` from a photo and a text file, with or without JPEG recompression, and
/// return its path with the (archive-relative path, bytes) of every file in it.
fn photo_cram(dir: &Path, tag: &str, recompress: bool) -> (PathBuf, Vec<(String, Vec<u8>)>) {
    let root = dir.join(tag);
    fs::create_dir_all(root.join("photos")).unwrap();
    let notes = b"not a photo ".repeat(300);
    fs::write(root.join("photos/photo.jpg"), SAMPLE_JPEG).unwrap();
    fs::write(root.join("photos/notes.txt"), &notes).unwrap();
    let arc = dir.join(format!("{tag}.cram"));
    engine::create::create(
        &arc,
        Format::cram(Codec::None),
        &[root.join("photos")],
        CreateOptions {
            recompress_images: recompress,
            ..Default::default()
        },
        &NullSink,
    )
    .expect("create photo .cram");
    let files = vec![
        ("photos/photo.jpg".into(), SAMPLE_JPEG.to_vec()),
        ("photos/notes.txt".into(), notes),
    ];
    (arc, files)
}

/// Read every file of `path` through the reader for its own format, as (name, bytes).
fn read_back(path: &Path, fmt: Format) -> Vec<(String, Vec<u8>)> {
    use std::io::Read;
    let mut reader = formats::open(path, fmt, Arc::new(NoPassword)).unwrap();
    let mut out = Vec::new();
    while let Some(mut es) = reader.next_entry().unwrap() {
        if !es.entry.is_file() {
            continue;
        }
        let mut body = Vec::new();
        es.body.read_to_end(&mut body).unwrap();
        out.push((es.entry.name().replace('\\', "/"), body));
    }
    out.sort();
    out
}

/// Convert `src` to zip, 7z and tar, read each destination back through its own reader, and
/// compare every file with `files` byte for byte. Every destination is tried before failing, so one
/// run reports each format's outcome.
fn convert_and_compare(dir: &Path, src: &Path, files: &[(String, Vec<u8>)]) {
    let mut want = files.to_vec();
    want.sort();
    let tag = src.file_stem().unwrap().to_string_lossy().into_owned();
    let names = |v: &[(String, Vec<u8>)]| v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    let mut failures = Vec::new();
    for (ext, dst_fmt) in [
        ("zip", Format::zip()),
        ("7z", Format::sevenz()),
        ("tar", Format::tar(Codec::None)),
    ] {
        let dst = dir.join(format!("{tag}.{ext}"));
        if let Err(e) = engine::convert::convert(
            src,
            Format::cram(Codec::None),
            &dst,
            dst_fmt,
            &CreateOptions::default(),
            Arc::new(NoPassword),
            &NullSink,
        ) {
            failures.push(format!("{tag} to {ext}: conversion failed: {e}"));
            continue;
        }
        let got = read_back(&dst, dst_fmt);
        if names(&got) != names(&want) {
            failures.push(format!(
                "{tag} to {ext}: files {:?}, expected {:?}",
                names(&got),
                names(&want)
            ));
            continue;
        }
        for ((name, got), (_, want)) in got.iter().zip(&want) {
            // Lengths and leading bytes, not the whole buffers, so a failure stays readable.
            if got != want {
                failures.push(format!(
                    "{tag} to {ext}: {name} came back as {} bytes {:02x?}.., the source is {} bytes {:02x?}..",
                    got.len(),
                    &got[..got.len().min(4)],
                    want.len(),
                    &want[..want.len().min(4)],
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A `.cram` whose JPEG was recompressed (format version 2) converts to zip, 7z and tar with the
/// original photo bytes. The sequential reader conversion uses once handed back the stored Lepton
/// stream instead: zip and 7z got it under the `.jpg` name with exit status 0, and tar refused it
/// because the body was shorter than the declared size.
#[test]
fn convert_recompressed_jpeg_cram_restores_the_photo() {
    let dir = scratch("lepton");
    let (arc, files) = photo_cram(&dir, "v2", true);
    // The version byte is the witness that a transform was used; without it this test proves
    // nothing.
    assert_eq!(
        fs::read(&arc).unwrap()[6],
        VERSION_XFORM,
        "the photo must have been recompressed"
    );
    convert_and_compare(&dir, &arc, &files);
    let _ = fs::remove_dir_all(&dir);
}

/// Control: the same tree without recompression is a version-1 archive and converts the same way.
#[test]
fn convert_plain_jpeg_cram_keeps_the_photo() {
    let dir = scratch("plainjpeg");
    let (arc, files) = photo_cram(&dir, "v1", false);
    assert_eq!(
        fs::read(&arc).unwrap()[6],
        VERSION_PLAIN,
        "recompression was off, so the archive must stay version 1"
    );
    convert_and_compare(&dir, &arc, &files);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn convert_round_trips_through_zip_back_to_cram() {
    // cram -> zip -> cram: the far end is still byte-identical content (dedup re-applied on the way in).
    let dir = scratch("rt");
    let sources = make_src(&dir);
    let base = dir.join("base.cram");
    engine::create::create(
        &base,
        Format::cram(Codec::None),
        &[dir.join("data")],
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();

    let zip = dir.join("mid.zip");
    engine::convert::convert(
        &base,
        Format::cram(Codec::None),
        &zip,
        Format::zip(),
        &CreateOptions::default(),
        Arc::new(NoPassword),
        &NullSink,
    )
    .unwrap();
    let back = dir.join("back.cram");
    engine::convert::convert(
        &zip,
        Format::zip(),
        &back,
        Format::cram(Codec::None),
        &CreateOptions::default(),
        Arc::new(NoPassword),
        &NullSink,
    )
    .unwrap();

    // Sanity: the round-tripped archive still opens and lists the same entries.
    let reader = formats::open(&back, Format::cram(Codec::None), Arc::new(NoPassword)).unwrap();
    let files = reader
        .entries()
        .unwrap()
        .iter()
        .filter(|e| e.is_file())
        .count();
    assert_eq!(
        files,
        sources.len(),
        "same file count after cram->zip->cram"
    );

    let out = dir.join("x_back");
    engine::extract(
        &back,
        &out,
        Arc::new(NoPassword),
        Default::default(),
        &NullSink,
    )
    .unwrap();
    for (rel, content) in &sources {
        assert_eq!(
            &fs::read(out.join(rel)).unwrap(),
            content,
            "round-trip {rel}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}
