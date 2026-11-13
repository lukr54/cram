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
use zip::{ExtraField, ZipArchive};

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream, RandomAccessReader};
use crate::secret::{PasswordProvider, PasswordRequest};

/// The CRC-32 a ZIP entry actually commits to, or `None` when the container stores none.
///
/// WinZip AES comes in two flavours. **AE-1** stores the real CRC-32 of the plaintext; **AE-2**
/// stores `0` and omits it, because the AES authentication code already proves the
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
///
/// Taken from the central header's Info-ZIP extended timestamp (`0x5455`, UTC seconds) when there
/// is one, else its NTFS field (`0x000A`), else its older Info-ZIP Unix field (`0x5855`), else the
/// DOS date-time read as **local** wall time, the way every other archiver reads it. The order and
/// the bounds are in [`super::zip_time`]. The first two come already parsed by the zip crate, which
/// rejects a malformed one when the archive is opened; only their values are checked here.
/// `0x5855` comes as raw bytes and is passed over when too short. `None` → extraction leaves the
/// file's current time (never stamps a bogus one).
fn zip_mtime<R: Read>(zf: &zip::read::ZipFile<'_, R>) -> Option<SystemTime> {
    let mut ext = None;
    let mut ntfs = None;
    let mut unix = None;
    for field in zf.extra_data_fields() {
        match field {
            ExtraField::ExtendedTimestamp(e) => ext = ext.or(e.mod_time()),
            ExtraField::Ntfs(n) => ntfs = ntfs.or(Some(n.mtime())),
            ExtraField::Custom(c) if c.header_id == super::zip_time::UNIX_TIME_ID => {
                unix = unix.or(super::zip_time::unix_time_mtime(&c.data));
            }
            _ => {}
        }
    }
    super::zip_time::mtime_for_read(zf.last_modified(), ext, ntfs, unix)
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
        let Some(safe) = EntryPath::from_raw(&zf.name().map_err(map_zip_err)?) else {
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
            modified: zip_mtime(&zf),
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

#[cfg(test)]
mod mtime_tests {
    //! Reading an entry's time from archives built byte by byte, so each one holds exactly the
    //! fields under test. None of these depends on the machine's zone: a DOS-only time is checked
    //! against the zone's own offset, whatever it is. That offset comes from the function under
    //! test, so in UTC these cannot tell local time from UTC; `localtime`'s tests rerun them under a
    //! fixed zone on Linux and macOS.

    use super::*;
    use crate::secret::NoPassword;
    use std::time::{Duration, UNIX_EPOCH};

    /// 2020-01-12T03:04:06Z, the time the extra fields carry. It is ten days from the DOS fields,
    /// so no zone offset (at most a day) can make a DOS reading land on it.
    const ELSEWHERE: u64 = 1_578_798_246;
    /// DOS fields for 2020-01-02 04:04:06, as (time, date).
    const DOS_TIME: u16 = (4 << 11) | (4 << 5) | (6 / 2);
    const DOS_DATE: u16 = ((2020 - 1980) << 9) | (1 << 5) | 2;
    /// Those fields read as UTC, in Unix seconds.
    const DOS_AS_UTC: i64 = 1_577_937_846;

    fn field(id: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = id.to_le_bytes().to_vec();
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A one-entry stored ZIP whose local and central headers both carry `extra`.
    fn one_entry_zip(extra: &[u8]) -> Vec<u8> {
        let name = b"a.txt";
        let data = b"timestamp fixture\n";
        let crc = crc32fast::hash(data);
        let mut z = Vec::new();
        // Local header.
        z.extend_from_slice(b"PK\x03\x04");
        z.extend_from_slice(&20u16.to_le_bytes()); // version needed
        z.extend_from_slice(&0u16.to_le_bytes()); // flags
        z.extend_from_slice(&0u16.to_le_bytes()); // stored
        z.extend_from_slice(&DOS_TIME.to_le_bytes());
        z.extend_from_slice(&DOS_DATE.to_le_bytes());
        z.extend_from_slice(&crc.to_le_bytes());
        z.extend_from_slice(&(data.len() as u32).to_le_bytes());
        z.extend_from_slice(&(data.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        z.extend_from_slice(name);
        z.extend_from_slice(extra);
        z.extend_from_slice(data);
        // Central header.
        let cd_start = z.len();
        z.extend_from_slice(b"PK\x01\x02");
        z.extend_from_slice(&20u16.to_le_bytes()); // version made by (MS-DOS)
        z.extend_from_slice(&20u16.to_le_bytes()); // version needed
        z.extend_from_slice(&0u16.to_le_bytes()); // flags
        z.extend_from_slice(&0u16.to_le_bytes()); // stored
        z.extend_from_slice(&DOS_TIME.to_le_bytes());
        z.extend_from_slice(&DOS_DATE.to_le_bytes());
        z.extend_from_slice(&crc.to_le_bytes());
        z.extend_from_slice(&(data.len() as u32).to_le_bytes());
        z.extend_from_slice(&(data.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes()); // comment length
        z.extend_from_slice(&0u16.to_le_bytes()); // disk number
        z.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        z.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        z.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        z.extend_from_slice(name);
        z.extend_from_slice(extra);
        let cd_len = z.len() - cd_start;
        // End of central directory.
        z.extend_from_slice(b"PK\x05\x06");
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&(cd_len as u32).to_le_bytes());
        z.extend_from_slice(&(cd_start as u32).to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z
    }

    /// Open `bytes` as a ZIP and return its one entry's time, or the open error. Each tag gets its
    /// own directory, removed before anything is asserted, so a failing check leaves nothing behind
    /// and tests running in parallel do not remove each other's files.
    fn read_mtime(tag: &str, bytes: &[u8]) -> Result<Option<SystemTime>> {
        let dir = std::env::temp_dir().join(format!("cram-zipmtime-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}.zip"));
        std::fs::write(&path, bytes).unwrap();
        let got = ZipReader::open(&path, Arc::new(NoPassword)).map(|r| {
            let entries = RandomAccessReader::entries(&r);
            (entries.len(), entries.first().and_then(|e| e.modified))
        });
        let _ = std::fs::remove_dir_all(&dir);
        got.map(|(count, modified)| {
            assert_eq!(count, 1, "{tag}: entry count");
            modified
        })
    }

    fn unix(t: SystemTime) -> i64 {
        i64::try_from(t.duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap()
    }

    /// The DOS fields of `t` in this machine's zone must be the fixture's fields: `t` plus the
    /// offset at `t` is the wall time the fixture holds. With no offset available that is UTC.
    fn assert_is_local_reading(tag: &str, got: Option<SystemTime>) {
        let t = unix(got.unwrap_or_else(|| panic!("{tag}: no time read")));
        let off = i64::from(crate::localtime::utc_offset_at(t).unwrap_or(0));
        assert_eq!(
            t + off,
            DOS_AS_UTC,
            "{tag}: read {t} with offset {off}; the DOS fields were not read as local time"
        );
    }

    #[test]
    fn dos_only_entry_reads_as_local_time() {
        let got = read_mtime("dos-only", &one_entry_zip(&[])).unwrap();
        assert_is_local_reading("dos-only", got);
    }

    #[test]
    fn extended_timestamp_wins_over_dos() {
        let mut ut = vec![0x01];
        ut.extend_from_slice(&(ELSEWHERE as u32).to_le_bytes());
        let got = read_mtime("ut", &one_entry_zip(&field(0x5455, &ut))).unwrap();
        assert_eq!(got, Some(UNIX_EPOCH + Duration::from_secs(ELSEWHERE)));
    }

    #[test]
    fn ntfs_wins_over_dos() {
        // 2020-01-12T03:04:06.5Z in 100 ns ticks since 1601.
        let ticks = (ELSEWHERE + 11_644_473_600) * 10_000_000 + 5_000_000;
        let mut nt = vec![0, 0, 0, 0, 0x01, 0x00, 24, 0];
        nt.extend_from_slice(&ticks.to_le_bytes());
        nt.extend_from_slice(&ticks.to_le_bytes());
        nt.extend_from_slice(&ticks.to_le_bytes());
        let got = read_mtime("ntfs", &one_entry_zip(&field(0x000A, &nt))).unwrap();
        assert_eq!(
            got,
            Some(UNIX_EPOCH + Duration::from_secs(ELSEWHERE) + Duration::from_millis(500))
        );
    }

    /// The older Info-ZIP Unix field, which Cram writes on encrypted entries of the parallel path
    /// and which tools wrote before `0x5455` existed: the access time, then the modification time.
    /// The two differ here, so reading the wrong four bytes shows.
    #[test]
    fn unix_field_wins_over_dos() {
        let mut ux = (ELSEWHERE as u32 - 3600).to_le_bytes().to_vec();
        ux.extend_from_slice(&(ELSEWHERE as u32).to_le_bytes());
        let got = read_mtime("ux", &one_entry_zip(&field(0x5855, &ux))).unwrap();
        assert_eq!(got, Some(UNIX_EPOCH + Duration::from_secs(ELSEWHERE)));
    }

    /// Both Info-ZIP fields in one header: the current one answers.
    #[test]
    fn extended_timestamp_wins_over_the_unix_field() {
        let mut ux = (ELSEWHERE as u32 - 3600).to_le_bytes().to_vec();
        ux.extend_from_slice(&(ELSEWHERE as u32 - 7200).to_le_bytes());
        let mut ut = vec![0x01];
        ut.extend_from_slice(&(ELSEWHERE as u32).to_le_bytes());
        let mut extra = field(0x5855, &ux);
        extra.extend_from_slice(&field(0x5455, &ut));
        let got = read_mtime("ux-ut", &one_entry_zip(&extra)).unwrap();
        assert_eq!(got, Some(UNIX_EPOCH + Duration::from_secs(ELSEWHERE)));
    }

    /// Hostile time fields must not panic. A value that parses but cannot be trusted falls back to
    /// the DOS fields; a field the zip crate cannot parse makes it refuse the archive at open, which
    /// is an error, not a crash.
    #[test]
    fn hostile_time_fields_do_not_panic() {
        // Parse, but out of range: fall back to the DOS fields.
        let mut huge_ut = vec![0x01];
        huge_ut.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut all_ut = vec![0x07];
        for _ in 0..3 {
            all_ut.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        let mut huge_nt = vec![0, 0, 0, 0, 0x01, 0x00, 24, 0];
        for _ in 0..3 {
            huge_nt.extend_from_slice(&u64::MAX.to_le_bytes());
        }
        let mut zero_nt = vec![0, 0, 0, 0, 0x01, 0x00, 24, 0];
        zero_nt.extend_from_slice(&[0u8; 24]);
        for (tag, extra) in [
            ("ut-max", field(0x5455, &huge_ut)),
            ("ut-all-max", field(0x5455, &all_ut)),
            ("nt-max", field(0x000A, &huge_nt)),
            ("nt-zero", field(0x000A, &zero_nt)),
            // The Unix field arrives as raw bytes: a value with the top bit set is not trusted,
            // and one too short to hold a modification time is passed over.
            ("ux-max", field(0x5855, &[0xFF; 8])),
            ("ux-short", field(0x5855, &[1, 2, 3, 4, 5])),
            ("ux-empty", field(0x5855, &[])),
        ] {
            let got = read_mtime(tag, &one_entry_zip(&extra))
                .unwrap_or_else(|e| panic!("{tag}: archive refused: {e}"));
            assert_is_local_reading(tag, got);
        }

        // Too short to hold what the flags promise, or empty: whatever the zip crate decides, it
        // must be an answer and not a panic. If it does open, the DOS fields are what is left.
        for (tag, extra) in [
            ("ut-empty", field(0x5455, &[])),
            ("ut-flags-only", field(0x5455, &[0x01])),
            ("ut-short", field(0x5455, &[0x01, 0xAA])),
            ("ut-truncated", field(0x5455, &[0x01, 0xAA, 0xBB, 0xCC])),
            (
                "nt-short",
                field(0x000A, &[0, 0, 0, 0, 0x01, 0x00, 24, 0, 1, 2]),
            ),
        ] {
            match read_mtime(tag, &one_entry_zip(&extra)) {
                Err(e) => eprintln!("{tag}: refused at open: {e}"),
                Ok(got) => {
                    eprintln!("{tag}: opened");
                    assert_is_local_reading(tag, got);
                }
            }
        }
    }
}
