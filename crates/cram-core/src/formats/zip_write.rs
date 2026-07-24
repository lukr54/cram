//! ZIP writer backend, the create counterpart to [`super::zip`]. Wraps the `zip` crate's
//! `ZipWriter` behind the incremental [`ArchiveWriter`] trait: `start_file` + stream the body,
//! `add_directory`, then `finish` writes the central directory.
//!
//! Encryption: **AES-256** (WinZip AE, via `with_aes_encryption`). The locked create fork also
//! offers a labeled-weak legacy ZipCrypto, but `zip` 8.x does not expose ZipCrypto *writing* in its
//! public API (`with_deprecated_encryption` is crate-private), so that path returns
//! [`ArchiveError::UnsupportedEncryption`] for now rather than silently downgrading the cipher.
//! ZIP cannot hide the file listing, so requesting [`HeaderMode::NamesToo`](crate::secret::HeaderMode)
//! for ZIP is **rejected** at create time rather than silently leaving every filename exposed.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::time::{Instant, SystemTime};

use zip::result::ZipError;
use zip::write::{FileOptions, SimpleFileOptions};
use zip::{AesMode, CompressionMethod, ZipWriter as ZipCrateWriter};

use crate::error::{ArchiveError, Result};
use crate::format::Codec;
use crate::model::Entry;
use crate::secret::{HeaderMode, Secret, ZipCipher};
use crate::writer::{ArchiveWriter, CreateOptions, CreateReport, Level, WriteHint};

/// Files at/above 4 GiB (compressed or uncompressed) need the ZIP64 `large_file` flag.
const ZIP64_THRESHOLD: u64 = 0xFFFF_FFFF;

fn map_zip_write(e: ZipError) -> ArchiveError {
    match e {
        ZipError::Io(io) => ArchiveError::Io(io),
        other => ArchiveError::Backend(format!("zip write: {other}")),
    }
}

/// Build the per-file options: compression method + level, plus AES-256 when a password is set.
/// Free function (not a `&self` method) so it borrows only the password field, leaving the writer
/// handle free to borrow mutably for `start_file`.
fn file_options(
    method: CompressionMethod,
    level: Option<i64>,
    pw: Option<&Secret>,
    large: bool,
    modified: Option<SystemTime>,
) -> FileOptions<'_, ()> {
    let mut opts = SimpleFileOptions::default().compression_method(method);
    // STORE takes no level; only compressing methods do.
    if method != CompressionMethod::Stored {
        opts = opts.compression_level(level);
    }
    if large {
        opts = opts.large_file(true);
    }
    if let Some(dt) = zip_datetime(modified) {
        opts = opts.last_modified_time(dt);
    }
    match pw {
        Some(s) => opts.with_aes_encryption(AesMode::Aes256, s.expose()),
        None => opts,
    }
}

/// The entry's mtime as a ZIP `DateTime`, or `None` if absent or outside ZIP's DOS-time range (which
/// starts at 1980, an older or missing timestamp is simply not stored, leaving the crate default).
/// Sourced from the input file's mtime (UTC), so an identical input tree still yields an identical zip.
///
/// The input may be a mtime a *reader* surfaced from an untrusted archive (via `convert`), so it must
/// be bounded before `time::OffsetDateTime::from(SystemTime)`, that conversion **panics** for a time
/// beyond `time`'s ±9999-year range. A far-future or pre-epoch timestamp is dropped, not stored.
fn zip_datetime(modified: Option<SystemTime>) -> Option<zip::DateTime> {
    let t = modified?;
    // `duration_since` is `Err` for a pre-1970 time; DOS time can't hold those anyway.
    let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    const MAX_SANE_UNIX: u64 = 32_503_680_000; // ~year 3000, well inside `time`'s range, absurdly future
    if secs > MAX_SANE_UNIX {
        return None;
    }
    let odt = time::OffsetDateTime::from(t);
    zip::DateTime::from_date_and_time(
        odt.year() as u16,
        u8::from(odt.month()),
        odt.day(),
        odt.hour(),
        odt.minute(),
        odt.second(),
    )
    .ok()
}

/// The ZIP name for an entry: the normalized-safe relative path with forward slashes (ZIP uses `/`).
fn zip_name(entry: &Entry) -> String {
    entry.path.safe().to_string_lossy().replace('\\', "/")
}

pub struct ZipArchiveWriter {
    /// `Option` so `finish` can take ownership out of `&mut self`-shaped call sites.
    zw: Option<ZipCrateWriter<BufWriter<File>>>,
    method: CompressionMethod,
    level: Option<i64>,
    /// AES-256 password, or `None` for an unencrypted archive.
    aes_pw: Option<Secret>,
    entries: u64,
    in_bytes: u64,
    /// Entries the adaptive probe stored verbatim (incompressible), for the report.
    stored: u64,
    start: Instant,
}

impl ZipArchiveWriter {
    pub fn create(path: &Path, opts: &CreateOptions) -> Result<Self> {
        // Encryption fork: AES-256 supported; legacy ZipCrypto write is not exposed by `zip` 8.x.
        let aes_pw = match &opts.encrypt {
            None => None,
            Some(spec) => match spec.zip_cipher {
                ZipCipher::Aes256 => Some(spec.password.clone()),
                ZipCipher::LegacyZipCrypto => {
                    return Err(ArchiveError::UnsupportedEncryption);
                }
            },
        };

        // ZIP encrypts file *contents* but not the central-directory names. Honoring a "hide names"
        // request silently would expose every filename while the user believes they're hidden, so
        // refuse it here. .7z and .cram encrypt the listing and should be used instead.
        if let Some(spec) = &opts.encrypt {
            if spec.header == HeaderMode::NamesToo {
                return Err(ArchiveError::Backend(
                    "ZIP cannot hide file names, use .7z or .cram to encrypt the file listing"
                        .into(),
                ));
            }
        }

        // Codec: STORE only when explicitly asked for no compression; otherwise DEFLATE.
        let method = match opts.codec {
            Some(Codec::None) => CompressionMethod::Stored,
            _ => CompressionMethod::Deflated,
        };
        let level = deflate_level(opts.level);

        let file = File::create(path)?;
        let zw = ZipCrateWriter::new(BufWriter::new(file));
        Ok(Self {
            zw: Some(zw),
            method,
            level,
            aes_pw,
            entries: 0,
            in_bytes: 0,
            stored: 0,
            start: Instant::now(),
        })
    }
}

/// Map the abstract [`Level`] onto DEFLATE's 0–9 scale (`None` = the crate default, 6).
fn deflate_level(level: Level) -> Option<i64> {
    match level {
        Level::Auto | Level::Balanced => None,
        Level::Fastest => Some(1),
        Level::Best => Some(9),
        Level::Explicit(n) => Some((n as i64).clamp(0, 9)),
    }
}

impl ArchiveWriter for ZipArchiveWriter {
    fn add_file(&mut self, entry: &Entry, body: &mut dyn io::Read, hint: WriteHint) -> Result<()> {
        // Adaptive store: an incompressible entry is written STORE even under a compressing level,
        // saving CPU and avoiding the slight growth DEFLATE adds to already-compressed data. When
        // the whole archive is already STORE (explicit `--store`), the hint is a no-op.
        let adaptive_store = hint.store && self.method != CompressionMethod::Stored;
        let method = if hint.store {
            CompressionMethod::Stored
        } else {
            self.method
        };
        // ZIP64 is needed when EITHER size crosses 4 GiB. The compressed stream can exceed the raw
        // size: AES-256 framing adds 28 bytes, and DEFLATE on incompressible data grows ~0.03% plus
        // block overhead, so a raw size just under the threshold (e.g. 0xFFFF_FFF0 stored+AES)
        // overflows the 32-bit compressed-size field mid-write and the zip crate hard-errors after
        // streaming the whole entry. Decide with a worst-case margin instead of the raw size alone.
        let large = entry.size.saturating_add(entry.size / 1000 + (64 << 10)) >= ZIP64_THRESHOLD;
        let opts = file_options(
            method,
            self.level,
            self.aes_pw.as_ref(),
            large,
            entry.modified,
        );
        let name = zip_name(entry);
        let zw = self
            .zw
            .as_mut()
            .ok_or_else(|| ArchiveError::Backend("zip writer already finished".into()))?;
        zw.start_file(name, opts).map_err(map_zip_write)?;
        let n = io::copy(body, zw)?;
        self.entries += 1;
        self.in_bytes += n;
        if adaptive_store {
            self.stored += 1;
        }
        Ok(())
    }

    fn add_dir(&mut self, entry: &Entry) -> Result<()> {
        // Dirs carry no body → no compression/encryption options needed, but still carry their mtime.
        let name = zip_name(entry);
        let mut opts = SimpleFileOptions::default();
        if let Some(dt) = zip_datetime(entry.modified) {
            opts = opts.last_modified_time(dt);
        }
        let zw = self
            .zw
            .as_mut()
            .ok_or_else(|| ArchiveError::Backend("zip writer already finished".into()))?;
        zw.add_directory(name, opts).map_err(map_zip_write)?;
        self.entries += 1;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<CreateReport> {
        let zw = self
            .zw
            .take()
            .ok_or_else(|| ArchiveError::Backend("zip writer already finished".into()))?;
        let buf = zw.finish().map_err(map_zip_write)?;
        // Flush the BufWriter down to the file and measure the final archive size.
        let file = buf
            .into_inner()
            .map_err(|e| ArchiveError::Io(e.into_error()))?;
        let out_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(CreateReport {
            entries: self.entries,
            in_bytes: self.in_bytes,
            out_bytes,
            stored: self.stored,
            dedup_saved: 0,
            elapsed: self.start.elapsed(),
        })
    }
}

#[cfg(test)]
mod mtime_guard_tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn zip_datetime_bounds_and_survives_hostile_input() {
        assert!(zip_datetime(None).is_none());
        // Pre-1980 (DOS epoch) has no representation -> dropped, not stored.
        assert!(zip_datetime(Some(UNIX_EPOCH)).is_none());
        // A far-future time (~year 5000) must be rejected BEFORE time::OffsetDateTime::from, which
        // panics beyond +/-9999 - the bound makes this return None, never crash.
        let huge = UNIX_EPOCH + Duration::from_secs(95_617_584_000);
        assert!(zip_datetime(Some(huge)).is_none());
        // A real 2020 timestamp converts.
        let real = UNIX_EPOCH + Duration::from_secs(1_577_934_246);
        assert!(zip_datetime(Some(real)).is_some());
    }
}
