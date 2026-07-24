//! Skip-already-correct: on extract, an entry whose destination file already holds exactly this
//! content can be skipped entirely, the "write fewer bytes than the disk wall allows" win that
//! matters most on re-extracting over an existing tree.
//!
//! We only skip when a match can be **proven**: same size *and* same CRC32. Formats that carry a
//! per-entry CRC (ZIP, 7z) get the full win; those that don't (tar, raw, RAR today) report no match,
//! so extraction stays correct, we never skip on a guess. On the random-access ZIP path the check
//! runs before decode+write (saves both); on the sequential path it still saves the write (the wall).

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use flate2::Crc;

use crate::model::Entry;

/// Read window for hashing the existing file (never loads the whole file into memory).
const CRC_BUF: usize = 1 << 20; // 1 MiB

/// Does `dest` already contain exactly this entry's bytes? Requires a CRC to verify; without one
/// (or if `dest` is missing / a different size), returns `false` so the caller extracts.
pub(crate) fn dest_already_correct(dest: &Path, entry: &Entry) -> bool {
    let Some(want_crc) = entry.crc32 else {
        return false; // no CRC to verify against → can't prove a match
    };
    let Ok(meta) = fs::metadata(dest) else {
        return false; // missing / unreadable
    };
    if !meta.is_file() || meta.len() != entry.size {
        return false;
    }
    matches!(crc32_of(dest), Ok(got) if got == want_crc)
}

/// Streamed CRC32 (IEEE, the polynomial ZIP and 7z both use) of a file's contents.
fn crc32_of(path: &Path) -> std::io::Result<u32> {
    let mut file = File::open(path)?;
    let mut crc = Crc::new();
    let mut buf = vec![0u8; CRC_BUF];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        crc.update(&buf[..n]);
    }
    Ok(crc.sum())
}
