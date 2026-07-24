//! The native `.cram` content-defined-dedup format. Proves (1) a mixed tree round-trips
//! byte-for-byte through create → extract, and (2) cross-file dedup actually eliminates duplicate
//! content: N identical files store their bytes once, so `dedup_saved == (N-1)*size` exactly and the
//! archive is a fraction of the logical input — while every copy still extracts correctly.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::error::ArchiveError;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::{EncryptSpec, FixedPassword, NoPassword, Secret};
use cram_core::writer::CreateOptions;
use cram_core::{engine, formats};

/// Does `hay` contain `needle` as a contiguous subslice?
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-cram-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Deterministic high-entropy bytes (xorshift, seeded) — incompressible, so the archive size
/// reflects dedup rather than compression.
fn blob(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
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

fn extract_and_check(archive: &Path, dir: &Path, sources: &[(String, Vec<u8>)]) {
    let out = dir.join("out");
    let rep = engine::extract(
        archive,
        &out,
        Arc::new(NoPassword),
        Default::default(),
        &NullSink,
    )
    .expect("extract");
    assert!(rep.failed.is_empty(), "failures: {:?}", rep.failed);
    for (name, content) in sources {
        let got = fs::read(out.join(name)).unwrap_or_else(|e| panic!("missing {name}: {e}"));
        assert_eq!(&got, content, "content mismatch for {name}");
    }
}

/// A failing create must leave a PRE-EXISTING archive at the destination untouched (and no staging
/// droppings). Before the staging fix, the writer truncated the destination the moment it opened,
/// so a create that failed mid-stream destroyed the old archive.
#[test]
fn failed_create_preserves_preexisting_archive() {
    let dir = scratch("clobber");
    let sentinel = b"PRE-EXISTING ARCHIVE BYTES (sentinel)".to_vec();
    let dest = dir.join("out.zip");
    fs::write(&dest, &sentinel).unwrap();

    // An input that exists at plan time but cannot be READ when the create loop streams it, so the
    // failure lands AFTER the writer is live — the exact case the staging fix must survive.
    let locked_path = dir.join("locked.bin");
    fs::write(&locked_path, b"soon locked").unwrap();

    // Windows: hold it open with share_mode 0 so the loop's `File::open` is denied.
    #[cfg(windows)]
    let _lock = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&locked_path)
            .unwrap()
    };
    // Unix: strip all read permission. Root ignores the mode bits, which would make the read succeed
    // and the test meaningless — detect that by probing and skip rather than fail spuriously.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked_path, fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&locked_path).is_ok() {
            eprintln!("skipping failed_create test: reads not denied (likely running as root)");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
    }

    let err = engine::create::create(
        &dest,
        Format::zip(),
        std::slice::from_ref(&locked_path),
        CreateOptions::default(),
        &NullSink,
    );
    assert!(err.is_err(), "creating from a locked input must fail");
    assert_eq!(
        fs::read(&dest).unwrap(),
        sentinel,
        "the pre-existing archive must survive a failed create"
    );
    let mut staging = dest.clone().into_os_string();
    staging.push(".cram-partial");
    assert!(
        !Path::new(&staging).exists(),
        "no staging droppings after a failed create"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cram_mixed_tree_round_trips() {
    let dir = scratch("rt");
    fs::create_dir_all(dir.join("src/sub")).unwrap();
    let text = b"the quick brown fox\n".repeat(5000); // spans several chunks, compressible
    let nested = b"nested data ".repeat(3000);
    let bin = blob(0xABCD, 300 * 1024); // incompressible
    fs::write(dir.join("src/readme.txt"), &text).unwrap();
    fs::write(dir.join("src/data.bin"), &bin).unwrap();
    fs::write(dir.join("src/sub/note.txt"), &nested).unwrap();

    let archive = dir.join("out.cram");
    let report = engine::create::create(
        &archive,
        Format::cram(cram_core::format::Codec::None),
        &[dir.join("src")],
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create");
    // 3 files + 2 dirs (src/, src/sub/).
    assert_eq!(report.entries, 5);

    // `cram l` path: the reader lists all entries.
    let reader = formats::open(
        &archive,
        Format::cram(cram_core::format::Codec::None),
        Arc::new(NoPassword),
    )
    .unwrap();
    let names: Vec<_> = reader
        .entries()
        .unwrap()
        .iter()
        .map(|e| e.name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "src/readme.txt"));
    assert!(names.iter().any(|n| n == "src/data.bin"));
    drop(reader);

    extract_and_check(
        &archive,
        &dir,
        &[
            ("src/readme.txt".into(), text),
            ("src/data.bin".into(), bin),
            ("src/sub/note.txt".into(), nested),
        ],
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cram_dedups_identical_files() {
    let dir = scratch("dedup");
    const N: usize = 4;
    const SIZE: usize = 300 * 1024;
    let content = blob(0x1234_5678, SIZE);

    let mut sources = Vec::new();
    for i in 0..N {
        let name = format!("copy{i}.bin");
        fs::write(dir.join(&name), &content).unwrap();
        sources.push((name, content.clone()));
    }
    let inputs: Vec<PathBuf> = sources.iter().map(|(n, _)| dir.join(n)).collect();

    let archive = dir.join("dup.cram");
    let report = engine::create::create(
        &archive,
        Format::cram(cram_core::format::Codec::None),
        &inputs,
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create");

    assert_eq!(report.entries, N as u64);
    // Every file after the first is entirely duplicate content → exactly (N-1)*SIZE eliminated.
    assert_eq!(report.in_bytes, (N * SIZE) as u64);
    assert_eq!(report.dedup_saved, ((N - 1) * SIZE) as u64);
    // Incompressible content stored once → the archive is far smaller than the logical input.
    assert!(
        report.out_bytes < report.in_bytes / 2,
        "expected heavy dedup: out {} vs in {}",
        report.out_bytes,
        report.in_bytes
    );

    // Every copy still reconstructs byte-for-byte from the single stored instance.
    extract_and_check(&archive, &dir, &sources);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cram_encrypted_round_trips_and_gates_password() {
    let dir = scratch("enc");
    let secret_blob = blob(0xAA11, 200 * 1024);
    let text = b"top secret dossier line\n".repeat(4000);
    fs::write(dir.join("a.bin"), &secret_blob).unwrap();
    fs::write(dir.join("a_copy.bin"), &secret_blob).unwrap(); // duplicate → must still dedup
    fs::write(dir.join("dossier.txt"), &text).unwrap();
    let inputs = vec![
        dir.join("a.bin"),
        dir.join("a_copy.bin"),
        dir.join("dossier.txt"),
    ];
    let sources = vec![
        ("a.bin".to_string(), secret_blob.clone()),
        ("a_copy.bin".to_string(), secret_blob.clone()),
        ("dossier.txt".to_string(), text.clone()),
    ];

    let archive = dir.join("enc.cram");
    let pw = "Cram-Secret-42";
    let opts = CreateOptions {
        encrypt: Some(EncryptSpec::new(Secret::new(pw))),
        ..Default::default()
    };
    let report = engine::create::create(
        &archive,
        Format::cram(Codec::None),
        &inputs,
        opts,
        &NullSink,
    )
    .expect("create encrypted");
    // Dedup happens on plaintext before encryption → the duplicate is still eliminated.
    assert_eq!(report.dedup_saved, (200 * 1024) as u64);

    // Neither the file contents nor the file names appear in the archive bytes (packs + index sealed).
    let raw = fs::read(&archive).unwrap();
    assert!(
        !contains(&raw, b"top secret dossier line"),
        "plaintext leaked"
    );
    assert!(!contains(&raw, b"dossier.txt"), "file name leaked");

    // Correct password → byte-for-byte.
    let out = dir.join("ok");
    let rep = engine::extract(
        &archive,
        &out,
        Arc::new(FixedPassword(Secret::new(pw))),
        Default::default(),
        &NullSink,
    )
    .expect("extract ok");
    assert!(rep.failed.is_empty(), "failures: {:?}", rep.failed);
    for (name, content) in &sources {
        assert_eq!(
            &fs::read(out.join(name)).unwrap(),
            content,
            "mismatch {name}"
        );
    }

    // Wrong password → the whole open fails cleanly (before any file is written).
    let bad = engine::extract(
        &archive,
        &dir.join("bad"),
        Arc::new(FixedPassword(Secret::new("wrong-password"))),
        Default::default(),
        &NullSink,
    );
    assert!(
        matches!(bad, Err(ArchiveError::WrongPassword)),
        "got {bad:?}"
    );

    // No password → PasswordRequired (the listing itself is encrypted).
    let none = engine::extract(
        &archive,
        &dir.join("none"),
        Arc::new(NoPassword),
        Default::default(),
        &NullSink,
    );
    assert!(
        matches!(none, Err(ArchiveError::PasswordRequired)),
        "got {none:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}
