//! `cram convert` — re-exporting a source archive into another container preserves every file's bytes
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
