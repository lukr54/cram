//! RAR backend, **read-only** via the official UnRAR C++ engine (the same decoder WinRAR uses).
//! Creating RAR is legally forbidden by the UnRAR license, so there is no writer.
//!
//! RAR entries share one compressed stream (solid archives especially), so there's no per-file
//! parallelism: this is a sequential [`ArchiveReader`]. It drives UnRAR's type-state cursor
//! (`read_header` → `read`/`skip`) across [`next_entry`](RarReader::next_entry) calls, possible
//! because `OpenArchive` owns its C handle (no borrow of the path/password), so it can live in the
//! struct between calls. Each file entry is read fully into memory (UnRAR has no per-chunk hook),
//! then streamed to disk by the sequential engine; only one entry is buffered at a time. Because
//! that read is whole-entry (unbounded by anything but the declared size), an entry larger than
//! [`inmem_ceiling`] is instead extracted by UnRAR straight to a scratch file and streamed back
//! from there, so a crafted RAR claiming a multi-TB entry cannot force the allocation, and a real
//! multi-gigabyte one still extracts.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use unrar::{Archive, CursorBeforeHeader, OpenArchive, Process, VolumeInfo};

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::hw;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream};
use crate::secret::{PasswordProvider, PasswordRequest, Secret};

/// Hard ceiling on what may be read whole into RAM, whatever the machine reports.
const INMEM_CAP: u64 = 1024 * 1024 * 1024;
/// Floor, so a machine that reports almost no free memory still takes the fast path for ordinary
/// files instead of writing a scratch copy of everything.
const INMEM_FLOOR: u64 = 64 * 1024 * 1024;

/// How large a RAR entry may be before it is streamed through a scratch file instead of a `Vec`.
///
/// UnRAR's safe API has no per-chunk hook: `read()` returns the whole entry at once. So every entry
/// is either one allocation of its full size or one temporary file, and the only question is where
/// the line sits. Below it, the allocation is cheaper than the extra write and read a scratch copy
/// costs. Above it, the allocation is the thing that fails.
///
/// Derived from free memory rather than fixed, because the old fixed 2 GiB was wrong in both
/// directions: it refused entries that a 24 GiB machine could hold easily, and it would happily
/// attempt a 1.9 GiB allocation on a machine with 2 GiB free. This is extraction, so nothing here
/// reaches an archive's bytes and it is free to depend on the machine.
fn inmem_ceiling() -> u64 {
    if let Some(n) = std::env::var_os("CRAM_RAR_INMEM")
        .and_then(|v| v.to_str()?.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    let avail = hw::HwProfile::detect().ram_avail;
    if avail == 0 {
        return INMEM_FLOOR;
    }
    (avail / 4).clamp(INMEM_FLOOR, INMEM_CAP)
}

/// A body backed by a scratch file, which is removed once the engine has finished reading it.
struct ScratchBody {
    file: Option<File>,
    path: PathBuf,
}

impl ScratchBody {
    fn new(file: File, path: PathBuf) -> Self {
        Self {
            file: Some(file),
            path,
        }
    }
}

impl io::Read for ScratchBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.file.as_mut() {
            Some(f) => f.read(buf),
            None => Ok(0),
        }
    }
}

impl Drop for ScratchBody {
    fn drop(&mut self) {
        // Close the handle before unlinking: Windows refuses to delete an open file.
        self.file = None;
        let _ = fs::remove_file(&self.path);
    }
}

/// A body that fails on first read, surfaces an entry we refuse to extract (an
/// oversized RAR entry UnRAR would buffer whole in RAM) as a per-entry failure via the engine's
/// normal write loop, rather than aborting the whole job or silently dropping the entry.
struct ErrBody(Option<String>);

impl ErrBody {
    fn new(msg: String) -> Self {
        ErrBody(Some(msg))
    }
}

impl io::Read for ErrBody {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other(
            self.0.take().unwrap_or_else(|| "entry error".into()),
        ))
    }
}

fn map_unrar(e: unrar::error::UnrarError) -> ArchiveError {
    use unrar::error::Code;
    match e.code {
        Code::BadPassword => ArchiveError::WrongPassword,
        Code::MissingPassword => ArchiveError::PasswordRequired,
        _ => ArchiveError::Backend(format!("unrar: {e}")),
    }
}

/// A RAR entry's MS-DOS packed mod-time (`file_time`) as a [`SystemTime`]. `0` (unset) → `None`. DOS
/// time is local wall time at 2-second granularity with no zone; the fields are read as UTC (the best
/// the format allows) and built through the `time` crate. Any out-of-range field yields `None`, never
/// a panic, `file_time` is attacker-controlled in a hostile archive.
fn dos_time(dos: u32) -> Option<SystemTime> {
    if dos == 0 {
        return None;
    }
    let sec = ((dos & 0x1F) * 2) as u8;
    let min = ((dos >> 5) & 0x3F) as u8;
    let hour = ((dos >> 11) & 0x1F) as u8;
    let day = ((dos >> 16) & 0x1F) as u8;
    let month = ((dos >> 21) & 0x0F) as u8;
    let year = 1980 + ((dos >> 25) & 0x7F) as i32;
    let month = time::Month::try_from(month).ok()?;
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let tod = time::Time::from_hms(hour, min, sec).ok()?;
    Some(SystemTime::from(date.with_time(tod).assume_utc()))
}

/// Cap on entry-metadata buffered by the listing pass. `open_for_listing` yields headers lazily, so
/// without a bound a hostile RAR declaring millions of members would grow `out` until OOM (the same
/// class the tar `scan` cap guards). 1 GiB of metadata covers any realistic listing.
const MAX_LIST_META: u64 = 1024 * 1024 * 1024;
const RAR_ENTRY_OVERHEAD: u64 = 256;

/// Headers-only listing pass → cram `Entry`s + whether any entry is encrypted. Zip-slip names are
/// dropped via [`EntryPath::from_raw`]. `Entry::index` isn't meaningful for RAR (sequential), kept 0.
fn try_list(path: &Path, pw: Option<&Secret>) -> Result<(Vec<Entry>, bool)> {
    let listing = match pw {
        Some(s) => Archive::with_password(path, s.expose()).open_for_listing(),
        None => Archive::new(path).open_for_listing(),
    }
    .map_err(map_unrar)?;
    let mut out = Vec::new();
    let mut any_encrypted = false;
    let mut meta: u64 = 0;
    for item in listing {
        let h = item.map_err(map_unrar)?;
        if h.is_encrypted() {
            any_encrypted = true;
        }
        let name = h.filename.to_string_lossy().into_owned();
        // Bound buffered metadata before retaining this entry (lazy iterator → otherwise unbounded).
        meta = meta
            .saturating_add(name.len() as u64)
            .saturating_add(RAR_ENTRY_OVERHEAD);
        if meta > MAX_LIST_META {
            return Err(ArchiveError::Backend(format!(
                "RAR lists more than {} MiB of entry metadata, too large to buffer; extract it instead",
                MAX_LIST_META / (1024 * 1024)
            )));
        }
        if let Some(safe) = EntryPath::from_raw(&name) {
            out.push(Entry {
                index: 0,
                path: safe,
                kind: if h.is_directory() {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                size: h.unpacked_size,
                compressed_size: None,
                modified: dos_time(h.file_time),
                unix_mode: None,
                crc32: None,
                encrypted: h.is_encrypted(),
            });
        }
    }
    Ok((out, any_encrypted))
}

/// Open the processing cursor (optionally with a password), rejecting a later volume of a multipart
/// set (the user must open the first part).
fn open_processing(
    path: &Path,
    pw: Option<&Secret>,
) -> Result<OpenArchive<Process, CursorBeforeHeader>> {
    let arc = match pw {
        Some(s) => Archive::with_password(path, s.expose()).open_for_processing(),
        None => Archive::new(path).open_for_processing(),
    }
    .map_err(map_unrar)?;
    if arc.volume_info() == VolumeInfo::Subsequent {
        return Err(ArchiveError::NeedFirstVolume(path.display().to_string()));
    }
    Ok(arc)
}

/// A RAR archive opened for sequential extraction. `state` is UnRAR's cursor, advanced per entry.
pub struct RarReader {
    entries: Vec<Entry>,
    state: Option<OpenArchive<Process, CursorBeforeHeader>>,
    /// Kept so the cursor can be REBUILT after a damaged entry, see [`RarReader::reposition`].
    path: std::path::PathBuf,
    secret: Option<Secret>,
    /// How many headers have been consumed. This is the cursor position to restore to.
    pos: usize,
}

impl RarReader {
    /// Where to put the scratch copy of an entry too large to hold in memory.
    ///
    /// Beside the archive first. That directory demonstrably holds a file at least as large as the
    /// one being extracted, and on Linux it avoids `/tmp`, which is frequently a tmpfs -- writing a
    /// 3 GiB scratch copy into RAM would defeat the entire point of not holding it in RAM. Falls
    /// back to the system temp directory when the archive's own directory is not writable, which is
    /// the read-only-share and mounted-image case.
    fn scratch_path(&self) -> Option<PathBuf> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!(".cram-rar-{}-{}.part", std::process::id(), stamp);
        for dir in [
            self.path.parent().map(Path::to_path_buf),
            Some(std::env::temp_dir()),
        ]
        .into_iter()
        .flatten()
        {
            let candidate = dir.join(&name);
            // Prove it is writable by creating it, rather than inferring from permissions.
            if File::create(&candidate).is_ok() {
                return Some(candidate);
            }
        }
        None
    }

    /// Re-open the archive and wind the cursor forward to just after header `self.pos`.
    ///
    /// Needed because `unrar`'s `read()` takes `self` by value and propagates with `?` *before*
    /// rebuilding the archive, so a failed entry read drops the C handle: there is no way to
    /// continue through the existing cursor. WinRAR reports a damaged file and carries on to the
    /// next one, and a partly-corrupt archive is exactly when the remaining files matter most; so
    /// we pay a re-open and skip forward instead of abandoning the job.
    ///
    /// Skipping only walks headers and seeks over file data (no decode), so the cost is small
    /// relative to the extraction itself. Returns false if the archive can no longer be positioned,
    /// in which case extraction ends cleanly with everything recovered so far.
    fn reposition(&mut self) -> bool {
        let Ok(mut arc) = open_processing(&self.path, self.secret.as_ref()) else {
            return false;
        };
        for _ in 0..self.pos {
            match arc.read_header() {
                Ok(Some(header)) => match header.skip() {
                    Ok(next) => arc = next,
                    Err(_) => return false,
                },
                // Ran out of entries, or the headers themselves are unreadable; nothing left to do.
                _ => return false,
            }
        }
        self.state = Some(arc);
        true
    }
}

impl RarReader {
    pub fn open(path: &Path, pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // List plainly first; if that fails the headers may be encrypted (-hp) → retry with a
        // password if the provider can supply one.
        let (entries, any_encrypted, header_secret) = match try_list(path, None) {
            Ok((e, enc)) => (e, enc, None),
            Err(list_err) => match pw.password(&PasswordRequest {
                archive: &name,
                entry: None,
                for_header: true,
                attempt: 0,
            }) {
                Some(secret) => {
                    let (e, enc) = try_list(path, Some(&secret))?;
                    (e, enc, Some(secret))
                }
                None => return Err(list_err),
            },
        };

        // Password for extraction: reuse the header password, or pull one if files are encrypted.
        let secret = if header_secret.is_some() {
            header_secret
        } else if any_encrypted {
            Some(
                pw.password(&PasswordRequest {
                    archive: &name,
                    entry: None,
                    for_header: false,
                    attempt: 0,
                })
                .ok_or(ArchiveError::PasswordRequired)?,
            )
        } else {
            None
        };

        let state = Some(open_processing(path, secret.as_ref())?);
        Ok(Self {
            entries,
            state,
            path: path.to_path_buf(),
            secret,
            pos: 0,
        })
    }
}

impl ArchiveReader for RarReader {
    fn format(&self) -> Format {
        Format::rar()
    }

    fn entries(&self) -> Result<&[Entry]> {
        Ok(&self.entries)
    }

    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>> {
        loop {
            let Some(arc) = self.state.take() else {
                return Ok(None); // exhausted
            };
            let header = match arc.read_header().map_err(map_unrar)? {
                Some(h) => h,
                None => return Ok(None), // end of archive; state stays None
            };
            // This header is now consumed by whichever branch follows, so `pos` is the index of the
            // NEXT header, exactly where `reposition` has to wind back to after a damaged entry.
            self.pos += 1;

            // Copy metadata out before the cursor-advancing call consumes `header`.
            let (name, is_dir, size, encrypted, modified) = {
                let e = header.entry();
                (
                    e.filename.to_string_lossy().into_owned(),
                    e.is_directory(),
                    e.unpacked_size,
                    e.is_encrypted(),
                    dos_time(e.file_time),
                )
            };

            let Some(safe) = EntryPath::from_raw(&name) else {
                // zip-slip: skip past it and keep going.
                self.state = Some(header.skip().map_err(map_unrar)?);
                continue;
            };
            let entry = Entry {
                index: 0,
                path: safe,
                kind: if is_dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                size,
                compressed_size: None,
                modified,
                unix_mode: None,
                crc32: None,
                encrypted,
            };

            if is_dir {
                self.state = Some(header.skip().map_err(map_unrar)?);
                return Ok(Some(EntryStream {
                    entry,
                    body: Box::new(io::empty()),
                    meta_final: true,
                }));
            }

            // An entry too big to hold in RAM goes out to a scratch file and is streamed back from
            // there. UnRAR has no per-chunk hook -- `read()` hands back the whole entry as one Vec
            // -- but it *can* write the entry to a file itself, so a large entry is a temporary
            // file rather than a large allocation.
            //
            // This used to refuse outright above a flat 2 GiB. A repacked game with one asset over
            // that size extracted every other file, reported a single failure, and left an install
            // that did not run; 7-Zip, WinRAR and Bandizip all extract it, because none of them
            // routes the bytes through memory.
            if size > inmem_ceiling() {
                let scratch = match self.scratch_path() {
                    Some(p) => p,
                    None => {
                        self.state = Some(header.skip().map_err(map_unrar)?);
                        return Ok(Some(EntryStream {
                            entry,
                            body: Box::new(ErrBody::new(format!(
                                "entry is {size} bytes, too large to hold in memory, and no \
                                 writable scratch directory was found to stream it through"
                            ))),
                            meta_final: true,
                        }));
                    }
                };
                match header.extract_to(&scratch) {
                    Ok(next) => {
                        self.state = Some(next);
                        let file = File::open(&scratch).map_err(|e| {
                            ArchiveError::Backend(format!("reopening the scratch copy: {e}"))
                        })?;
                        return Ok(Some(EntryStream {
                            entry,
                            body: Box::new(ScratchBody::new(file, scratch)),
                            meta_final: true,
                        }));
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&scratch);
                        // Same policy as the in-memory path below: one damaged entry is a per-entry
                        // failure, not the end of the job.
                        let msg = format!("unrar: {e}");
                        if !self.reposition() {
                            self.state = None; // cannot continue; the report keeps what we recovered
                        }
                        return Ok(Some(EntryStream {
                            entry,
                            body: Box::new(ErrBody::new(msg)),
                            meta_final: true,
                        }));
                    }
                }
            }

            // File: UnRAR extracts the whole entry into a Vec (no per-chunk hook), advancing the
            // cursor. The sequential engine streams that Vec to disk.
            let (bytes, next) = match header.read() {
                Ok(v) => v,
                // A damaged entry, most often "File CRC error" from a partly corrupt archive.
                // One bad file must not end the whole extraction: WinRAR reports the file and
                // continues, and a damaged archive is exactly when the remaining files matter
                // most. So report this entry as a per-entry failure (an `ErrBody` the engine turns
                // into one `report.failed` line) and wind the cursor back to the next entry. A
                // password problem is different in kind, it is not damage and it will affect every
                // entry, so it still fails the job.
                Err(e)
                    if !matches!(
                        e.code,
                        unrar::error::Code::BadPassword | unrar::error::Code::MissingPassword
                    ) =>
                {
                    let msg = format!("unrar: {e}");
                    if !self.reposition() {
                        self.state = None; // cannot continue; the report keeps what we recovered
                    }
                    return Ok(Some(EntryStream {
                        entry,
                        body: Box::new(ErrBody::new(msg)),
                        meta_final: true,
                    }));
                }
                Err(e) => return Err(map_unrar(e)),
            };
            self.state = Some(next);
            return Ok(Some(EntryStream {
                entry,
                body: Box::new(io::Cursor::new(bytes)),
                meta_final: true,
            }));
        }
    }
}
