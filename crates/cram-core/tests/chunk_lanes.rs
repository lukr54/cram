//! **How many files are chunked at once must not reach the archive.**
//!
//! `.cram` chunks entries on a pool of workers, because FastCDC boundary search, BLAKE3 and the
//! Lepton pass are pure functions of one file's bytes and nothing about them depends on what came
//! before. Everything that *does* depend on order stays on one thread: which chunk is the first
//! occurrence of its hash, the id that gives it, the pack it lands in, and the byte accounting.
//!
//! That split is only legitimate if it is invisible. The lane counts come from the machine -- cores
//! and available RAM -- so two machines building the same folder take genuinely different paths
//! through the pipeline and must still agree byte for byte, or `.cram` stops being
//! content-addressable and a signed archive stops being reproducible on a rebuild.
//!
//! The trap this is really watching for is dedup order. A chunk that is a *hit* at one lane count
//! and a *miss* at another would still produce a valid, extractable archive with the same files in
//! it, just with different ids, different pack contents and a different length. Nothing else in the
//! suite would notice, which is exactly how the `--store` and Lepton bugs survived.
//!
//! Its own binary because it sets environment variables, which are process-wide: sharing a binary
//! with anything else that creates archives would let it change their results mid-run.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::{CreateOptions, Level};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-lanes-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn pseudo(seed: u32, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = 0x1234_5678u32 ^ seed.wrapping_mul(0x9E37_79B9);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        v.push((x >> 24) as u8);
    }
    v
}

/// A tree shaped to exercise every path the pipeline has, in one build:
///
/// * nested directories interleaved with files, so entry order mixes the two kinds
/// * **duplicates, and duplicates of a later file placed earlier**, which is the whole dedup-order
///   question: whichever copy the commit stage sees first is the one that gets stored
/// * files far larger than one worker's buffer, so a worker blocks mid-entry and the backpressure
///   path is taken rather than the everything-fits path
/// * a real JPEG, which takes the Lepton branch inside the worker
/// * an empty file, which produces no chunks at all
fn build_tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    let deep = src.join("nested").join("deeper");
    fs::create_dir_all(&deep).unwrap();
    fs::create_dir_all(src.join("empty-dir")).unwrap();

    let shared = pseudo(7, 300_000);
    // Written before its twin below, and again after, so a commit stage running out of order would
    // pick a different first occurrence and store a different copy.
    fs::write(src.join("a-twin.bin"), &shared).unwrap();
    for i in 0..16u32 {
        fs::write(
            src.join(format!("f{i:02}.bin")),
            pseudo(i, 40_000 + i as usize * 3_000),
        )
        .unwrap();
    }
    fs::write(src.join("m-twin.bin"), &shared).unwrap();
    fs::write(deep.join("z-twin.bin"), &shared).unwrap();

    // Two files whose tails match: shared chunks without whole-file identity.
    let mut head_a = pseudo(101, 300_000);
    let mut head_b = pseudo(202, 250_000);
    let tail = pseudo(303, 700_000);
    head_a.extend_from_slice(&tail);
    head_b.extend_from_slice(&tail);
    fs::write(src.join("tail-a.bin"), &head_a).unwrap();
    fs::write(deep.join("tail-b.bin"), &head_b).unwrap();

    // Tens of chunks, so it is well past the small buffers below and its worker fills up and waits
    // on the commit stage rather than running to the end of the file uninterrupted.
    fs::write(src.join("big.bin"), pseudo(999, 3_000_000)).unwrap();
    fs::write(src.join("empty.bin"), b"").unwrap();
    fs::write(
        src.join("text.txt"),
        "a line that repeats and repeats\n".repeat(10_000),
    )
    .unwrap();
    fs::write(
        deep.join("photo.jpg"),
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.jpg")).unwrap(),
    )
    .unwrap();
    src
}

/// Build once at the given lane counts. Safe to set the environment here: this binary runs its
/// tests one after another and nothing else in the process reads these variables concurrently.
fn build_with_lanes(
    src: &Path,
    out: &Path,
    workers: usize,
    buffer: usize,
    level: Level,
) -> Vec<u8> {
    std::env::set_var("CRAM_CHUNK_WORKERS", workers.to_string());
    std::env::set_var("CRAM_CHUNK_DEPTH", workers.to_string());
    std::env::set_var("CRAM_CHUNK_BUFFER", buffer.to_string());
    let r = engine::create::create(
        out,
        Format::cram(Codec::None),
        std::slice::from_ref(&src.to_path_buf()),
        CreateOptions {
            level,
            ..Default::default()
        },
        &NullSink,
    );
    std::env::remove_var("CRAM_CHUNK_WORKERS");
    std::env::remove_var("CRAM_CHUNK_DEPTH");
    std::env::remove_var("CRAM_CHUNK_BUFFER");
    r.unwrap_or_else(|e| panic!("create at {workers} workers: {e}"));
    fs::read(out).unwrap()
}

#[test]
fn lane_count_does_not_change_the_archive() {
    let dir = scratch("inv");
    let src = build_tree(&dir);

    // Every level, because each picks a different pack target, so the pack boundaries fall in
    // different places relative to the entries and a mis-ordered commit shows up differently.
    for level in [Level::Fastest, Level::Auto, Level::Best] {
        // One worker with a one-chunk buffer is the fully serialised case and the reference; the
        // rest run genuinely wide, with buffers small enough that workers block mid-entry.
        let lanes = [(1usize, 1usize), (2, 2), (4, 3), (16, 64)];
        let mut built: Vec<((usize, usize), Vec<u8>)> = Vec::new();
        for (workers, buffer) in lanes {
            let out = dir.join(format!("{level:?}-{workers}x{buffer}.cram"));
            built.push((
                (workers, buffer),
                build_with_lanes(&src, &out, workers, buffer, level),
            ));
        }
        let (first_lanes, first) = &built[0];
        assert!(!first.is_empty(), "{level:?}: archive is non-empty");
        for (lanes, bytes) in &built[1..] {
            assert_eq!(
                first,
                bytes,
                "{level:?}: lanes {first_lanes:?} and lanes {lanes:?} produced different archives \
                 ({} vs {} bytes). Lanes are chosen from the machine's cores and RAM, so this \
                 would make the same folder compress differently on different hardware and break \
                 both content-addressing and signature verification of a rebuild.",
                first.len(),
                bytes.len(),
            );
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_file_survives_the_pipeline() {
    let dir = scratch("rt");
    let src = build_tree(&dir);
    let arc = dir.join("wide.cram");
    build_with_lanes(&src, &arc, 8, 4, Level::Auto);

    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    engine::extract(
        &arc,
        &out,
        Arc::new(NoPassword),
        Default::default(),
        &NullSink,
    )
    .unwrap_or_else(|e| panic!("extract: {e}"));

    // Byte-identical, every file, including the JPEG that went through Lepton and back.
    let mut checked = 0;
    for entry in walkdir(&src) {
        let rel = entry.strip_prefix(&src).unwrap();
        let got = out.join("src").join(rel);
        assert!(
            got.is_file(),
            "{} missing from the extraction",
            rel.display()
        );
        assert_eq!(
            fs::read(&entry).unwrap(),
            fs::read(&got).unwrap(),
            "{} came back different",
            rel.display()
        );
        checked += 1;
    }
    assert!(checked >= 24, "expected the whole tree, checked {checked}");
    let _ = fs::remove_dir_all(&dir);
}

/// **An abandoned create must not hang.**
///
/// Dropping the writer without finishing is the normal end of a cancelled job, an unreadable source
/// or a full disk. At that moment workers are typically blocked sending into output channels that
/// are full, and the only thing that can release them is dropping the receivers -- which live in the
/// writer's pending queue. If the pool is joined before that queue is dropped, the join waits on a
/// worker waiting on the join, and the process stops for good with no error and no output.
///
/// Rust drops fields in declaration order, so this is settled by where `pending` sits in the struct
/// relative to `prep`, which is not the kind of thing that survives an unrelated edit unwatched. The
/// watchdog is here so a regression fails in ten seconds instead of hanging CI until it is killed.
#[test]
fn abandoning_a_create_does_not_hang() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = scratch("abandon");
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    // Far more files than lanes, each far larger than the one-chunk buffer, so by the time the
    // writer is dropped every worker is mid-file with a full channel in front of it.
    for i in 0..64u32 {
        fs::write(src.join(format!("w{i:02}.bin")), pseudo(i, 2_000_000)).unwrap();
    }

    let (tx, rx) = mpsc::channel();
    let arc = dir.join("abandoned.cram");
    let handle = std::thread::spawn(move || {
        std::env::set_var("CRAM_CHUNK_WORKERS", "8");
        std::env::set_var("CRAM_CHUNK_DEPTH", "8");
        std::env::set_var("CRAM_CHUNK_BUFFER", "1");
        let mut writer =
            cram_core::formats::create(&arc, Format::cram(Codec::None), &CreateOptions::default())
                .unwrap();
        for (i, path) in walkdir(&src).into_iter().enumerate() {
            let entry = cram_core::Entry {
                index: i,
                path: cram_core::EntryPath::from_raw(&path.file_name().unwrap().to_string_lossy())
                    .unwrap(),
                kind: cram_core::EntryKind::File,
                size: fs::metadata(&path).unwrap().len(),
                compressed_size: None,
                modified: None,
                unix_mode: None,
                crc32: None,
                encrypted: false,
            };
            // Stop part-way, exactly as a cancel does: the queue is full of work nobody will
            // collect.
            if writer.add_path(&entry, &path, Default::default()).is_err() {
                break;
            }
            if i >= 24 {
                break;
            }
        }
        drop(writer);
        let _ = tx.send(());
    });

    assert!(
        rx.recv_timeout(Duration::from_secs(60)).is_ok(),
        "dropping a half-finished writer deadlocked: the chunk pool was joined before the pending \
         queue that holds its receivers was dropped. Check the field order in CramArchiveWriter."
    );
    handle.join().unwrap();
    std::env::remove_var("CRAM_CHUNK_WORKERS");
    std::env::remove_var("CRAM_CHUNK_DEPTH");
    std::env::remove_var("CRAM_CHUNK_BUFFER");
    let _ = fs::remove_dir_all(&dir);
}

fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}
