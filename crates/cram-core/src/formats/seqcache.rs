//! `SeqCacheReader`, mount a sequential-only archive (tar / 7z / rar / raw single-stream) by decoding
//! it **once** into a bounded in-memory cache, then serving random-access reads from RAM.
//!
//! tar, 7z, rar and raw single-stream codecs are front-to-back streams with no random-access hand-off point, so
//! unlike ZIP / `.cram` / ISO they can't directly back a ProjFS mount. This adapter bridges the gap:
//! at open time it drains every entry body into memory (up to `MOUNT_CACHE_CAP`), then implements
//! [`RandomAccessReader`] over those buffers. It is the simple option, the whole
//! *uncompressed* archive must fit under the cap, but it makes every readable format mountable through
//! the exact same boundary as the natively-seekable ones.
//!
//! Because all decoding finishes *before* the adapter is returned (the underlying backend is opened,
//! fully drained, and dropped inside [`SeqCacheReader::decode`]), the resulting reader owns only
//! `Vec`/`Arc` buffers and is therefore `Send + Sync`, even for a backend like RAR whose native
//! handle is neither. That is what lets the parallel mount callbacks fan out over it safely.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::model::Entry;
use crate::reader::{EntryStream, RandomAccessReader};
use crate::secret::PasswordProvider;

/// Upper bound on total *uncompressed* bytes held in RAM for a mounted sequential archive. Above this,
/// the mount is refused (with a "extract instead" error) rather than exhausting memory.
const MOUNT_CACHE_CAP: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

pub struct SeqCacheReader {
    entries: Vec<Entry>,
    /// Decoded body per entry, aligned to `entries` (directories carry an empty buffer). `Arc` so a
    /// clone is cheap if a caller ever needs to share a body.
    bodies: Vec<Arc<Vec<u8>>>,
}

impl SeqCacheReader {
    /// Decode `path` (a sequential archive of `fmt`) fully into memory, ready for random access.
    pub fn decode(path: &Path, fmt: Format, pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        Self::decode_with_cap(path, fmt, pw, MOUNT_CACHE_CAP)
    }

    fn decode_with_cap(
        path: &Path,
        fmt: Format,
        pw: Arc<dyn PasswordProvider>,
        cap: u64,
    ) -> Result<Self> {
        let mut reader = super::open(path, fmt, pw)?;
        let mut entries: Vec<Entry> = Vec::new();
        let mut bodies: Vec<Arc<Vec<u8>>> = Vec::new();
        let mut total: u64 = 0;

        while let Some(es) = reader.next_entry()? {
            // Destructure so `entry` (owned) is free of the `body` borrow on `reader`; the body must be
            // fully drained before the next `next_entry()` call (the trait contract), which we do here.
            let EntryStream {
                mut entry,
                mut body,
                ..
            } = es;

            let mut buf = Vec::new();
            if !entry.is_dir() {
                // Read at most the remaining budget (+1 byte to detect an overflowing entry), so a
                // single huge member can't blow the cap before the running-total check below.
                // `saturating_add` guards the degenerate case of a near-`u64::MAX` cap.
                let remaining = cap.saturating_sub(total);
                body.by_ref()
                    .take(remaining.saturating_add(1))
                    .read_to_end(&mut buf)?;
            }
            drop(body); // release the &mut borrow of `reader` before looping

            // Charge BOTH the body bytes and the per-entry metadata (name string + Entry + Arc
            // bookkeeping) against the cap. Counting body bytes alone let an archive of millions of
            // zero-length entries (or entries with megabyte-long PAX names) grow `entries`/`bodies`
            // without ever tripping the cap, an OOM from a ~50 MB hostile file.
            const PER_ENTRY_OVERHEAD: u64 = 512;
            total = total
                .saturating_add(buf.len() as u64)
                .saturating_add(entry.path.raw().len() as u64)
                .saturating_add(PER_ENTRY_OVERHEAD);
            if total > cap {
                return Err(ArchiveError::Backend(format!(
                    "archive is larger than {} MiB uncompressed, too large to mount in memory; extract it instead",
                    cap / (1024 * 1024)
                )));
            }

            // Reindex to the cache position and pin the reported size to what we can actually serve, so
            // the mount never asks for bytes the cache doesn't hold (which would be a short read).
            entry.index = entries.len();
            if !entry.is_dir() {
                entry.size = buf.len() as u64;
            }
            entries.push(entry);
            bodies.push(Arc::new(buf));
        }

        Ok(Self { entries, bodies })
    }
}

impl RandomAccessReader for SeqCacheReader {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn copy_entry(&self, index: usize, out: &mut dyn Write) -> Result<u64> {
        let body = self
            .bodies
            .get(index)
            .ok_or_else(|| ArchiveError::Corrupt("bad entry index".into()))?;
        out.write_all(body)?;
        Ok(body.len() as u64)
    }

    fn read_range(&self, index: usize, off: u64, len: u64) -> Result<Vec<u8>> {
        let body = self
            .bodies
            .get(index)
            .ok_or_else(|| ArchiveError::Corrupt("bad entry index".into()))?;
        let size = body.len() as u64;
        if off >= size || len == 0 {
            return Ok(Vec::new());
        }
        let end = off.saturating_add(len).min(size);
        Ok(body[off as usize..end as usize].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Codec;
    use crate::progress::NullSink;
    use crate::secret::NoPassword;
    use crate::writer::CreateOptions;
    use std::fs;

    /// Build a small real `.tar` on disk and return its path (kept alive by the returned dir). The
    /// `tag` keeps concurrently-running tests in distinct directories (cargo runs them in parallel).
    fn tiny_tar(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("cram-seqcache-ut-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("d")).unwrap();
        fs::write(dir.join("d/a.txt"), b"hello cache".repeat(10)).unwrap();
        fs::write(dir.join("d/b.txt"), b"second file").unwrap();
        let tar = dir.join("t.tar");
        crate::engine::create::create(
            &tar,
            Format::tar(Codec::None),
            &[dir.join("d")],
            CreateOptions::default(),
            &NullSink,
        )
        .unwrap();
        (dir, tar)
    }

    #[test]
    fn decodes_a_real_tar_into_cache() {
        let (dir, tar) = tiny_tar("decode");
        let r =
            SeqCacheReader::decode(&tar, Format::tar(Codec::None), Arc::new(NoPassword)).unwrap();
        let files: Vec<_> = r.entries().iter().filter(|e| e.is_file()).collect();
        assert_eq!(files.len(), 2, "two files cached");
        // Every file's reported size equals the servable buffer length.
        for e in &files {
            let got = r.read_range(e.index, 0, e.size).unwrap();
            assert_eq!(got.len() as u64, e.size);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_metadata_flood_of_empty_entries() {
        // Zero-length entries carry no body bytes, so the old body-only accounting never tripped
        // the cap no matter how many entries streamed in. The per-entry overhead charge must
        // refuse such an archive once entry metadata alone exceeds the budget.
        let dir = std::env::temp_dir().join(format!("cram-seqcache-flood-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("d")).unwrap();
        for i in 0..64 {
            fs::write(dir.join(format!("d/empty-{i:03}.txt")), b"").unwrap();
        }
        let tar = dir.join("flood.tar");
        crate::engine::create::create(
            &tar,
            Format::tar(Codec::None),
            &[dir.join("d")],
            CreateOptions::default(),
            &NullSink,
        )
        .unwrap();
        // 64 empty files (0 body bytes) against a 4 KiB cap: metadata overhead alone must refuse.
        let err = SeqCacheReader::decode_with_cap(
            &tar,
            Format::tar(Codec::None),
            Arc::new(NoPassword),
            4096,
        );
        assert!(
            err.is_err(),
            "an entry-metadata flood must be refused even with zero body bytes"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_archive_over_the_cap() {
        let (dir, tar) = tiny_tar("cap");
        // The tar's uncompressed payload is ~120 bytes; a 16-byte cap must be refused, not OOM.
        let err = SeqCacheReader::decode_with_cap(
            &tar,
            Format::tar(Codec::None),
            Arc::new(NoPassword),
            16,
        );
        assert!(
            err.is_err(),
            "an archive larger than the cap must be refused"
        );
        // A generous cap succeeds.
        assert!(SeqCacheReader::decode_with_cap(
            &tar,
            Format::tar(Codec::None),
            Arc::new(NoPassword),
            1 << 20
        )
        .is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
