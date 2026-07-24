//! End-to-end create → read round-trips for the 7z backend: plain LZMA2, AES-256 content
//! encryption, and header (name) encryption; exercised through the public engine API.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::Format;
use cram_core::progress::NullSink;
use cram_core::secret::{EncryptSpec, FixedPassword, HeaderMode, NoPassword, Secret};
use cram_core::writer::CreateOptions;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-7z-rt-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_sources(root: &Path) -> Vec<(String, Vec<u8>)> {
    fs::create_dir_all(root.join("data/sub")).unwrap();
    let text = b"seven zip round trip ".repeat(400);
    let nested = b"nested 7z bytes ".repeat(250);
    let blob: Vec<u8> = (0..48_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 12) as u8)
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

fn assert_matches(out: &Path, sources: &[(String, Vec<u8>)]) {
    for (rel, content) in sources {
        let got = fs::read(out.join(rel)).unwrap_or_else(|e| panic!("missing {rel}: {e}"));
        assert_eq!(&got, content, "content mismatch for {rel}");
    }
}

#[test]
fn plain_7z_round_trips() {
    let dir = scratch("plain");
    let sources = make_sources(&dir);
    let archive = dir.join("out.7z");

    let report = engine::create::create(
        &archive,
        Format::sevenz(),
        &[dir.join("data")],
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create 7z");
    assert_eq!(report.entries, 5);

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
    assert!(rep.failed.is_empty(), "failures: {:?}", rep.failed);
    assert_matches(&out, &sources);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn aes256_7z_round_trips() {
    let dir = scratch("aes");
    let sources = make_sources(&dir);
    let archive = dir.join("enc.7z");
    let password = "7z-Content-Pw";

    let opts = CreateOptions {
        encrypt: Some(EncryptSpec::new(Secret::new(password))),
        ..Default::default()
    };
    engine::create::create(
        &archive,
        Format::sevenz(),
        &[dir.join("data")],
        opts,
        &NullSink,
    )
    .expect("create encrypted 7z");

    let out = dir.join("out");
    let pw = Arc::new(FixedPassword(Secret::new(password)));
    let rep = engine::extract(&archive, &out, pw, Default::default(), &NullSink).expect("extract");
    assert_eq!(rep.extracted, 3);
    assert_matches(&out, &sources);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn header_encrypted_7z_round_trips_and_hides_names() {
    let dir = scratch("hdr");
    let sources = make_sources(&dir);
    let archive = dir.join("hdr.7z");
    let password = "7z-Header-Pw";

    let opts = CreateOptions {
        encrypt: Some(EncryptSpec {
            password: Secret::new(password),
            header: HeaderMode::NamesToo,
            ..EncryptSpec::new(Secret::new(password))
        }),
        ..Default::default()
    };
    engine::create::create(
        &archive,
        Format::sevenz(),
        &[dir.join("data")],
        opts,
        &NullSink,
    )
    .expect("create header-encrypted 7z");

    // Without the password the listing can't even be read (names are encrypted).
    let no_pw = cram_core::formats::open(&archive, Format::sevenz(), Arc::new(NoPassword));
    assert!(
        no_pw.is_err(),
        "listing must fail without the header password"
    );

    // With the password it round-trips byte-for-byte.
    let out = dir.join("out");
    let pw = Arc::new(FixedPassword(Secret::new(password)));
    let rep = engine::extract(&archive, &out, pw, Default::default(), &NullSink).expect("extract");
    assert_eq!(rep.extracted, 3);
    assert_matches(&out, &sources);
    let _ = fs::remove_dir_all(&dir);
}
