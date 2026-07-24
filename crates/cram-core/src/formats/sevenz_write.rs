//! 7z writer backend — the create counterpart to [`super::sevenz`], via `sevenz-rust2`'s encoder
//! (the `compress` feature). Uses `push_archive_entry` (one independently-decodable pack per entry,
//! *non-solid*): this both fits the incremental [`ArchiveWriter`] contract (stream one entry at a
//! time) and matches Cram's strategy of authoring parallel-extractable layouts.
//!
//! **Adaptive per-entry store:** because each entry is its own pack, the content-method chain can
//! change between entries. `push_archive_entry` records whatever `set_content_methods` holds at the
//! time of the call into that entry's folder, so an incompressible entry (per the probe's
//! [`WriteHint`]) is written with a COPY chain while the rest use LZMA2 — heterogeneous folders in
//! one 7z are standard and both 7-Zip and our own reader handle them.
//!
//! Encryption is 7z's real strength and both create forks are honored:
//!   - **AES-256** content encryption (`AesEncoderOptions`) — 7-Zip's own scheme: ONE random salt
//!     per archive (the KDF then runs once and is cached), a fresh random IV per entry, and the
//!     7-Zip-standard KDF work factor (`num_cycles_power = 19`, ≈524k SHA-256 rounds — the library
//!     default of 8 is ~256 rounds, far too weak against offline guessing).
//!   - **Header (name) encryption** — [`HeaderMode::NamesToo`] maps to `set_encrypt_header(true)`,
//!     so the file listing needs the password too; [`HeaderMode::ContentsOnly`] leaves names visible.
//!     `finish` installs a FRESH AES configuration before finalizing: the library encrypts the
//!     header with the last configuration it saw, which would otherwise reuse the last entry's IV —
//!     and an archive with no file entries would have no AES configuration at all, silently writing
//!     a NamesToo header in plaintext.
//!
//! The content-method chain is written AES-first, compressor-second (`vec![aes, lzma2]`): the last
//! method is applied to the data first, so bytes are compressed *then* encrypted.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::Instant;

use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
use sevenz_rust2::{
    ArchiveEntry, ArchiveWriter as SzWriter, EncoderConfiguration, EncoderMethod, Error as SzError,
    Password,
};

use crate::error::{ArchiveError, Result};
use crate::format::Codec;
use crate::model::Entry;
use crate::secret::{HeaderMode, Secret};
use crate::writer::{ArchiveWriter, CreateOptions, CreateReport, Level, WriteHint};

fn map_sz(e: SzError) -> ArchiveError {
    match e {
        SzError::Io(io, _) | SzError::FileOpen(io, _) => ArchiveError::Io(io),
        SzError::PasswordRequired => ArchiveError::PasswordRequired,
        SzError::MaybeBadPassword(_) => ArchiveError::WrongPassword,
        other => ArchiveError::Backend(format!("7z write: {other}")),
    }
}

/// 7-Zip's standard AES-256 key-derivation work factor: 2^19 ≈ 524k SHA-256 rounds. The library
/// default is 8 (~256 rounds), which makes offline password guessing ~2000× cheaper. The decoder
/// side (ours and 7-Zip's) accepts up to 24.
const AES_CYCLES_POWER: u8 = 19;

/// Map the abstract [`Level`] onto LZMA2's 0–9 scale (`Auto`/`Balanced` → 6).
fn lzma_level(level: Level) -> u32 {
    match level {
        Level::Auto | Level::Balanced => 6,
        Level::Fastest => 1,
        Level::Best => 9,
        Level::Explicit(n) => n.clamp(0, 9),
    }
}

/// The 7z archive name for an entry: normalized-safe relative path, forward slashes.
fn arc_name(entry: &Entry) -> String {
    entry.path.safe().to_string_lossy().replace('\\', "/")
}

pub struct SevenZArchiveWriter {
    /// `Option` so `finish` can move the writer out.
    sz: Option<SzWriter<File>>,
    /// LZMA2 level for compressible entries.
    level: u32,
    /// Explicit `--store` (`opts.codec == Some(Codec::None)`): every entry is COPY regardless of hint.
    store_forced: bool,
    /// AES-256 password, or `None` for an unencrypted archive.
    aes_pw: Option<Secret>,
    /// One random KDF salt for the whole archive (7-Zip's model): the derived key is then computed
    /// once and cached by the library, while each entry still gets its own fresh random IV. A fresh
    /// salt per entry would be equivalent cryptographically but re-runs the ~524k-round KDF for
    /// every entry. Unused when `aes_pw` is `None`.
    aes_salt: [u8; 16],
    entries: u64,
    in_bytes: u64,
    /// Entries the adaptive probe stored verbatim (incompressible), for the report.
    stored: u64,
    start: Instant,
}

impl SevenZArchiveWriter {
    pub fn create(path: &Path, opts: &CreateOptions) -> Result<Self> {
        let mut sz = SzWriter::create(path).map_err(map_sz)?;

        let mut aes_salt = [0u8; 16];
        let aes_pw = match &opts.encrypt {
            None => {
                sz.set_encrypt_header(false); // no password → can't (and needn't) encrypt the header
                None
            }
            Some(spec) => {
                sz.set_encrypt_header(spec.header == HeaderMode::NamesToo);
                // One random salt for the whole archive (see the field docs). `new` generates a
                // cryptographically random salt; keep it, discard the rest of the throwaway.
                aes_salt = AesEncoderOptions::new(Password::new(spec.password.expose())).salt;
                Some(spec.password.clone())
            }
        };

        Ok(Self {
            sz: Some(sz),
            level: lzma_level(opts.level),
            store_forced: matches!(opts.codec, Some(Codec::None)),
            aes_pw,
            aes_salt,
            entries: 0,
            in_bytes: 0,
            stored: 0,
            start: Instant::now(),
        })
    }

    fn writer(&mut self) -> Result<&mut SzWriter<File>> {
        self.sz
            .as_mut()
            .ok_or_else(|| ArchiveError::Backend("7z writer already finished".into()))
    }

    /// The content-method chain for one entry: COPY when storing, else LZMA2; wrapped in AES
    /// (applied last, so compress-then-encrypt) when the archive is encrypted.
    fn content_methods(&self, store: bool) -> Vec<EncoderConfiguration> {
        let compress: EncoderConfiguration = if store {
            EncoderConfiguration::new(EncoderMethod::COPY)
        } else {
            Lzma2Options::from_level(self.level).into()
        };
        match &self.aes_pw {
            Some(pw) => {
                // `new` gives a fresh random IV each call (one per entry); pin the archive-wide
                // salt and the 7-Zip-standard KDF work factor over the library defaults.
                let mut aes = AesEncoderOptions::new(Password::new(pw.expose()));
                aes.salt = self.aes_salt;
                aes.num_cycles_power = AES_CYCLES_POWER;
                vec![aes.into(), compress]
            }
            None => vec![compress],
        }
    }
}

impl ArchiveWriter for SevenZArchiveWriter {
    fn add_file(&mut self, entry: &Entry, body: &mut dyn io::Read, hint: WriteHint) -> Result<()> {
        let store = hint.store || self.store_forced;
        let adaptive_store = hint.store && !self.store_forced;
        // Swap the content-method chain for this entry, then push it as its own pack.
        let methods = self.content_methods(store);
        let sz_entry = ArchiveEntry::new_file(&arc_name(entry));
        let w = self.writer()?;
        w.set_content_methods(methods);
        w.push_archive_entry(sz_entry, Some(body)).map_err(map_sz)?;
        self.entries += 1;
        self.in_bytes += entry.size;
        if adaptive_store {
            self.stored += 1;
        }
        Ok(())
    }

    fn add_dir(&mut self, entry: &Entry) -> Result<()> {
        let sz_entry = ArchiveEntry::new_directory(&arc_name(entry));
        self.writer()?
            .push_archive_entry(sz_entry, None::<io::Empty>)
            .map_err(map_sz)?;
        self.entries += 1;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<CreateReport> {
        let mut sz = self
            .sz
            .take()
            .ok_or_else(|| ArchiveError::Backend("7z writer already finished".into()))?;
        if self.aes_pw.is_some() {
            // Install a FRESH AES configuration for the header pass. The library encrypts the
            // header by cloning the AES entry of the *current* content-method chain, so without
            // this: (a) the header reuses the LAST entry's IV, and (b) an archive that never had
            // an `add_file` (empty, or directories only) has no AES configuration at all and a
            // NamesToo header is silently written in PLAINTEXT.
            sz.set_content_methods(self.content_methods(false));
        }
        let file = sz.finish().map_err(ArchiveError::Io)?;
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
