//! End-to-end create → read round-trips for the tar backend across every write codec
//! (`.tar`, `.tar.gz`, `.tar.xz`, `.tar.bz2`, `.tar.lz4`, `.tar.br`, `.tar.zst`), via the public
//! engine API.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-tar-rt-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_sources(root: &Path) -> Vec<(String, Vec<u8>)> {
    fs::create_dir_all(root.join("data/sub")).unwrap();
    let text = b"tar round trip content ".repeat(400);
    let nested = b"nested tar bytes ".repeat(250);
    let blob: Vec<u8> = (0..32_000u32)
        .map(|i| (i.wrapping_mul(2_246_822_519) >> 11) as u8)
        .collect();

    fs::write(root.join("data/readme.txt"), &text).unwrap();
    fs::write(root.join("data/blob.bin"), &blob).unwrap();
    fs::write(root.join("data/sub/nested.txt"), &nested).unwrap();

    vec![
        ("data/readme.txt".into(), text),
        ("data/blob.bin".into(), blob),
        ("data/sub/nested.txt".into(), nested),
    ]
}

fn round_trip(tag: &str, codec: Codec, file_name: &str) {
    let dir = scratch(tag);
    let sources = make_sources(&dir);
    let archive = dir.join(file_name);

    let report = engine::create::create(
        &archive,
        Format::tar(codec),
        &[dir.join("data")],
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create tar");
    assert_eq!(report.entries, 5, "3 files + 2 dirs");

    let out = dir.join("out");
    let rep = engine::extract(
        &archive,
        &out,
        Arc::new(NoPassword),
        Default::default(),
        &NullSink,
    )
    .expect("extract");
    assert_eq!(rep.extracted, 3);
    assert!(
        rep.failed.is_empty(),
        "unexpected failures: {:?}",
        rep.failed
    );
    for (rel, content) in &sources {
        let got = fs::read(out.join(rel)).unwrap_or_else(|e| panic!("missing {rel}: {e}"));
        assert_eq!(&got, content, "content mismatch for {rel} in {file_name}");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn plain_tar_round_trips() {
    round_trip("plain", Codec::None, "out.tar");
}

#[test]
fn tar_gz_round_trips() {
    round_trip("gz", Codec::Gzip, "out.tar.gz");
}

#[test]
fn tar_xz_round_trips() {
    round_trip("xz", Codec::Xz, "out.tar.xz");
}

#[test]
fn tar_bz2_round_trips() {
    round_trip("bz2", Codec::Bzip2, "out.tar.bz2");
}

#[test]
fn tar_lz4_round_trips() {
    round_trip("lz4", Codec::Lz4, "out.tar.lz4");
}

#[test]
fn tar_br_round_trips() {
    round_trip("br", Codec::Brotli, "out.tar.br");
}

#[test]
fn tar_zst_round_trips() {
    round_trip("zst", Codec::Zstd, "out.tar.zst");
}
