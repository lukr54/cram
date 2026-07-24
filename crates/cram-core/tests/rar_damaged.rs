//! A damaged RAR must not cost you the files that are still good.
//!
//! UnRAR's safe Rust API takes the archive by value when reading an entry and drops the C handle if
//! that read fails, so a single "File CRC error" leaves no cursor to continue through, and WinRAR,
//! the reference behaviour here, reports that file and carries on to the next one. The backend
//! therefore rebuilds the cursor past a damaged entry, which is what this test pins down.
//!
//! Skipped when WinRAR is absent (CI), matching `interop_realworld.rs`: Cram cannot create RAR (the
//! UnRAR licence forbids writing it), so the fixture has to come from the real tool.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use cram_core::engine;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::ExtractOptions;

fn rar_exe() -> Option<PathBuf> {
    [
        "C:/Program Files/WinRAR/Rar.exe",
        "C:/Program Files (x86)/WinRAR/Rar.exe",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-rardmg-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Deterministic pseudo-random bytes, incompressible enough that the archive layout is predictable,
/// so the corruption below lands inside one entry's payload rather than in a header.
fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

#[test]
fn one_damaged_entry_does_not_lose_the_others() {
    let Some(rar) = rar_exe() else {
        eprintln!("skip rar_damaged: WinRAR (Rar.exe) not found");
        return;
    };
    let dir = scratch("mixed");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();

    // The third file is much larger than the rest, so the middle of the archive is squarely inside
    // its compressed payload, exactly one entry gets damaged.
    let names = [
        "file1.bin",
        "file2.bin",
        "file3.bin",
        "file4.bin",
        "file5.bin",
    ];
    for (i, name) in names.iter().enumerate() {
        let len = if i == 2 { 400_000 } else { 60_000 };
        fs::write(src.join(name), noise(i as u64 + 1, len)).unwrap();
    }

    let archive = dir.join("damaged.rar");
    let ok = Command::new(&rar)
        .args(["a", "-ep", "-m1", "-idq"])
        .arg(&archive)
        .args(names.iter().map(|n| src.join(n)))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !archive.is_file() {
        eprintln!("skip rar_damaged: WinRAR failed to build the fixture");
        return;
    }

    // Flip a run of bytes a third of the way in: inside file3's data, clear of every header.
    let mut bytes = fs::read(&archive).unwrap();
    let start = bytes.len() / 3;
    for b in bytes[start..start + 3_000].iter_mut() {
        *b ^= 0xFF;
    }
    fs::write(&archive, &bytes).unwrap();

    let out = dir.join("out");
    let report = engine::extract(
        &archive,
        &out,
        Arc::new(NoPassword),
        ExtractOptions::default(),
        &NullSink,
    )
    .expect("a damaged entry must not fail the whole extraction");

    // The damaged entry is reported by name...
    assert_eq!(
        report.failed.len(),
        1,
        "expected exactly one failure, got {:?}",
        report.failed
    );
    assert!(
        report.failed[0].0.contains("file3"),
        "the failure should name the damaged entry, got {:?}",
        report.failed[0]
    );
    assert!(
        !report.is_ok(),
        "a run with failures must never report as clean"
    );

    // ...and every other file comes out whole. file4 and file5 are the load-bearing part of this
    // assertion: they sit AFTER the damaged entry, so only a rebuilt cursor reaches them at all.
    assert_eq!(
        report.extracted, 4,
        "the four intact files should all be extracted"
    );
    for (i, name) in names.iter().enumerate() {
        let got = out.join(name);
        if i == 2 {
            assert!(
                !got.exists(),
                "the damaged entry must not be left on disk as a partial file"
            );
            continue;
        }
        let want = fs::read(src.join(name)).unwrap();
        assert_eq!(
            fs::read(&got).unwrap(),
            want,
            "{name} should be byte-identical to the source"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Guards against: the cursor-rebuild path changing behaviour on an ordinary, undamaged archive.
#[test]
fn an_intact_rar_still_extracts_cleanly() {
    let Some(rar) = rar_exe() else {
        eprintln!("skip rar_damaged: WinRAR (Rar.exe) not found");
        return;
    };
    let dir = scratch("intact");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    for i in 0..4 {
        fs::write(src.join(format!("f{i}.bin")), noise(i + 10, 40_000)).unwrap();
    }
    let archive = dir.join("intact.rar");
    let ok = Command::new(&rar)
        .args(["a", "-ep", "-m1", "-idq"])
        .arg(&archive)
        .arg(src.join("*.bin"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !archive.is_file() {
        eprintln!("skip rar_damaged: WinRAR failed to build the fixture");
        return;
    }

    let out = dir.join("out");
    let report = engine::extract(
        &archive,
        &out,
        Arc::new(NoPassword),
        ExtractOptions::default(),
        &NullSink,
    )
    .expect("extract");
    assert!(
        report.is_ok(),
        "intact archive reported failures: {:?}",
        report.failed
    );
    assert_eq!(report.extracted, 4);
    let _ = fs::remove_dir_all(&dir);
}
