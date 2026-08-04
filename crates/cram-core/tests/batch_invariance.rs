//! **How many packs compress at once must not reach the archive.**
//!
//! This is the property the create path's memory story rests on. Pack size is fixed by the effort
//! level, so it travels with the archive and the same inputs always give the same bytes; the
//! machine's constraint is answered instead by `hw::create_batch`, which picks how many packs are in
//! flight from available RAM. That split is only legitimate because a differently-sized batch
//! produces an identical file. Once batch depends on installed memory, two machines building the
//! same folder take different paths through `flush_batch` and must still agree byte for byte, or
//! `.cram` stops being content-addressable and a signed archive stops being reproducible.
//!
//! `reproducible.rs` cannot cover this: it builds twice in one process on one machine, so batch is
//! constant across its builds. This lives in its own test binary because it sets `CRAM_BATCH`, and
//! an environment variable is process-wide, so sharing a binary with tests that also create archives
//! would let it change their results mid-run.
//!
//! Also confirmed outside the test suite, on the 1.6 GB kernel tree at 32 MiB packs: batches of 16,
//! 8 and 4 each produced exactly 164,607,029 bytes while peak RSS fell 4952 -> 3251 -> 1734 MB.

use std::fs;
use std::path::{Path, PathBuf};

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::writer::{CreateOptions, Level};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-batchinv-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Enough distinct, incompressible data to fill several packs at any level's pack target, so a
/// batch boundary genuinely falls inside the build instead of every pack landing in one flush.
fn build_inputs(root: &Path, files: u32, each: usize) -> PathBuf {
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    for f in 0..files {
        let mut v = Vec::with_capacity(each);
        let mut x = 0x1234_5678u32 ^ f.wrapping_mul(0x9E37_79B9);
        for _ in 0..each {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push((x >> 24) as u8);
        }
        fs::write(src.join(format!("b{f}.bin")), &v).unwrap();
    }
    src
}

fn build_with_batch(src: &Path, out: &Path, batch: usize, level: Level) -> Vec<u8> {
    // Safe here: this binary holds exactly one test, so nothing else in the process reads the
    // environment concurrently. That isolation is why the test lives in its own file.
    std::env::set_var("CRAM_BATCH", batch.to_string());
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
    std::env::remove_var("CRAM_BATCH");
    r.unwrap_or_else(|e| panic!("create at batch {batch}: {e}"));
    fs::read(out).unwrap()
}

#[test]
fn batch_size_does_not_change_the_archive() {
    let dir = scratch("inv");
    let src = build_inputs(&dir, 10, 600_000);

    // Every level, because each picks a different pack target and the batch boundary therefore
    // falls in a different place relative to the packs.
    for level in [Level::Fastest, Level::Auto, Level::Best] {
        let mut built: Vec<(usize, Vec<u8>)> = Vec::new();
        for batch in [1usize, 3, 16] {
            let out = dir.join(format!("{level:?}-{batch}.cram"));
            built.push((batch, build_with_batch(&src, &out, batch, level)));
        }
        let (first_n, first) = &built[0];
        assert!(!first.is_empty(), "{level:?}: archive is non-empty");
        for (n, bytes) in &built[1..] {
            assert_eq!(
                first,
                bytes,
                "{level:?}: batch {first_n} and batch {n} produced different archives \
                 ({} vs {} bytes). Batch is chosen from the machine's RAM, so this would make the \
                 same inputs compress differently on different hardware and break both \
                 content-addressing and signature verification of a rebuild.",
                first.len(),
                bytes.len(),
            );
        }
    }
    let _ = fs::remove_dir_all(&dir);
}
