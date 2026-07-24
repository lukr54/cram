//! 7z encryption correctness — the create side must produce archives that (1) round-trip through
//! our own reader with the hardened KDF parameters (num_cycles_power = 19, one archive salt, fresh
//! per-entry IVs), and (2) actually encrypt the header when NamesToo is requested, including the
//! empty/dirs-only case (no files, only directories), where the header must still be encrypted.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::Format;
use cram_core::formats;
use cram_core::progress::NullSink;
use cram_core::secret::{EncryptSpec, FixedPassword, HeaderMode, NoPassword, Secret};
use cram_core::writer::CreateOptions;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-7zc-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn sevenz_encrypted_multi_entry_round_trips() {
    let dir = scratch("rt");
    let a = b"alpha content line\n".repeat(500);
    let b = b"totally different beta bytes\n".repeat(700);
    fs::write(dir.join("a.txt"), &a).unwrap();
    fs::write(dir.join("b.txt"), &b).unwrap();

    let archive = dir.join("enc.7z");
    let pw = "Hardened-KDF-19";
    let opts = CreateOptions {
        encrypt: Some(EncryptSpec::new(Secret::new(pw))),
        ..Default::default()
    };
    engine::create::create(
        &archive,
        Format::sevenz(),
        &[dir.join("a.txt"), dir.join("b.txt")],
        opts,
        &NullSink,
    )
    .expect("create encrypted 7z");

    // Round-trips through our own reader: the per-entry AES chains (shared archive salt, fresh IV
    // per entry, cycles=19) must decode back to the exact source bytes.
    let out = dir.join("out");
    let rep = engine::extract(
        &archive,
        &out,
        Arc::new(FixedPassword(Secret::new(pw))),
        Default::default(),
        &NullSink,
    )
    .expect("extract");
    assert!(rep.failed.is_empty(), "failures: {:?}", rep.failed);
    assert_eq!(fs::read(out.join("a.txt")).unwrap(), a);
    assert_eq!(fs::read(out.join("b.txt")).unwrap(), b);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sevenz_names_too_header_encrypted_even_without_file_entries() {
    let dir = scratch("hdr");
    // A dirs-only input: `add_file` never runs, so the writer's finish() must install the AES
    // configuration itself for the header to be encrypted at all.
    fs::create_dir_all(dir.join("secretdir")).unwrap();

    let archive = dir.join("names.7z");
    let pw = "Names-Are-Secret";
    let mut spec = EncryptSpec::new(Secret::new(pw));
    spec.header = HeaderMode::NamesToo;
    let opts = CreateOptions {
        encrypt: Some(spec),
        ..Default::default()
    };
    engine::create::create(
        &archive,
        Format::sevenz(),
        &[dir.join("secretdir")],
        opts,
        &NullSink,
    )
    .expect("create dirs-only NamesToo 7z");

    // Without the password the listing must be unreadable — before the fix this open SUCCEEDED
    // and revealed the directory name because the header went out in plaintext.
    let no_pw = formats::open(&archive, Format::sevenz(), Arc::new(NoPassword));
    assert!(
        no_pw.is_err(),
        "NamesToo header must not be readable without the password"
    );

    // With the password the listing works and shows the entry.
    let with_pw = formats::open(
        &archive,
        Format::sevenz(),
        Arc::new(FixedPassword(Secret::new(pw))),
    )
    .expect("open with password");
    let names: Vec<_> = with_pw
        .entries()
        .unwrap()
        .iter()
        .map(|e| e.name().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("secretdir")),
        "expected the dir entry, got {names:?}"
    );
    drop(with_pw);

    let _ = fs::remove_dir_all(&dir);
}
