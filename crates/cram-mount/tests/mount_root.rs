//! What a mount is allowed to do to the folder it is pointed at. ProjFS has no unmark call, so
//! unmounting can only clear the root's reparse tag by deleting the folder; that is safe exactly as
//! long as the mount refuses any folder it did not create empty. These tests pin both halves,
//! because a guard that only refuses is as broken as one that only deletes.
//!
//! Windows-only, and skipped where the optional `Client-ProjFS` feature is off: the refusal lives
//! behind the availability check, so without ProjFS there is nothing here to observe.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

/// A scratch directory unique to the test, removed on the way in.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-mount-it-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A small `.cram` holding `payload/hello.txt`.
fn tiny_archive(dir: &Path) -> PathBuf {
    let src = dir.join("payload");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("hello.txt"), b"archive content").unwrap();
    let archive = dir.join("tiny.cram");
    engine::create::create(
        &archive,
        Format::cram(Codec::None),
        &[src],
        CreateOptions::default(),
        &NullSink,
    )
    .unwrap();
    archive
}

#[test]
fn mount_refuses_a_root_that_holds_files_and_leaves_them_untouched() {
    if !cram_mount::available() {
        return;
    }
    let dir = scratch("occupied");
    let archive = tiny_archive(&dir);

    // The user's own folder, with a file at the top and one a level down.
    let root = dir.join("victim");
    fs::create_dir_all(root.join("keepme")).unwrap();
    fs::write(root.join("IMPORTANT.txt"), b"do not delete me").unwrap();
    fs::write(root.join("keepme/nested.txt"), b"nor me").unwrap();

    let err = cram_mount::mount(&archive, &root, Arc::new(NoPassword))
        .err()
        .expect("mounting over a folder with contents must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("victim"),
        "the error must name the folder: {msg}"
    );

    // Nothing was hidden, moved or deleted.
    assert_eq!(
        fs::read(root.join("IMPORTANT.txt")).unwrap(),
        b"do not delete me"
    );
    assert_eq!(fs::read(root.join("keepme/nested.txt")).unwrap(), b"nor me");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn mount_serves_a_root_it_created_and_removes_it_on_unmount() {
    if !cram_mount::available() {
        return;
    }
    let dir = scratch("created");
    let archive = tiny_archive(&dir);
    let root = dir.join("fresh/mnt");

    let m = cram_mount::mount(&archive, &root, Arc::new(NoPassword)).expect("mount");
    assert_eq!(
        fs::read_to_string(root.join("payload/hello.txt")).unwrap(),
        "archive content",
        "the archive must be readable through the mount"
    );
    drop(m);
    assert!(
        !root.exists(),
        "a root the mount created is removed on unmount"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_root_the_user_made_survives_unmount_and_remounts() {
    if !cram_mount::available() {
        return;
    }
    let dir = scratch("borrowed");
    let archive = tiny_archive(&dir);

    // An empty folder the user created. Nothing is browsed, so nothing hydrates into it.
    let root = dir.join("mine");
    fs::create_dir_all(&root).unwrap();

    let m = cram_mount::mount(&archive, &root, Arc::new(NoPassword)).expect("first mount");
    drop(m);
    assert!(root.is_dir(), "a folder the user made outlives the mount");

    // It still carries the reparse tag `PrjStopVirtualizing` cannot remove, which is what would
    // otherwise make it unmountable for good. Empty and tagged is the one case a mount may clear.
    let m = cram_mount::mount(&archive, &root, Arc::new(NoPassword))
        .expect("a folder left tagged by an earlier mount must be mountable again");
    assert_eq!(
        fs::read_to_string(root.join("payload/hello.txt")).unwrap(),
        "archive content"
    );
    drop(m);

    let _ = fs::remove_dir_all(&dir);
}
