//! Dedup-savings estimate: run the `.cram` content-defined chunker + BLAKE3 dedup accounting over a
//! set of inputs WITHOUT compressing or writing anything, to answer "how much of this is duplicate
//! content that `.cram` would store once?" — the cross-file dedup a zip/7z can't do.
//!
//! It reports the DEDUP saving only, never a compressed-size guess, so the number is honest and never
//! overstated: it is exactly the bytes a real `.cram` create would eliminate by dedup, because it uses
//! the same chunk sizing as the writer ([`CHUNK_MIN`]/[`CHUNK_AVG`]/[`CHUNK_MAX`]).

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use fastcdc::v2020::StreamCDC;

use crate::error::{ArchiveError, Result};
use crate::formats::cram::{CHUNK_AVG, CHUNK_MAX, CHUNK_MIN};
use crate::progress::ProgressSink;

/// The outcome of a dedup scan.
#[derive(Clone, Copy, Debug, Default)]
pub struct DedupEstimate {
    /// Total logical bytes across all scanned files.
    pub total_bytes: u64,
    /// Bytes remaining after content-defined dedup (unique chunk content).
    pub unique_bytes: u64,
    /// Number of files scanned.
    pub files: u64,
}

impl DedupEstimate {
    /// Bytes eliminated by cross-file dedup — what `.cram` stores once and a zip/7z stores per copy.
    pub fn saved(&self) -> u64 {
        self.total_bytes.saturating_sub(self.unique_bytes)
    }
}

/// Chunk + hash every input file, accounting duplicate chunks across the whole set. Bytes read are
/// reported to `sink` (so a GUI/CLI can show progress), and the scan stops early with
/// [`ArchiveError::Cancelled`] the moment the sink is cancelled. Unreadable files are skipped rather
/// than aborting the whole estimate.
pub fn estimate_dedup(inputs: &[PathBuf], sink: &dyn ProgressSink) -> Result<DedupEstimate> {
    let mut files = Vec::new();
    for p in inputs {
        collect_files(p, &mut files)?;
    }

    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut est = DedupEstimate::default();
    for path in files {
        if sink.is_cancelled() {
            return Err(ArchiveError::Cancelled);
        }
        sink.wait_if_paused();
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => continue, // unreadable now: skip it, keep estimating the rest
        };
        est.files += 1;
        let chunker = StreamCDC::new(file, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX);
        for chunk in chunker {
            if sink.is_cancelled() {
                return Err(ArchiveError::Cancelled);
            }
            let data = match chunk {
                Ok(c) => c.data,
                Err(_) => break, // read error mid-file: stop this file, keep what we have
            };
            let n = data.len() as u64;
            est.total_bytes += n;
            sink.on_bytes(n);
            if seen.insert(*blake3::hash(&data).as_bytes()) {
                est.unique_bytes += n;
            }
        }
    }
    Ok(est)
}

/// Recursively collect regular files under `path` (a file yields itself; a directory yields its file
/// tree). Symlinks are not followed — the same rule the create side uses, so the estimate matches what
/// a real create would actually archive.
fn collect_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        let mut children: Vec<_> = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            collect_files(&child.path(), out)?;
        }
    } else if meta.is_file() {
        out.push(path.to_path_buf());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Progress;
    use std::io::Write;

    #[test]
    fn two_identical_files_dedup_to_one_copy() {
        let dir = std::env::temp_dir().join(format!("cram_estimate_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Two identical files, each well above CHUNK_MAX so they split into several chunks.
        let blob = vec![0x5Au8; 700 * 1024];
        for name in ["a.bin", "b.bin"] {
            File::create(dir.join(name)).unwrap().write_all(&blob).unwrap();
        }

        let sink = Progress::new(0, 0);
        let est = estimate_dedup(std::slice::from_ref(&dir), &sink).unwrap();

        assert_eq!(est.files, 2);
        assert_eq!(est.total_bytes, 2 * blob.len() as u64);
        // The whole second file is duplicate content: unique ≈ one copy, saved ≈ one copy
        // (allow one chunk of slack for content-defined boundary effects).
        let one = blob.len() as u64;
        let slack = CHUNK_MAX as u64;
        assert!(est.unique_bytes <= one + slack, "unique {} should be ~one copy", est.unique_bytes);
        assert!(est.saved() >= one - slack, "saved {} should be ~one copy", est.saved());
        let _ = fs::remove_dir_all(&dir);
    }
}
