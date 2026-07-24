//! 7z backend — **read-only** for now (create lands with the writer phase), via the pure-Rust
//! `sevenz-rust2` decoder (LZMA/LZMA2 always; BZip2/PPMd/Deflate/LZ4/AES-256 behind features).
//!
//! 7z is solid/blocked: entries in a block share one decode stream, so there's no cheap per-entry
//! random access → this is a sequential [`ArchiveReader`], routed to the sequential engine. The
//! crate's extraction API is a **push** callback (`for_each_entries(|entry, &mut Read|)`), the same
//! shape as tar, so the same fix applies: a **worker thread** owns the reader and pushes
//! `(metadata, bytes)` over a bounded channel; `next_entry` pulls. Listing (`entries`) is a cheap
//! header pass off `archive().files` (no block decode).
//!
//! Passwords: 7z uses ONE archive-wide password. If the *header* is encrypted we resolve it at
//! `open()` (needed even to list). If only *content* is encrypted (header plain, listing browsable),
//! metadata reads with an empty password and the worker resolves the password lazily on the first
//! block-decode failure, retrying from the start (safe: the failure precedes any emitted entry).

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;

use sevenz_rust2::{ArchiveEntry, ArchiveReader as SzReader, Error as SzError, Password};

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream};
use crate::secret::{PasswordProvider, PasswordRequest, Secret};

/// Bytes per streamed body chunk — bounds the worker's in-flight buffer so a huge or compression-
/// bombed entry is streamed to the destination, never buffered whole in RAM (a crafted metadata size
/// would otherwise force an allocation that aborts the process). Reused each read.
const STREAM_CHUNK: usize = 1024 * 1024;

/// One item streamed from the worker thread. A file is `FileStart` then N × `Chunk` then `FileEnd`;
/// a directory is a lone `Dir`.
enum SzMsg {
    Dir(Entry),
    FileStart(Entry),
    Chunk(Vec<u8>),
    FileEnd,
    Err(ArchiveError),
}

fn is_password_error(e: &SzError) -> bool {
    matches!(e, SzError::PasswordRequired | SzError::MaybeBadPassword(_))
}

fn map_sevenz(e: SzError) -> ArchiveError {
    match e {
        SzError::PasswordRequired => ArchiveError::PasswordRequired,
        SzError::MaybeBadPassword(_) => ArchiveError::WrongPassword,
        SzError::BadSignature(_)
        | SzError::ChecksumVerificationFailed
        | SzError::NextHeaderCrcMismatch
        | SzError::BadTerminatedStreamsInfo(_)
        | SzError::BadTerminatedUnpackInfo
        | SzError::BadTerminatedPackInfo(_)
        | SzError::BadTerminatedSubStreamsInfo
        | SzError::BadTerminatedHeader(_)
        | SzError::FileNotFound => ArchiveError::Corrupt(e.to_string()),
        SzError::UnsupportedVersion { .. }
        | SzError::UnsupportedCompressionMethod(_)
        | SzError::ExternalUnsupported
        | SzError::Unsupported(_) => ArchiveError::Backend(format!("7z: {e}")),
        other => ArchiveError::Backend(format!("7z: {other}")),
    }
}

/// A 7z entry's last-modified NTFS FILETIME as a [`SystemTime`], or `None` when absent, zero, or out
/// of a sane range. The raw tick count is attacker-controlled and the crate's `NtTime -> SystemTime`
/// conversion panics on overflow, so we reject anything above ~year 9999 before converting.
fn seven_z_mtime(f: &ArchiveEntry) -> Option<std::time::SystemTime> {
    if !f.has_last_modified_date {
        return None;
    }
    // 100 ns ticks since 1601. ~year 9999 ≈ 2.65e18 ticks — far below `u64::MAX` (1.8e19) and within
    // the representable `SystemTime` range on Windows, so the `+` in the conversion cannot overflow.
    const MAX_SANE_TICKS: u64 = 2_650_000_000_000_000_000;
    let raw = u64::from(f.last_modified_date);
    if raw == 0 || raw >= MAX_SANE_TICKS {
        return None;
    }
    Some(std::time::SystemTime::from(f.last_modified_date))
}

/// Map a 7z metadata entry to a cram [`Entry`], funneling the name through the zip-slip guard
/// (returns `None` for an unsafe name → the caller drops it). `encrypted` is decided at the archive
/// level by [`archive_has_aes`] and applied to every file, since 7z carries no per-entry flag.
fn cram_entry(f: &ArchiveEntry, encrypted: bool) -> Option<Entry> {
    EntryPath::from_raw(&f.name).map(|safe| Entry {
        index: 0,
        path: safe,
        kind: if f.is_directory {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: f.size,
        compressed_size: None, // per-file compressed size is meaningless in a solid block
        // 7z stores an NTFS FILETIME (100 ns ticks since 1601) per entry; surface it so extract can
        // restore it. The value is attacker-controlled, and `NtTime -> SystemTime` adds a `Duration`
        // with a plain `+` that PANICS on overflow — so a crafted near-`u64::MAX` FILETIME could crash
        // the reader. Bound it to a sane range (0 < t < ~year 9999) before converting; anything else
        // is treated as "no timestamp" rather than trusted.
        modified: seven_z_mtime(f),
        unix_mode: None,
        crc32: f.has_crc.then_some(f.crc as u32),
        encrypted: encrypted && !f.is_directory, // container-level (7z has no per-entry flag)
    })
}

/// Whether any block's coder chain uses the AES-256-SHA-256 method (id `06 F1 07 01`). 7z never sets a
/// per-entry encryption flag, so content encryption (a `7z a -pPASS` archive, whose header lists fine
/// without a password) would otherwise be reported as "unprotected". Reading it off the header blocks
/// is the reliable signal — a header-encrypted (`-mhe`) archive never reaches here (open fails first
/// with a password error, handled upstream).
fn archive_has_aes(archive: &sevenz_rust2::Archive) -> bool {
    archive.blocks.iter().any(|b| {
        b.coders
            .iter()
            .any(|c| matches!(c.encoder_method_id(), [0x06, 0xF1, 0x07, 0x01]))
    })
}

/// Header-only pass → the entry list, off `archive().files` (no block is decoded). Also the point
/// where header encryption surfaces (open fails without the right password).
fn read_metadata(path: &Path, secret: &Secret) -> std::result::Result<Vec<Entry>, SzError> {
    let reader = SzReader::open(path, Password::new(secret.expose()))?;
    let aes = archive_has_aes(reader.archive());
    let mut out = Vec::new();
    for f in &reader.archive().files {
        if let Some(e) = cram_entry(f, aes) {
            out.push(e);
        }
    }
    Ok(out)
}

/// One extraction pass: decode every block, buffering each entry and pushing it over `tx`. Sets
/// `*sent_any` once anything has been emitted (so a caller can tell a pre-emit failure — safe to
/// retry with a new password — from a mid-stream one). Stops early (Ok) if the consumer drops.
fn extract_pass(
    path: &Path,
    secret: &Secret,
    tx: &SyncSender<SzMsg>,
    sent_any: &mut bool,
) -> std::result::Result<(), SzError> {
    let mut reader = SzReader::open(path, Password::new(secret.expose()))?;
    // Entries needing NO block decode (directories, empty files) are BUFFERED until the content
    // password is proven by the first successful non-empty read, then flushed in walk order. This
    // keeps `sent_any` false until a real decode succeeds — so even when such an entry precedes the
    // first encrypted file in the walk (common in `7z a -p` archives, whose header is plaintext and
    // whose folders are listed first), a content-password failure still satisfies the worker's
    // `!sent_any` retry gate. Emitting them eagerly would set `sent_any` before any block decode and
    // permanently defeat the lazy retry (a retry re-walks from scratch, so nothing may be emitted).
    let mut pending: Vec<SzMsg> = Vec::new();
    let mut proven = false;
    reader.for_each_entries(|entry, rd| {
        let Some(cram) = cram_entry(entry, false) else {
            // The extract stream never consults `encrypted`, so its value is irrelevant here.
            return Ok(true); // zip-slip name → drop, keep going
        };
        if entry.is_directory {
            let msg = SzMsg::Dir(cram);
            if proven {
                if tx.send(msg).is_err() {
                    return Ok(false); // consumer dropped → stop the walk cleanly
                }
            } else {
                pending.push(msg);
            }
            return Ok(true);
        }
        // Read the FIRST chunk before emitting anything for this file: a content-password failure
        // surfaces on the first block decode, and must happen while `sent_any` is still false. 7z
        // uses one archive-wide key, so once a block decodes there are no further password errors.
        // Streaming in bounded chunks (vs one whole-entry Vec) means a huge/bomb entry can't OOM.
        let mut buf = vec![0u8; STREAM_CHUNK];
        let mut n = rd.read(&mut buf)?;
        if n == 0 {
            // Empty file: no block decoded, so it can't prove the password — buffer it too (unless
            // the password is already proven, in which case emit it now).
            let (start, end) = (SzMsg::FileStart(cram), SzMsg::FileEnd);
            if proven {
                if tx.send(start).is_err() || tx.send(end).is_err() {
                    return Ok(false);
                }
            } else {
                pending.push(start);
                pending.push(end);
            }
            return Ok(true);
        }
        // A non-empty block decoded → the password is proven. Flush any buffered no-decode entries
        // (in walk order) ahead of this file, then emit immediately from here on.
        if !proven {
            proven = true;
            *sent_any = true; // about to emit → a retry-from-scratch is no longer safe
            for msg in pending.drain(..) {
                if tx.send(msg).is_err() {
                    return Ok(false);
                }
            }
        }
        if tx.send(SzMsg::FileStart(cram)).is_err() {
            return Ok(false);
        }
        while n > 0 {
            if tx.send(SzMsg::Chunk(buf[..n].to_vec())).is_err() {
                return Ok(false);
            }
            n = rd.read(&mut buf)?;
        }
        if tx.send(SzMsg::FileEnd).is_err() {
            return Ok(false);
        }
        Ok(true)
    })?;
    // Walk finished cleanly with entries still buffered → the archive had no non-empty file to prove
    // a password (all dirs / empty files), so there was nothing to decrypt: emit them now.
    for msg in pending {
        if tx.send(msg).is_err() {
            break;
        }
        *sent_any = true;
    }
    Ok(())
}

/// The worker: run [`extract_pass`], resolving a *content* password on the first pre-emit failure
/// (header-plain / content-encrypted archives) and retrying from the start. `secret` starts as the
/// password that read the header (empty when the header was plain — 7z uses one password archive-wide).
fn worker(
    path: PathBuf,
    name: String,
    mut secret: Secret,
    pw: Arc<dyn PasswordProvider>,
    tx: SyncSender<SzMsg>,
) {
    let mut attempt = 0u32;
    loop {
        let mut sent_any = false;
        match extract_pass(&path, &secret, &tx, &mut sent_any) {
            Ok(()) => return,
            // A password failure before any entry was emitted → content is encrypted and our
            // header password (possibly empty) is wrong. Ask the provider and retry from scratch.
            Err(e) if is_password_error(&e) && !sent_any => {
                match pw.password(&PasswordRequest {
                    archive: &name,
                    entry: None,
                    for_header: false,
                    attempt,
                }) {
                    Some(s) => {
                        secret = s;
                        attempt += 1;
                    }
                    None => {
                        let _ = tx.send(SzMsg::Err(map_sevenz(e)));
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(SzMsg::Err(map_sevenz(e)));
                return;
            }
        }
    }
}

/// Streams one file entry's body from the worker channel, one chunk at a time. On drop it drains any
/// unread chunks up to `FileEnd`, so an entry the engine abandons early (e.g. a write error, where
/// the sequential path does not drain) still leaves the channel aligned to the next entry — the
/// "drain before the next `next_entry`" invariant stays local to this backend.
struct SzBody<'a> {
    rx: &'a Receiver<SzMsg>,
    cur: io::Cursor<Vec<u8>>,
    done: bool,
}

impl Read for SzBody<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.cur.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(SzMsg::Chunk(bytes)) => self.cur = io::Cursor::new(bytes),
                Ok(SzMsg::FileEnd) => {
                    self.done = true;
                    return Ok(0);
                }
                Ok(SzMsg::Err(e)) => {
                    self.done = true;
                    return Err(io::Error::other(e.to_string()));
                }
                Ok(_) => {
                    self.done = true;
                    return Err(io::Error::other("7z stream desync"));
                }
                Err(_) => {
                    self.done = true;
                    return Err(io::Error::other("7z worker ended mid-entry"));
                }
            }
        }
    }
}

impl Drop for SzBody<'_> {
    fn drop(&mut self) {
        // Discard any remaining chunks up to the entry boundary so the next entry starts clean.
        if self.done {
            return;
        }
        loop {
            match self.rx.recv() {
                Ok(SzMsg::FileEnd) | Ok(SzMsg::Err(_)) | Err(_) => break,
                Ok(_) => {} // leftover chunk → drop
            }
        }
    }
}

/// A 7z archive opened for sequential extraction. Metadata is read up front; bodies stream from a
/// worker thread on first use.
pub struct SevenZReader {
    path: PathBuf,
    name: String,
    /// The password that read the header (empty if the header was not encrypted). Reused as the
    /// starting point for content decryption.
    header_pw: Secret,
    pw: Arc<dyn PasswordProvider>,
    entries: Vec<Entry>,
    rx: Option<Receiver<SzMsg>>,
    started: bool,
}

impl SevenZReader {
    pub fn open(path: &Path, pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // Resolve the header password: try empty first, then ask the provider on a password error
        // (encrypted-names archives can't even be listed without it).
        let mut secret = Secret::new(String::new());
        let mut attempt = 0u32;
        let entries = loop {
            match read_metadata(path, &secret) {
                Ok(entries) => break entries,
                Err(e) if is_password_error(&e) => {
                    match pw.password(&PasswordRequest {
                        archive: &name,
                        entry: None,
                        for_header: true,
                        attempt,
                    }) {
                        Some(s) => {
                            secret = s;
                            attempt += 1;
                        }
                        None => {
                            return Err(if attempt == 0 {
                                ArchiveError::PasswordRequired
                            } else {
                                ArchiveError::WrongPassword
                            });
                        }
                    }
                }
                Err(e) => return Err(map_sevenz(e)),
            }
        };

        Ok(Self {
            path: path.to_path_buf(),
            name,
            header_pw: secret,
            pw,
            entries,
            rx: None,
            started: false,
        })
    }

    /// Spawn the extraction worker on first use.
    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let (tx, rx) = sync_channel::<SzMsg>(1); // bounded → backpressure, ~1–2 entries in flight
        let path = self.path.clone();
        let name = self.name.clone();
        let secret = self.header_pw.clone();
        let pw = Arc::clone(&self.pw);
        thread::spawn(move || worker(path, name, secret, pw, tx));
        self.rx = Some(rx);
    }
}

impl ArchiveReader for SevenZReader {
    fn format(&self) -> Format {
        Format::sevenz()
    }

    fn entries(&self) -> Result<&[Entry]> {
        Ok(&self.entries)
    }

    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>> {
        self.ensure_started();
        let rx = self.rx.as_ref().unwrap();
        match rx.recv() {
            Ok(SzMsg::FileStart(entry)) => Ok(Some(EntryStream {
                entry,
                body: Box::new(SzBody {
                    rx,
                    cur: io::Cursor::new(Vec::new()),
                    done: false,
                }),
                meta_final: true,
            })),
            Ok(SzMsg::Dir(entry)) => Ok(Some(EntryStream {
                entry,
                body: Box::new(io::empty()),
                meta_final: true,
            })),
            Ok(SzMsg::Err(e)) => Err(e),
            // A stray body message with no active entry means the stream is out of sync.
            Ok(SzMsg::Chunk(_)) | Ok(SzMsg::FileEnd) => {
                Err(ArchiveError::Backend("7z stream desync".into()))
            }
            Err(_) => Ok(None), // channel closed → done
        }
    }
}

#[cfg(test)]
mod mtime_guard_tests {
    use super::*;
    use sevenz_rust2::{ArchiveEntry, NtTime};

    #[test]
    fn seven_z_mtime_rejects_hostile_filetime_without_panicking() {
        let mut e = ArchiveEntry::new();
        // A near-`u64::MAX` FILETIME would overflow `NtTime -> SystemTime` — must be rejected, not
        // converted (the conversion uses a panicking `+`).
        e.has_last_modified_date = true;
        e.last_modified_date = NtTime::from(u64::MAX);
        assert!(seven_z_mtime(&e).is_none());
        // A realistic ~2020 FILETIME (≈1.32e17 ticks since 1601) converts fine.
        e.last_modified_date = NtTime::from(132_223_104_000_000_000);
        assert!(seven_z_mtime(&e).is_some());
        // The presence flag is honored.
        e.has_last_modified_date = false;
        assert!(seven_z_mtime(&e).is_none());
    }
}
