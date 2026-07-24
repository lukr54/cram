//! ZIP backend, the random-access fast path. Each entry is independently decodable, so the engine
//! extracts many at once, every worker opening its **own** file handle (see [`ZipReader::copy_entry`]);
//! the ceiling becomes the disk write wall, not one CPU core. It is rebound to cram-core's
//! `Entry`/`Report`/`PasswordProvider` and the centralized zip-slip guard.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use zip::read::ZipArchiveMetadata;
use zip::result::ZipError;
use zip::ZipArchive;

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream, RandomAccessReader};
use crate::secret::{PasswordProvider, PasswordRequest};

/// The CRC-32 a ZIP entry actually commits to, or `None` when the container stores none.
///
/// WinZip AES comes in two flavours. **AE-1** stores the real CRC-32 of the plaintext; **AE-2**
/// stores `0` and deliberately omits it, because the AES authentication code already proves the
/// data is intact and a plaintext CRC leaks information about short entries. Writers pick between
/// them per entry, the `zip` crate emits AE-2 for anything under 20 bytes, so a zero here is not a
/// checksum, it is the absence of one.
///
/// Taking that zero literally would make `cram t` recompute the CRC of a small encrypted entry,
/// compare it against `0`, and declare corrupt an archive **Cram itself has just written**. A false
/// "this archive is corrupt" is worse than no check at all: it is the one verdict a user cannot
/// safely ignore, so a missing checksum is reported as missing rather than compared against.
///
/// Only encrypted entries are treated this way. An unencrypted entry legitimately has CRC `0` when
/// it is empty, and there the comparison is trivially correct, so it is left alone.
fn stored_crc(crc: u32, encrypted: bool, size: u64) -> Option<u32> {
    if encrypted && crc == 0 && size > 0 {
        None // AE-2: no CRC stored; integrity rests on the AES authentication instead
    } else {
        Some(crc)
    }
}

/// A ZIP entry's stored modification time as a [`SystemTime`], or `None` when absent/unrepresentable.
/// ZIP's DOS time is local wall time at 2-second granularity with no zone; we read the calendar fields
/// and treat them as UTC (the highest fidelity DOS time supports), building the instant through the
/// `time` crate. Field accessors are used rather than a blanket `TryFrom` so this holds across zip
/// versions. `None` → extraction leaves the file's current time (never stamps a bogus one).
fn zip_mtime(dt: Option<zip::DateTime>) -> Option<SystemTime> {
    let dt = dt?;
    let month = time::Month::try_from(dt.month()).ok()?;
    let date = time::Date::from_calendar_date(dt.year() as i32, month, dt.day()).ok()?;
    let tod = time::Time::from_hms(dt.hour(), dt.minute(), dt.second()).ok()?;
    Some(SystemTime::from(date.with_time(tod).assume_utc()))
}

/// Translate a `zip` crate error into a typed [`ArchiveError`].
fn map_zip_err(e: ZipError) -> ArchiveError {
    match e {
        ZipError::Io(io) => ArchiveError::Io(io),
        ZipError::InvalidPassword => ArchiveError::WrongPassword,
        ZipError::UnsupportedArchive(m) if m == ZipError::PASSWORD_REQUIRED => {
            ArchiveError::PasswordRequired
        }
        ZipError::FileNotFound => ArchiveError::Corrupt("entry not found in archive".into()),
        ZipError::InvalidArchive(m) => ArchiveError::Corrupt(m.into_owned()),
        other => ArchiveError::Backend(other.to_string()),
    }
}

/// Scan the central directory once into cram-core `Entry`s. Entries whose names would escape the
/// output dir (zip-slip) are dropped here via [`EntryPath::from_raw`], they can't be listed or
/// extracted. `Entry::index` keeps the true archive index so `by_index` still resolves.
///
/// Uses `by_index_raw` (not `by_index`): raw access reads central-directory metadata, including the
/// `encrypted` flag, **without** preparing/decrypting the data stream, so an AES/ZipCrypto archive
/// can be listed without a password (WinZip AES leaves names in the clear). `by_index` would return
/// `PASSWORD_REQUIRED` here and make even listing an encrypted archive fail.
fn scan(path: &Path) -> Result<(Vec<Entry>, Arc<ZipArchiveMetadata>)> {
    let mut archive = ZipArchive::new(File::open(path)?).map_err(map_zip_err)?;
    let mut out = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let zf = archive.by_index_raw(i).map_err(map_zip_err)?;
        let Some(safe) = EntryPath::from_raw(zf.name()) else {
            continue; // zip-slip: refuse to list/extract traversal entries
        };
        let kind = if zf.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        out.push(Entry {
            index: i,
            path: safe,
            kind,
            size: zf.size(),
            compressed_size: Some(zf.compressed_size()),
            modified: zip_mtime(zf.last_modified()),
            unix_mode: zf.unix_mode(),
            crc32: stored_crc(zf.crc32(), zf.encrypted(), zf.size()),
            encrypted: zf.encrypted(),
        });
    }
    Ok((out, archive.metadata()))
}

/// A parsed ZIP. Holds only `Sync` state (path/entries/password) plus a lazily-opened archive for
/// the cold sequential path, so `&ZipReader` is a valid `RandomAccessReader` shared across workers.
pub struct ZipReader {
    path: PathBuf,
    name: String,
    entries: Vec<Entry>,
    /// The central directory, parsed ONCE at open and shared by every per-call handle. Without
    /// it, `copy_entry`/`read_range` would re-parse the whole CD on every call; O(n²) work across
    /// an n-entry extraction, and an O(n) parse per on-access read of a mounted ZIP.
    meta: Arc<ZipArchiveMetadata>,
    pw: Arc<dyn PasswordProvider>,
    /// Lazily opened only if the sequential `next_entry` path is used (random access re-opens
    /// its own handle per call instead).
    seq: Option<ZipArchive<File>>,
    cursor: usize,
}

impl ZipReader {
    pub fn open(path: &Path, pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        let (entries, meta) = scan(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            name: path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
            entries,
            meta,
            pw,
            seq: None,
            cursor: 0,
        })
    }

    /// A fresh archive handle over an independent `File`, reusing the once-parsed central
    /// directory. SAFETY (the crate's documented invariant for `unsafe_new_with_metadata`): the
    /// reader must view the same byte stream the metadata was parsed from, we re-open the same
    /// path, exactly as the crate's own multi-handle example does. If the file is swapped on disk
    /// mid-run, the stored offsets land on wrong bytes and decoding fails with a typed error /
    /// CRC mismatch, the same TOCTOU exposure any re-open-and-read design carries, never UB.
    fn handle(&self) -> Result<ZipArchive<File>> {
        let file = File::open(&self.path)?;
        Ok(unsafe { ZipArchive::unsafe_new_with_metadata(file, self.meta.clone()) })
    }

    /// Fetch the password for an encrypted entry, or fail with `PasswordRequired`.
    fn password_for(&self, entry: &Entry) -> Result<crate::secret::Secret> {
        self.pw
            .password(&PasswordRequest {
                archive: &self.name,
                entry: Some(entry.name()),
                for_header: false,
                attempt: 0,
            })
            .ok_or(ArchiveError::PasswordRequired)
    }
}

impl ArchiveReader for ZipReader {
    fn format(&self) -> Format {
        Format::zip()
    }

    fn entries(&self) -> Result<&[Entry]> {
        Ok(&self.entries)
    }

    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>> {
        if self.cursor >= self.entries.len() {
            return Ok(None);
        }
        let pos = self.cursor;
        self.cursor += 1;
        let entry = self.entries[pos].clone();
        let zip_idx = entry.index;

        // Resolve the password before borrowing the archive (keeps the borrows disjoint).
        let secret = if entry.encrypted {
            Some(self.password_for(&entry)?)
        } else {
            None
        };
        if self.seq.is_none() {
            self.seq = Some(self.handle()?);
        }
        let archive = self.seq.as_mut().unwrap();
        let zf = match &secret {
            Some(s) => archive
                .by_index_decrypt(zip_idx, s.expose().as_bytes())
                .map_err(map_zip_err)?,
            None => archive.by_index(zip_idx).map_err(map_zip_err)?,
        };
        Ok(Some(EntryStream {
            entry,
            body: Box::new(zf),
            meta_final: true,
        }))
    }

    fn as_random_access(&self) -> Option<&dyn RandomAccessReader> {
        Some(self)
    }
}

impl RandomAccessReader for ZipReader {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// `index` is the position in [`entries`](Self::entries). Opens its own archive handle, so this
    /// is safe to call from many workers at once.
    fn copy_entry(&self, index: usize, out: &mut dyn Write) -> Result<u64> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| ArchiveError::Backend(format!("entry index {index} out of range")))?;
        let zip_idx = entry.index;
        let mut archive = self.handle()?;
        let n = if entry.encrypted {
            let secret = self.password_for(entry)?;
            let mut zf = archive
                .by_index_decrypt(zip_idx, secret.expose().as_bytes())
                .map_err(map_zip_err)?;
            io::copy(&mut zf, out)?
        } else {
            let mut zf = archive.by_index(zip_idx).map_err(map_zip_err)?;
            io::copy(&mut zf, out)?
        };
        Ok(n)
    }

    /// Mount / on-access primitive: return the `[off, off+len)` window of the entry's *uncompressed*
    /// stream. DEFLATE has no random seek, so we decode from the start, but we **stream**: skip the
    /// leading `off` bytes through a small scratch buffer, copy out at most `len`, and stop, never
    /// materializing the whole entry. This bounds memory to the requested window, so a huge or
    /// zip-bombed entry cannot OOM the process hosting the mount when only a slice is read.
    /// Decoding the entry in full and slicing afterwards would instead make every mount read an
    /// unbounded allocation sized by the archive rather than by the caller's request, and once
    /// ZIP is mountable, that path is reachable from any read. Cost note: each call re-opens the
    /// archive and re-decodes from the entry start, so many small out-of-order reads are O(offset)
    /// each; the common mount patterns (copy / read-to-end) issue one large range and decode once.
    fn read_range(&self, index: usize, off: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| ArchiveError::Backend(format!("entry index {index} out of range")))?;
        let zip_idx = entry.index;
        let mut archive = self.handle()?;
        let mut zf = if entry.encrypted {
            let secret = self.password_for(entry)?;
            archive
                .by_index_decrypt(zip_idx, secret.expose().as_bytes())
                .map_err(map_zip_err)?
        } else {
            archive.by_index(zip_idx).map_err(map_zip_err)?
        };

        // Skip `off` bytes by decoding and discarding them (bounded scratch, never a full buffer).
        let mut scratch = [0u8; 64 * 1024];
        let mut skipped = 0u64;
        while skipped < off {
            let want = ((off - skipped).min(scratch.len() as u64)) as usize;
            let n = zf.read(&mut scratch[..want])?;
            if n == 0 {
                return Ok(Vec::new()); // offset at/after end of entry
            }
            skipped += n as u64;
        }

        // Copy out up to `len` bytes. Capacity is only a hint (capped at 1 MiB) so a caller passing a
        // huge `len` for a short entry can't force a huge up-front allocation; the Vec grows to the
        // real byte count, which the decoder bounds to the entry's actual size.
        let mut out = Vec::with_capacity((len as usize).min(1024 * 1024));
        let mut remaining = len;
        while remaining > 0 {
            let want = (remaining.min(scratch.len() as u64)) as usize;
            let n = zf.read(&mut scratch[..want])?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&scratch[..n]);
            remaining -= n as u64;
        }
        Ok(out)
    }
}
