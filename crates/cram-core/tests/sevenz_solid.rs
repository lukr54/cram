//! Solid-block 7z creation: it must round-trip, keep every directory, and actually be smaller.
//!
//! `.7z` used to be written one independently-decodable pack per entry. On a 41,305-file tree that
//! cost 23% of archive size against 7-Zip, because every small file got its own LZMA2 dictionary,
//! and it pinned create to one core because a pack is compressed inline into the output stream.
//! Entries are now grouped into ~64 MiB solid blocks.
//!
//! The directory case is the subtle one. A 7z directory has no stream and
//! `push_archive_entries` asserts one reader per entry, so directories cannot ride inside a solid
//! pack; writing them inline instead would close the open block every time the walk stepped into a
//! new directory. They are therefore deferred and written after every file block, which reorders
//! the header. These tests pin down that nothing is lost by that.
//!
//! `CRAM_7Z_SOLID` is process-wide. Each integration test file is its own binary, so the env work
//! is confined to a single test here to avoid racing the other one.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::Format;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-7z-solid-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Many small, highly similar files across several nested directories: the shape where a shared
/// dictionary pays and a per-entry dictionary cannot. Also includes empty directories, which have
/// no files to imply them and so are only preserved if the deferred directory entries survive.
fn build_tree(root: &Path) -> PathBuf {
    let data = root.join("data");
    for d in ["a", "b", "b/deep", "empty", "empty/deeper"] {
        fs::create_dir_all(data.join(d)).unwrap();
    }
    for i in 0..60u32 {
        let body =
            format!("record {i} of a repetitive corpus, shared across every file\n").repeat(60);
        let sub = if i % 2 == 0 { "a" } else { "b/deep" };
        fs::write(data.join(sub).join(format!("f{i}.txt")), body).unwrap();
    }
    data
}

fn create_7z(src: &Path, out: &Path) -> u64 {
    let report = engine::create::create(
        out,
        Format::sevenz(),
        std::slice::from_ref(&src.to_path_buf()),
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create 7z");
    report.entries
}

fn extract_and_count(archive: &Path, out: &Path) -> u64 {
    let rep = engine::extract(
        archive,
        out,
        Arc::new(NoPassword),
        Default::default(),
        &NullSink,
    )
    .expect("extract");
    assert!(rep.failed.is_empty(), "failures: {:?}", rep.failed);
    rep.extracted
}

fn tree_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v = Vec::new();
    for p in walk(root) {
        if p.is_file() {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            v.push((rel, fs::read(&p).unwrap()));
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out.sort();
    out
}

#[test]
fn solid_7z_round_trips_and_keeps_empty_directories() {
    let dir = scratch("roundtrip");
    let src = build_tree(&dir);
    let before = tree_files(&src);

    let archive = dir.join("solid.7z");
    create_7z(&src, &archive);

    let out = dir.join("out");
    let extracted = extract_and_count(&archive, &out);
    assert_eq!(extracted, 60, "expected 60 files back");
    assert_eq!(
        tree_files(&out.join("data")),
        before,
        "content differs after round-trip"
    );

    // Deferring directory entries must not lose the ones with no files under them.
    for d in ["empty", "empty/deeper"] {
        assert!(
            out.join("data").join(d).is_dir(),
            "empty directory {d} was not restored"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The point of the change: a shared dictionary across many similar small files must produce a
/// materially smaller archive than one dictionary per entry.
#[test]
fn solid_beats_non_solid_on_size() {
    let dir = scratch("size");
    let src = build_tree(&dir);

    let solid = dir.join("solid.7z");
    let solid_entries = create_7z(&src, &solid);

    let non_solid = dir.join("nonsolid.7z");
    std::env::set_var("CRAM_7Z_SOLID", "0");
    let non_solid_entries = create_7z(&src, &non_solid);
    std::env::remove_var("CRAM_7Z_SOLID");

    assert_eq!(
        solid_entries, non_solid_entries,
        "the two layouts must archive the same number of entries"
    );

    let a = fs::metadata(&solid).unwrap().len();
    let b = fs::metadata(&non_solid).unwrap().len();
    assert!(
        a < b,
        "solid ({a} bytes) should be smaller than non-solid ({b} bytes)"
    );

    // Both layouts must still extract; non-solid is the documented escape hatch, not dead code.
    let out = dir.join("out-nonsolid");
    assert_eq!(extract_and_count(&non_solid, &out), 60);
    let _ = fs::remove_dir_all(&dir);
}
