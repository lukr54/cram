//! RAR backend, **read-only** via the official UnRAR C++ engine (the same decoder WinRAR uses).
//! Creating RAR is legally forbidden by the UnRAR license, so there is no writer.
//!
//! RAR entries share one compressed stream (solid archives especially), so there's no per-file
//! parallelism: this is a sequential [`ArchiveReader`]. It drives UnRAR's type-state cursor
//! (`read_header` → `read`/`skip`) across [`next_entry`](RarReader::next_entry) calls, possible
//! because `OpenArchive` owns its C handle (no borrow of the path/password), so it can live in the
//! struct between calls. Each file entry is read fully into memory (UnRAR has no per-chunk hook),
//! then streamed to disk by the sequential engine; only one entry is buffered at a time. Because
//! that read is whole-entry (unbounded by anything but the declared size), an entry whose declared
//! size exceeds `MAX_INMEM_ENTRY` is refused (surfaced as a per-entry failure) rather than allowed
//! to OOM the process, a crafted RAR could otherwise claim a multi-TB entry.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use unrar::{Archive, CursorBeforeHeader, OpenArchive, Process, VolumeInfo};

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream};
use crate::secret::{PasswordProvider, PasswordRequest, Secret};

/// Ceiling on a single RAR entry read whole into RAM. UnRAR's safe API has no per-chunk hook (only
/// whole-entry `read()` or extract-to-file), so we bound the declared size: a hostile archive can't
/// force an unbounded allocation, and an entry above this is reported as a failure, not extracted.
/// Generous so real files pass; a future extract-to-temp path could stream and lift this.
const MAX_INMEM_ENTRY: u64 = 2 * 1024 * 1024 * 1024;

/// A body that fails on first read, surfaces an entry we deliberately refuse to extract (an
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

            // Refuse an entry whose declared size would buffer an unreasonable amount in RAM: UnRAR
            // reads the whole entry (no per-chunk hook), so a crafted huge size would OOM. Skip its
            // data and hand back a body that errors, so the engine records one failure and moves on.
            if size > MAX_INMEM_ENTRY {
                self.state = Some(header.skip().map_err(map_unrar)?);
                return Ok(Some(EntryStream {
                    entry,
                    body: Box::new(ErrBody::new(format!(
                        "entry too large to extract from RAR in memory ({size} bytes)"
                    ))),
                    meta_final: true,
                }));
            }

            // File: UnRAR extracts the whole entry into a Vec (no per-chunk hook), advancing the
            // cursor. The sequential engine streams that Vec to disk.
            let (bytes, next) = match header.read() {
                Ok(v) => v,
                // A damaged entry, most often "File CRC error" from a partly corrupt archive.
                // One bad file must not end the whole extraction: WinRAR reports the file and
                // continues, and a damaged archive is precisely when the remaining files matter
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
