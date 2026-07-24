//! Real-world interop. Fuzzing proves the parsers don't *crash*; it does NOT prove cram
//! correctly reads an archive produced by the actual incumbents. This test drives whichever of the
//! real tools are installed — 7-Zip (`7z.exe` → `.7z` and `.zip`), Windows' bundled bsdtar
//! (`tar.exe` → `.tar`), and WinRAR (`Rar.exe` → `.rar`, which cram can only *read*) — over a corpus
//! with the things archivers actually trip on (nested dirs, an empty file, a Unicode name, a name with
//! a space) and asserts cram extracts every file back byte-for-byte.
//!
//! It is **self-skipping**: a tool that isn't installed (or fails to produce an archive) is noted and
//! skipped, so the test is a no-op on a machine without that tool rather than flaky. When a tool IS
//! present, a mismatch is a hard failure — that's the interop guarantee.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use cram_core::engine;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;

fn first_existing(cands: &[&str]) -> Option<PathBuf> {
    cands.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Recursively map every file under `root` to its bytes, keyed by forward-slash relative path.
/// Directories are ignored (tools disagree on empty-dir handling; file content + paths is the contract).
fn file_map(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                let key = rel.to_string_lossy().replace('\\', "/");
                out.insert(key, std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, root, &mut m);
    m
}

/// Build the corpus under `<root>/src/files/…` and return `<root>/src` (the cwd tools archive from,
/// so archive entries are rooted at `files/`).
fn build_corpus(root: &Path) -> PathBuf {
    let files = root.join("src/files");
    std::fs::create_dir_all(files.join("nested/deep")).unwrap();
    std::fs::write(files.join("hello.txt"), b"hello world\n").unwrap();
    std::fs::write(files.join("empty.txt"), b"").unwrap();
    std::fs::write(
        files.join("nested/deep/data.bin"),
        (0..2000u32).map(|i| i as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    std::fs::write(files.join("café.txt"), "unicode café content".as_bytes()).unwrap();
    std::fs::write(files.join("with space.txt"), b"spaces in the name").unwrap();
    root.join("src")
}

/// Run `exe args…` in `cwd`, returning true on a clean exit (tool absent / nonzero → false).
fn run_tool(exe: &Path, args: &[&str], cwd: &Path) -> bool {
    Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn cram_reads_archives_made_by_the_real_incumbents() {
    let root = std::env::temp_dir().join(format!("cram-interop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let src = build_corpus(&root);
    let want = file_map(&src); // keys like "files/hello.txt"
    assert!(want.len() >= 5, "corpus should have the tricky files");

    let seven = first_existing(&[
        "C:/Program Files/7-Zip/7z.exe",
        "C:/Program Files (x86)/7-Zip/7z.exe",
    ]);
    let tar = first_existing(&["C:/Windows/System32/tar.exe"]);
    let rar = first_existing(&["C:/Program Files/WinRAR/Rar.exe"]);

    // (label, produce-the-archive closure). Each returns the archive path if it was created.
    let mut cases: Vec<(&str, PathBuf)> = Vec::new();
    if let Some(z) = &seven {
        for name in ["corpus.7z", "corpus.zip"] {
            let arc = root.join(name);
            if run_tool(
                z,
                &["a", "-bso0", "-bsp0", "-y", arc.to_str().unwrap(), "files"],
                &src,
            ) {
                cases.push((
                    if name.ends_with(".7z") {
                        "7-Zip .7z"
                    } else {
                        "7-Zip .zip"
                    },
                    arc,
                ));
            } else {
                eprintln!("skip {name}: 7-Zip failed to create it");
            }
        }
    } else {
        eprintln!("skip 7-Zip cases: 7z.exe not found");
    }
    if let Some(t) = &tar {
        let arc = root.join("corpus.tar");
        if run_tool(t, &["-cf", arc.to_str().unwrap(), "files"], &src) {
            cases.push(("bsdtar .tar", arc));
        } else {
            eprintln!("skip .tar: bsdtar failed to create it");
        }
    } else {
        eprintln!("skip .tar: tar.exe not found");
    }
    if let Some(r) = &rar {
        let arc = root.join("corpus.rar");
        // -r recurse, -idq quiet, -ep1 keep paths relative to the archived dir.
        if run_tool(
            r,
            &["a", "-r", "-idq", arc.to_str().unwrap(), "files"],
            &src,
        ) && arc.is_file()
        {
            cases.push(("WinRAR .rar (read-only)", arc));
        } else {
            eprintln!("skip .rar: WinRAR failed to create it");
        }
    } else {
        eprintln!("skip .rar: Rar.exe not found");
    }

    if cases.is_empty() {
        eprintln!("no incumbent archivers installed — interop test is a no-op here");
        return;
    }

    let n = cases.len();
    for (label, arc) in cases {
        let out = root.join(format!(
            "out-{}",
            label.replace([' ', '.', '(', ')', '-'], "_")
        ));
        engine::extract(
            &arc,
            &out,
            Arc::new(NoPassword),
            Default::default(),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("{label}: cram failed to extract a real archive: {e}"));

        let got = file_map(&out);
        assert_eq!(
            got, want,
            "{label}: cram's extraction does not match the source tree byte-for-byte"
        );
        eprintln!("interop OK: read a real {label} byte-for-byte");
    }
    eprintln!("interop: verified {n} archive(s) from installed incumbents");

    let _ = std::fs::remove_dir_all(&root);
}
