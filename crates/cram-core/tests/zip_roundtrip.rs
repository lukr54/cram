//! End-to-end create → read round-trips for the ZIP backend, exercising the public engine API the
//! way `cram` does. Covers plain DEFLATE and AES-256 encryption (the create side is what finally
//! makes an encrypted ZIP available to test the read side against).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::Format;
use cram_core::progress::NullSink;
use cram_core::secret::{EncryptSpec, FixedPassword, NoPassword, Secret};
use cram_core::writer::CreateOptions;

/// A fresh scratch dir under the OS temp dir, wiped if it somehow already exists.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-zip-rt-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a source tree (a compressible text file, an incompressible blob, a nested file) and return
/// the map of archive-relative name → content for later comparison.
fn make_sources(root: &Path) -> Vec<(String, Vec<u8>)> {
    fs::create_dir_all(root.join("data/sub")).unwrap();
    let text = b"the quick brown fox ".repeat(500);
    let nested = b"nested content here ".repeat(300);
    // Deterministic pseudo-random blob (incompressible) without pulling an RNG dep.
    let blob: Vec<u8> = (0..40_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
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

fn assert_extracted_matches(out_dir: &Path, sources: &[(String, Vec<u8>)]) {
    for (rel, content) in sources {
        let path = out_dir.join(rel);
        let got = fs::read(&path).unwrap_or_else(|e| panic!("missing extracted {rel}: {e}"));
        assert_eq!(&got, content, "content mismatch for {rel}");
    }
}

#[test]
fn plain_deflate_zip_round_trips() {
    let dir = scratch("plain");
    let sources = make_sources(&dir);
    let archive = dir.join("out.zip");

    let report = engine::create::create(
        &archive,
        Format::zip(),
        &[dir.join("data")],
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create");
    // 3 files + 2 dirs (data/, data/sub/).
    assert_eq!(report.entries, 5);
    assert!(report.out_bytes > 0);

    let out = dir.join("extracted");
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
    assert_extracted_matches(&out, &sources);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn aes256_zip_round_trips_and_rejects_wrong_password() {
    let dir = scratch("aes");
    let sources = make_sources(&dir);
    let archive = dir.join("enc.zip");
    let password = "R0und-Trip-Pw";

    let opts = CreateOptions {
        encrypt: Some(EncryptSpec::new(Secret::new(password))),
        ..Default::default()
    };
    engine::create::create(
        &archive,
        Format::zip(),
        &[dir.join("data")],
        opts,
        &NullSink,
    )
    .expect("create encrypted");

    // Correct password → byte-for-byte.
    let out_ok = dir.join("ok");
    let pw_ok = Arc::new(FixedPassword(Secret::new(password)));
    let rep = engine::extract(&archive, &out_ok, pw_ok, Default::default(), &NullSink)
        .expect("extract ok");
    assert_eq!(rep.extracted, 3);
    assert!(
        rep.failed.is_empty(),
        "unexpected failures: {:?}",
        rep.failed
    );
    assert_extracted_matches(&out_ok, &sources);

    // Wrong password → every encrypted entry fails (non-fatal), nothing extracted.
    let out_bad = dir.join("bad");
    let pw_bad = Arc::new(FixedPassword(Secret::new("not-the-password")));
    let rep = engine::extract(&archive, &out_bad, pw_bad, Default::default(), &NullSink)
        .expect("extract returns report");
    assert_eq!(rep.extracted, 0, "wrong password must not extract anything");
    assert_eq!(rep.failed.len(), 3, "each encrypted file should fail");

    let _ = fs::remove_dir_all(&dir);
}

/// An encrypted ZIP that Cram wrote must pass Cram's own integrity check.
///
/// Guards against: an AE-2 zero CRC being read as if it were a checksum. WinZip AES has two
/// variants: AE-1 stores the plaintext CRC-32, AE-2 stores `0` and omits it because the AES
/// authentication already proves integrity. The writer picks per entry — anything under 20 bytes
/// gets AE-2 — so a reader that compares that `0` against the recomputed CRC of a short encrypted
/// entry would declare corrupt an archive Cram has just written itself. A false "your archive is
/// damaged" is the one verdict a user cannot safely ignore, so the boundary is worth a test of its
/// own.
#[test]
fn small_entries_in_an_encrypted_zip_do_not_fail_verification() {
    let dir = scratch("aes-ae2");
    let src = dir.join("data");
    fs::create_dir_all(&src).unwrap();
    // Straddle the writer's 20-byte AE-1/AE-2 boundary in both directions.
    fs::write(src.join("tiny.txt"), b"tiny").unwrap();
    fs::write(src.join("empty.txt"), b"").unwrap();
    fs::write(
        src.join("long.txt"),
        b"comfortably more than twenty bytes of text",
    )
    .unwrap();

    let archive = dir.join("enc.zip");
    let password = "AE2-Boundary-Pw";
    engine::create::create(
        &archive,
        Format::zip(),
        std::slice::from_ref(&src),
        CreateOptions {
            encrypt: Some(EncryptSpec::new(Secret::new(password))),
            ..Default::default()
        },
        &NullSink,
    )
    .expect("create encrypted");

    let pw = Arc::new(FixedPassword(Secret::new(password)));
    let report = engine::verify::verify(&archive, pw.clone(), &NullSink).expect("verify runs");
    assert!(
        report.failures.is_empty(),
        "an encrypted archive Cram just wrote must verify clean, got: {:?}",
        report.failures
    );
    assert_eq!(report.checked, 3, "every entry should have been checked");
    // Two of the three are genuinely CRC-checked: `long.txt` keeps a real AE-1 CRC, and `empty.txt`
    // has a stored CRC of 0 that is *correct* — the CRC of no bytes is 0 — which is exactly why
    // `stored_crc` discounts a zero CRC only on a non-empty entry. Only `tiny.txt` is AE-2 with no
    // checksum to compare, and it is proven by its AES authentication instead. Pinning the count
    // stops a future change from satisfying this test by dropping CRC checking for encrypted ZIPs
    // altogether.
    assert_eq!(
        report.crc_verified, 2,
        "long.txt (AE-1) and empty.txt (CRC 0 is correct for zero bytes) should both be CRC-checked"
    );

    // And the contents must still come back byte-for-byte: treating an absent checksum as absent
    // must never widen into skipping verification of the data itself.
    let out = dir.join("out");
    let rep = engine::extract(&archive, &out, pw, Default::default(), &NullSink).expect("extract");
    assert!(rep.is_ok(), "extract failed: {:?}", rep.failed);
    assert_eq!(fs::read(out.join("data/tiny.txt")).unwrap(), b"tiny");
    assert_eq!(
        fs::read(out.join("data/long.txt")).unwrap(),
        b"comfortably more than twenty bytes of text"
    );

    let _ = fs::remove_dir_all(&dir);
}
