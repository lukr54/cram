//! Symbolic links must never disappear in silence.
//!
//! No format cram writes can record a link target today. The `.cram` v1 index has no field for one
//! (`EntryMeta` is `is_dir | name | size | mode | chunk_ids`, and `mode` is specified as permission
//! bits), and the spec is frozen, so representing a symlink is a format decision rather than a code
//! change. Leaving them out is therefore the current, deliberate behaviour.
//!
//! **Leaving them out quietly is not.** A Linux kernel tree went into a `.cram` carrying 99 symlinks
//! and came out with none, while `cram t` reported the archive completely clean, because by its own
//! index it was. Unreported loss is the one failure a tool sold on backup integrity cannot have, and
//! these tests exist so it cannot come back: whatever the walk refuses to archive, it names.
//!
//! Dereferencing instead is not the obvious safer default. 7-Zip and WinRAR do it, and on that same
//! kernel tree it duplicated 8,011 files behind twelve directory symlinks; it also turns a link
//! cycle into an unbounded walk. If that is ever chosen it should be chosen on purpose.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs as unixfs;
use std::path::{Path, PathBuf};

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::writer::CreateOptions;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-symlink-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A tree with one real file, one real subdirectory, and all three kinds of link: to a file, to a
/// directory, and to a target that does not exist.
fn build_tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    fs::create_dir_all(src.join("real_dir")).unwrap();
    fs::write(src.join("real.txt"), b"the only real payload").unwrap();
    fs::write(src.join("real_dir/inner.txt"), b"inside a real directory").unwrap();

    unixfs::symlink("real.txt", src.join("link_to_file")).unwrap();
    unixfs::symlink("real_dir", src.join("link_to_dir")).unwrap();
    unixfs::symlink("nowhere.txt", src.join("broken_link")).unwrap();
    src
}

#[test]
fn every_kind_of_symlink_is_reported_rather_than_dropped() {
    let dir = scratch("report");
    let src = build_tree(&dir);

    // Every writable format, because the walk is shared: `cram a out.zip` loses them exactly the
    // same way `cram a out.cram` does, and a warning that only fired for `.cram` would be worse
    // than none.
    for (name, fmt) in [
        ("out.cram", Format::cram(Codec::None)),
        ("out.zip", Format::zip()),
        ("out.tar", Format::tar(Codec::None)),
        ("out.7z", Format::sevenz()),
    ] {
        let arc = dir.join(name);
        let report = engine::create::create(
            &arc,
            fmt,
            std::slice::from_ref(&src),
            CreateOptions::default(),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("create {name}: {e}"));

        let mut got: Vec<&str> = report
            .skipped_links
            .iter()
            .map(|s| s.rsplit('/').next().unwrap())
            .collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["broken_link", "link_to_dir", "link_to_file"],
            "{name}: every skipped link must be named in the report, got {:?}",
            report.skipped_links
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A directory symlink must not be walked as though it were a real directory. Following it would
/// duplicate the target's whole subtree into the archive (what 7-Zip and WinRAR do, at a cost of
/// 8,011 files on the kernel tree) and would never terminate on a link cycle.
#[test]
fn a_directory_symlink_is_not_followed_into_the_archive() {
    let dir = scratch("nofollow");
    let src = build_tree(&dir);

    let arc = dir.join("out.cram");
    let report = engine::create::create(
        &arc,
        Format::cram(Codec::None),
        std::slice::from_ref(&src),
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();

    let reader = cram_core::formats::open(
        &arc,
        Format::cram(Codec::None),
        std::sync::Arc::new(cram_core::secret::NoPassword),
    )
    .unwrap();
    let names: Vec<String> = reader
        .as_random_access()
        .unwrap()
        .entries()
        .iter()
        .map(|e| e.name().to_string())
        .collect();
    drop(reader);

    assert!(
        !names.iter().any(|n| n.contains("link_to_dir")),
        "the link itself must not be archived: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("link_to_dir/")),
        "and its target's contents must not be archived through it: {names:?}"
    );
    // The real file behind the real directory is still there exactly once.
    assert_eq!(
        names.iter().filter(|n| n.ends_with("inner.txt")).count(),
        1,
        "the real directory is archived once and only once: {names:?}"
    );
    assert_eq!(report.skipped_links.len(), 3);
    let _ = fs::remove_dir_all(&dir);
}

/// A cycle of directory symlinks must terminate. This is the failure mode dereferencing invites, and
/// the reason "just follow them" is not the safe default it appears to be.
#[test]
fn a_symlink_cycle_terminates() {
    let dir = scratch("cycle");
    let src = dir.join("src");
    fs::create_dir_all(src.join("a")).unwrap();
    fs::write(src.join("a/file.txt"), b"payload").unwrap();
    // a/loop -> the directory that contains it.
    unixfs::symlink("..", src.join("a/loop")).unwrap();

    let arc = dir.join("out.cram");
    let report = engine::create::create(
        &arc,
        Format::cram(Codec::None),
        std::slice::from_ref(&src),
        CreateOptions::default(),
        &NullSink,
    )
    .expect("a symlink cycle must not hang or blow the stack");
    assert_eq!(report.skipped_links.len(), 1, "the loop link is reported");
    let _ = fs::remove_dir_all(&dir);
}

/// A symlink named directly on the command line is refused too. `fs::metadata` follows links, so
/// without an explicit check `cram a out.cram some-symlinked-dir` would archive the target's
/// contents as though they were the link's own.
#[test]
fn a_symlink_given_as_an_input_is_not_silently_followed() {
    let dir = scratch("input");
    let real = dir.join("real_dir");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("f.txt"), b"payload").unwrap();
    let link = dir.join("linked_dir");
    unixfs::symlink(&real, &link).unwrap();

    let arc = dir.join("out.cram");
    let report = engine::create::create(
        &arc,
        Format::cram(Codec::None),
        std::slice::from_ref(&link),
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();
    assert_eq!(
        report.skipped_links.len(),
        1,
        "a symlink input is reported, not walked as a real directory"
    );
    assert_eq!(report.entries, 0, "and nothing was archived through it");
    let _ = fs::remove_dir_all(&dir);
}
