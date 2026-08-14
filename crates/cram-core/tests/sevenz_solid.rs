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
//! `CRAM_7Z_SOLID` is process-wide, and the tests in one file run **concurrently in one binary**.
//! A test that sets it changes what every other test's create does while it is set, so they are
//! serialised through [`ENV_LOCK`] rather than trusted to miss each other.
//!
//! That is not hypothetical. Adding a second env-setting test here made
//! `solid_beats_non_solid_on_size` fail intermittently — its "non-solid" archive came out solid —
//! and because it depended on timing it first appeared when an unrelated feature changed how fast
//! the binary ran, which reads as "that feature broke 7z" and is not what happened.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use cram_core::engine;
use cram_core::format::Format;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

/// Serialises every test in this file. See the module comment: one of them sets a process-wide
/// environment variable that changes what a create does.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Take the lock, surviving a panic in another test rather than cascading into a poisoned-mutex
/// failure that hides which test actually broke.
fn serialised() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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
    create_7z_with(src, out, CreateOptions::default())
}

fn create_7z_with(src: &Path, out: &Path, opts: CreateOptions) -> u64 {
    let report = engine::create::create(
        out,
        Format::sevenz(),
        std::slice::from_ref(&src.to_path_buf()),
        opts,
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
    let _serial = serialised();
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

/// A solid block holds one open file handle per entry until it is written, so a tree with more
/// files than the process may have descriptors must still archive.
///
/// This is a real defect that shipped: the per-block entry cap was tuned on Windows, where a process
/// may hold millions of handles, and on Linux — soft limit 1024 by default, 256 on macOS — a
/// 41,305-file tree died after 0.15s with `Too many open files`. Every other 7z test here uses 3 or
/// 60 files, so all three CI platforms passed it.
///
/// 1,200 files: above the 512-entry cap and above Linux's default 1024, so on a Unix runner this
/// exercises the descriptor-exhaustion path for real rather than merely covering multiple blocks.
#[test]
fn more_files_than_the_descriptor_limit_still_archives() {
    let _serial = serialised();
    let dir = scratch("fdlimit");
    let data = dir.join("many");
    fs::create_dir_all(&data).unwrap();
    // Deliberately tiny bodies: the file COUNT is what this test is about, and LZMA2 in a debug
    // build is slow enough that a 200-byte payload each turned this into a 60-second test.
    for i in 0..1200u32 {
        fs::write(data.join(format!("f{i:05}.txt")), format!("e{i}\n")).unwrap();
    }

    let archive = dir.join("many.7z");
    let entries = create_7z(&data, &archive);
    assert_eq!(entries, 1201, "expected 1200 files plus the root directory");

    let out = dir.join("out");
    assert_eq!(extract_and_count(&archive, &out), 1200);
    assert_eq!(
        fs::read(out.join("many/f00999.txt")).unwrap(),
        b"e999\n",
        "content mismatch after a multi-block, descriptor-bounded create"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The point of the change: a shared dictionary across many similar small files must produce a
/// materially smaller archive than one dictionary per entry.
#[test]
fn solid_beats_non_solid_on_size() {
    let _serial = serialised();
    let dir = scratch("size");
    let src = build_tree(&dir);

    let solid = dir.join("solid.7z");
    let solid_entries = create_7z(&src, &solid);

    // Through the option, which is what `cram a --no-solid` sets. The environment variable still
    // works and still wins where it is set, but a user-facing choice that changes archive layout
    // should be reachable without one, and for a long time it was not: `CreateOptions::solid` said
    // `false` while the writer ignored it and made every archive solid anyway.
    let non_solid = dir.join("nonsolid.7z");
    let non_solid_entries = create_7z_with(
        &src,
        &non_solid,
        CreateOptions {
            solid: false,
            ..CreateOptions::default()
        },
    );

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

/// The environment variable still wins where it is set, and the option decides otherwise.
///
/// Both directions matter. `CRAM_7Z_SOLID` was the only way to reach this for months and may sit in
/// somebody's script, so it cannot start being ignored; and a library caller that builds its own
/// `CreateOptions` must get what it asked for without knowing the variable exists.
#[test]
fn the_environment_override_beats_the_option_and_nothing_else_does() {
    let _serial = serialised();
    let dir = scratch("override");
    let src = build_tree(&dir);

    let not_solid = CreateOptions {
        solid: false,
        ..CreateOptions::default()
    };

    // Option says non-solid, environment forces solid: the environment wins.
    std::env::set_var("CRAM_7Z_SOLID", "1");
    let forced = dir.join("forced-solid.7z");
    create_7z_with(&src, &forced, not_solid.clone());
    std::env::remove_var("CRAM_7Z_SOLID");

    // Same option, no environment: the option decides.
    let honoured = dir.join("honoured.7z");
    create_7z_with(&src, &honoured, not_solid);

    let a = fs::metadata(&forced).unwrap().len();
    let b = fs::metadata(&honoured).unwrap().len();
    assert!(
        a < b,
        "CRAM_7Z_SOLID=1 must override solid:false ({a} bytes should be the solid one, \
         against {b} for the option-honoured non-solid archive)"
    );

    let _ = fs::remove_dir_all(&dir);
}
