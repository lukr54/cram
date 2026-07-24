//! Sequential-format archives (tar, 7z) made mountable via the decode-to-cache adapter.
//!
//! These drive the *exact* seam the ProjFS mount uses — [`formats::open_random_access`] — on formats
//! that have no native random-access ability, then check the resulting [`RandomAccessReader`] serves
//! every entry's bytes correctly (whole-entry `copy_entry`, and arbitrary `read_range` slices,
//! including out-of-range requests that must clamp/empty rather than panic).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::formats;
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-seqcache-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A fixed tree with a mix of sizes so range math is exercised across small and multi-KiB entries.
fn make_sources(root: &Path) -> Vec<(String, Vec<u8>)> {
    fs::create_dir_all(root.join("data/sub")).unwrap();
    let readme = b"seqcache mount readme ".repeat(50);
    let blob: Vec<u8> = (0..40_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let nested = b"deeply nested content marker ".repeat(7);

    fs::write(root.join("data/readme.txt"), &readme).unwrap();
    fs::write(root.join("data/blob.bin"), &blob).unwrap();
    fs::write(root.join("data/sub/nested.txt"), &nested).unwrap();

    vec![
        ("data/readme.txt".into(), readme),
        ("data/blob.bin".into(), blob),
        ("data/sub/nested.txt".into(), nested),
    ]
}

/// Create an archive of `fmt`, open it through the mount seam, and verify random access matches source.
fn check_random_access(tag: &str, fmt: Format, file_name: &str) {
    let dir = scratch(tag);
    let sources = make_sources(&dir);
    let archive = dir.join(file_name);

    engine::create::create(
        &archive,
        fmt,
        &[dir.join("data")],
        CreateOptions::default(),
        &NullSink,
    )
    .expect("create archive");

    let reader = formats::open_random_access(&archive, fmt, Arc::new(NoPassword))
        .expect("open_random_access must accept a sequential format via the cache adapter");

    // Index every file entry by its path so we can match against the known source bytes.
    let entries = reader.entries().to_vec();
    for (rel, content) in &sources {
        let idx = entries
            .iter()
            .position(|e| e.is_file() && e.path.safe() == Path::new(rel))
            .unwrap_or_else(|| {
                panic!(
                    "{file_name}: entry {rel} not found in {:?}",
                    entries.iter().map(|e| e.path.raw()).collect::<Vec<_>>()
                )
            });

        // Reported size must equal what we can serve.
        assert_eq!(
            entries[idx].size as usize,
            content.len(),
            "{file_name}: size for {rel}"
        );

        // Whole entry via copy_entry.
        let mut whole = Vec::new();
        let n = reader.copy_entry(idx, &mut whole).expect("copy_entry");
        assert_eq!(n as usize, content.len());
        assert_eq!(&whole, content, "{file_name}: copy_entry bytes for {rel}");

        // Whole entry via read_range(0, size).
        let full = reader
            .read_range(idx, 0, content.len() as u64)
            .expect("read_range full");
        assert_eq!(&full, content, "{file_name}: read_range full for {rel}");

        // A mid-stream slice.
        if content.len() >= 20 {
            let slice = reader.read_range(idx, 5, 10).expect("read_range slice");
            assert_eq!(
                &slice,
                &content[5..15],
                "{file_name}: read_range slice for {rel}"
            );
        }

        // Out-of-range / boundary requests must clamp or return empty — never panic.
        assert!(reader
            .read_range(idx, content.len() as u64, 10)
            .unwrap()
            .is_empty());
        assert!(reader.read_range(idx, 0, 0).unwrap().is_empty());
        let tail = reader
            .read_range(idx, content.len() as u64 - 3, 999)
            .expect("clamped tail");
        assert_eq!(
            &tail,
            &content[content.len() - 3..],
            "{file_name}: clamped tail for {rel}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tar_is_mountable_via_decode_cache() {
    check_random_access("tar", Format::tar(Codec::None), "archive.tar");
}

#[test]
fn tar_gz_is_mountable_via_decode_cache() {
    check_random_access("targz", Format::tar(Codec::Gzip), "archive.tar.gz");
}

#[test]
fn sevenz_is_mountable_via_decode_cache() {
    check_random_access("7z", Format::sevenz(), "archive.7z");
}
