//! **A RAR entry bigger than memory must still extract.**
//!
//! UnRAR's safe Rust API has no per-chunk hook: `read()` hands back the whole entry as one `Vec`.
//! The backend used to bound that with a flat 2 GiB refusal, which meant a repacked game carrying
//! one asset over that size extracted every *other* file, reported a single failure, and left an
//! install that did not run. 7-Zip, WinRAR and Bandizip all extract such an archive, because none
//! of them routes the bytes through memory.
//!
//! It now hands anything above [`inmem_ceiling`] to UnRAR's own extract-to-file call and streams the
//! result back from a scratch file. Verified by hand against a real 2.5 GiB entry, which extracted
//! byte-identical in 10.7 s; a fixture that size has no business in a test suite, so the ceiling is
//! lowered instead. `CRAM_RAR_INMEM` exists for exactly this — it is the only way to exercise the
//! path without a multi-gigabyte file.
//!
//! Its own binary because it sets an environment variable, which is process-wide.
//!
//! Skipped when WinRAR is absent (CI): Cram cannot create RAR, the UnRAR licence forbids writing it,
//! so the fixture has to come from the real tool.

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
        "/scratch/bench/bin/rar",
        "/usr/bin/rar",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

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
fn an_entry_over_the_memory_ceiling_extracts_through_a_scratch_file() {
    let Some(rar) = rar_exe() else {
        eprintln!("skip rar_large_entry: Rar not found");
        return;
    };
    let dir = std::env::temp_dir().join(format!("cram-rarbig-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();

    // Two entries either side of the ceiling set below, so one run covers both paths and proves the
    // switch is per entry rather than per archive.
    let big = noise(7, 400_000);
    let small = noise(9, 1_000);
    fs::write(dir.join("src/big.bin"), &big).unwrap();
    fs::write(dir.join("src/small.bin"), &small).unwrap();

    let archive = dir.join("t.rar");
    let ok = Command::new(&rar)
        .args(["a", "-m0", "-y", "-ep1"])
        .arg(&archive)
        .arg(dir.join("src"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok || !archive.is_file() {
        eprintln!("skip rar_large_entry: Rar failed to build the fixture");
        let _ = fs::remove_dir_all(&dir);
        return;
    }

    // 64 KiB: above `small.bin`, well below `big.bin`. Safe to set here, this binary holds one test.
    std::env::set_var("CRAM_RAR_INMEM", "65536");
    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    let report = engine::extract(
        &archive,
        &out,
        Arc::new(NoPassword),
        ExtractOptions::default(),
        &NullSink,
    );
    std::env::remove_var("CRAM_RAR_INMEM");
    let report = report.unwrap_or_else(|e| panic!("extract: {e}"));

    assert!(
        report.failed.is_empty(),
        "an entry over the in-memory ceiling was reported as a failure instead of being streamed: \
         {:?}",
        report.failed
    );

    // The bytes are what matters. A scratch-file path that loses or truncates data would still
    // report success, which is the failure mode worth pinning.
    // `-ep1` keeps the base folder in the entry names, so they land under `out/src/`.
    for (name, want) in [("src/big.bin", &big), ("src/small.bin", &small)] {
        let got = fs::read(out.join(name))
            .unwrap_or_else(|e| panic!("{name} missing after extraction: {e}"));
        assert_eq!(got, *want, "{name} came back different");
    }

    // The scratch copy is a temporary, not a leftover. It is written beside the archive, so the
    // archive's own directory is where a leak would show.
    let leaked: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".cram-rar-"))
        .collect();
    assert!(leaked.is_empty(), "scratch files left behind: {leaked:?}");

    let _ = fs::remove_dir_all(&dir);
}
