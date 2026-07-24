//! `cram-extract <archive.cram> <dest-dir> [-p <password>]` — a **standalone** extractor for the
//! `.cram` v1 format (see docs/CRAM_FORMAT.md).
//!
//! This binary shares **no code** with `cram-core`. It is an independent, from-the-spec reader whose
//! whole job is to get your files back out of a `.cram` with the smallest possible trusted surface:
//! four pure-Rust decode crates (XZ, zstd, AES-GCM, Argon2) and the standard library, nothing else —
//! no chunker, no hasher, no parallel engine, no writer. That makes it (a) a second, independent
//! implementation that validates the frozen format really is implementable from the document alone,
//! and (b) a tiny auditable recovery tool you can build and run even if the main Cram tool is gone.
//!
//! It enforces every mandatory reader check from spec §9 (bounds by subtraction, exact `raw_len`,
//! `size == Σ chunk length`, KDF caps, the anti-amplification budget, no allocation from untrusted
//! counts) and refuses unsafe entry paths, so pointing it at a hostile archive is safe.
//!
//! It also doubles as the **self-extractor toolkit**: `--make-sfx <archive.cram> <out.exe>` appends a
//! `.cram` payload to a copy of this binary, producing a standalone `.exe` that extracts itself on any
//! Windows machine with nothing installed — the interop escape hatch that keeps `.cram` from ever
//! being a dead end. When run with an embedded payload it self-extracts through this same safe reader.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};

// ---- format constants (docs/CRAM_FORMAT.md §2–§11) ----
const MAGIC: &[u8; 6] = b"CRAM\x1b\x01";
const VERSION: u8 = 1;
const HEADER_LEN: u64 = 8;
const TRAILER_LEN: u64 = 22;
const CRYPTO_BLOCK_LEN: u64 = 28; // salt(16) + m/t/p (u32 each)
const FLAG_ENCRYPTED: u8 = 0x01;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

const CODEC_STORE: u8 = 0;
const CODEC_XZ: u8 = 1;
const CODEC_ZSTD: u8 = 2;

const MAX_PACK_RAW: u64 = 64 * 1024 * 1024;
// Caps on the Argon2 params READ from an untrusted archive. Enforced before the KDF allocates, these
// bound both the memory AND the CPU a single password attempt can cost. Kept comfortably above the
// only writer's fixed 19 MiB / t=2 (see cram-core), but far below the old 1 GiB / t=64 ceiling, which
// let a hostile header demand ~64 GiB of memory traffic per "wrong password" — a nuisance DoS.
const MAX_ARGON_M: u32 = 262_144; // 256 MiB (in KiB)
const MAX_ARGON_T: u32 = 8;
const MAX_ARGON_P: u32 = 16;
const INDEX_AAD: &[u8] = b"cram-index";
/// Total decompression WORK may be at most `RE_DECODE_FACTOR × total output` (see `Archive::budget`).
const RE_DECODE_FACTOR: u64 = 16;
const MIN_DECOMP_BUDGET: u64 = 256 * 1024 * 1024;
/// Bound the decompressed-pack cache; dedup means packs are reused across entries, but a recovery
/// tool must stay memory-bounded on a large archive.
const CACHE_CAP: usize = 256 * 1024 * 1024;
/// Reject entry paths deeper than this many components (matches the reference reader's hostile-archive
/// depth guard, so a crafted deep path can't drive pathological per-component work).
const MAX_PATH_DEPTH: usize = 4096;

// ---- self-extracting (SFX) trailer ----
// An SFX executable is `<cram-extract stub bytes> ++ <.cram payload> ++ trailer`. The 24-byte trailer
// at EOF lets the stub find its own embedded payload: payload_offset(u64 LE) | payload_len(u64 LE) |
// magic(8). A plain `cram-extract` has no trailer, so it runs as the ordinary extractor.
const SFX_MAGIC: &[u8; 8] = b"CRAMSFX1";
const SFX_TRAILER_LEN: u64 = 24;

type R<T> = Result<T, String>;

fn err<T>(msg: impl Into<String>) -> R<T> {
    Err(msg.into())
}

// ---- bounds-checked cursor over the (decrypted) index bytes ----
struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.p.checked_add(n).ok_or("index overflow")?;
        let s = self.b.get(self.p..end).ok_or("index truncated")?;
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> R<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[derive(Clone, Copy)]
struct PackLoc {
    file_offset: u64,
    comp_len: u64,
    raw_len: u32,
    codec: u8,
}
#[derive(Clone, Copy)]
struct ChunkLoc {
    pack_id: u32,
    offset: u32,
    length: u32,
}
struct EntryMeta {
    /// Sanitized, forward-slash display path (set after `sanitize`); the raw index name before that.
    name: String,
    /// Safe relative filesystem path (set once the entry passes sanitization). Unsafe entries are
    /// dropped in `Archive::open`, so every retained entry has a real path here.
    safe: PathBuf,
    is_dir: bool,
    size: u64,
    chunk_ids: Vec<u32>,
}

/// Parse the plaintext index (§6). Counts are never used to pre-size an allocation (§9.11).
fn deserialize_index(buf: &[u8]) -> R<(Vec<PackLoc>, Vec<ChunkLoc>, Vec<EntryMeta>)> {
    let mut c = Cur::new(buf);
    let np = c.u32()?;
    let mut packs = Vec::new();
    for _ in 0..np {
        packs.push(PackLoc {
            file_offset: c.u64()?,
            comp_len: c.u64()?,
            raw_len: c.u32()?,
            codec: c.u8()?,
        });
    }
    let nc = c.u32()?;
    let mut chunks = Vec::new();
    for _ in 0..nc {
        chunks.push(ChunkLoc {
            pack_id: c.u32()?,
            offset: c.u32()?,
            length: c.u32()?,
        });
    }
    let ne = c.u32()?;
    let mut entries = Vec::new();
    for _ in 0..ne {
        let is_dir = c.u8()? != 0;
        let name_len = c.u32()? as usize;
        let name =
            String::from_utf8(c.take(name_len)?.to_vec()).map_err(|_| "entry name is not utf-8")?;
        let size = c.u64()?;
        let _mode = c.u32()?; // permissions — not applied by this minimal extractor
        let nci = c.u32()?;
        let mut chunk_ids = Vec::new();
        for _ in 0..nci {
            chunk_ids.push(c.u32()?);
        }
        entries.push(EntryMeta {
            name,
            safe: PathBuf::new(), // filled in Archive::open after sanitization
            is_dir,
            size,
            chunk_ids,
        });
    }
    Ok((packs, chunks, entries))
}

/// Windows reserved device names — matches the reference `is_reserved_dos_name`: the device is
/// matched on the stem before the first '.', ignoring the trailing spaces/dots that Win32 also
/// ignores (so `CON`, `CON.`, `CON ` and `CON.txt` all match), and covers `CONIN$`/`CONOUT$`,
/// `COM0-9`/`LPT0-9`, and the superscript `COM¹/²/³` forms newer Windows reserves.
fn is_reserved_dos(comp: &str) -> bool {
    let trimmed = comp.trim_end_matches([' ', '.']);
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let rest = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"));
    matches!(
        rest,
        Some(
            "0" | "1"
                | "2"
                | "3"
                | "4"
                | "5"
                | "6"
                | "7"
                | "8"
                | "9"
                | "\u{00B9}"
                | "\u{00B2}"
                | "\u{00B3}"
        )
    )
}

/// Normalize an archive entry name into a SAFE RELATIVE path, or `None` if it is unsafe. Mirrors the
/// reference reader's `EntryPath::from_raw`: rejects `..`, absolute/drive (`:`) and NUL, caps path
/// depth ([`MAX_PATH_DEPTH`]), and mangles Windows reserved device names (prefixing `_`) so the file
/// is kept under a safe name rather than opening a device. The caller joins the result under `dest`.
fn sanitize(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for comp in name.replace('\\', "/").split('/') {
        let c = match comp {
            "" | "." => continue,
            ".." => return None,
            c if c.contains(':') || c.contains('\0') => return None,
            c => c,
        };
        depth += 1;
        if depth > MAX_PATH_DEPTH {
            return None;
        }
        if is_reserved_dos(c) {
            out.push(format!("_{c}"));
        } else {
            out.push(c);
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn derive_key(password: &str, salt: &[u8], m: u32, t: u32, p: u32) -> R<[u8; 32]> {
    if m > MAX_ARGON_M || t > MAX_ARGON_T || p > MAX_ARGON_P {
        return err("unreasonable KDF parameters");
    }
    let params = Params::new(m, t, p, Some(32)).map_err(|e| format!("argon2 params: {e}"))?;
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    a2.hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("argon2: {e}"))?;
    Ok(key)
}

/// Open a sealed blob `nonce(12) | ct | tag(16)` with AAD; a GCM failure is a wrong password / tamper.
fn gcm_open(cipher: &Aes256Gcm, blob: &[u8], aad: &[u8]) -> R<Vec<u8>> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return err("encrypted blob too short");
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| "wrong password or corrupt/tampered archive".to_string())
}

/// Read exactly `len` bytes at `offset`.
fn read_at(file: &mut File, offset: u64, len: usize) -> R<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).map_err(io_err)?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).map_err(io_err)?;
    Ok(buf)
}

fn io_err(e: io::Error) -> String {
    format!("io error: {e}")
}

/// Decode one pack's raw bytes: decrypt (if encrypted), then decompress to EXACTLY `raw_len` (§9.8).
fn decode_pack(
    file: &mut File,
    p: &PackLoc,
    pack_id: u32,
    cipher: Option<&Aes256Gcm>,
) -> R<Vec<u8>> {
    let on_disk = read_at(file, p.file_offset, p.comp_len as usize)?;
    let comp = match cipher {
        Some(c) => gcm_open(c, &on_disk, &pack_id.to_le_bytes())?,
        None => on_disk,
    };
    let raw_len = p.raw_len as usize;
    let raw = match p.codec {
        CODEC_STORE => comp,
        CODEC_XZ => {
            // Bound to raw_len + 1: a stream decoding to MORE than raw_len yields raw_len+1 bytes and
            // is rejected by the exact-length check below (never silently truncated).
            let mut raw = Vec::with_capacity(raw_len);
            lzma_rust2::XzReader::new(comp.as_slice(), false)
                .take(p.raw_len as u64 + 1)
                .read_to_end(&mut raw)
                .map_err(|e| format!("xz decode: {e}"))?;
            raw
        }
        CODEC_ZSTD => {
            let mut raw = Vec::with_capacity(raw_len);
            ruzstd::decoding::StreamingDecoder::new(comp.as_slice())
                .map_err(|e| format!("zstd init: {e}"))?
                .take(p.raw_len as u64 + 1)
                .read_to_end(&mut raw)
                .map_err(|e| format!("zstd decode: {e}"))?;
            raw
        }
        other => return err(format!("unknown pack codec {other}")),
    };
    if raw.len() != raw_len {
        return err("pack decompressed to unexpected length");
    }
    Ok(raw)
}

/// A byte-bounded FIFO cache of decompressed packs (dedup reuses packs across entries).
struct PackCache {
    map: HashMap<u32, std::rc::Rc<Vec<u8>>>,
    order: std::collections::VecDeque<u32>,
    bytes: usize,
}
impl PackCache {
    fn new() -> Self {
        PackCache {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
            bytes: 0,
        }
    }
    fn insert(&mut self, id: u32, data: std::rc::Rc<Vec<u8>>) {
        if self.map.contains_key(&id) {
            return;
        }
        self.bytes = self.bytes.saturating_add(data.len());
        self.order.push_back(id);
        self.map.insert(id, data);
        while self.bytes > CACHE_CAP && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front() {
                if let Some(d) = self.map.remove(&old) {
                    self.bytes -= d.len();
                }
            }
        }
    }
}

struct Archive {
    file: File,
    packs: Vec<PackLoc>,
    chunks: Vec<ChunkLoc>,
    entries: Vec<EntryMeta>,
    cipher: Option<Aes256Gcm>,
    /// Σ of the entries' declared sizes = the bytes extraction will write; the budget scales off this.
    total_out: u64,
    cache: PackCache,
    /// Cumulative decompressed bytes across the WHOLE run. Metering per entry (the old scheme)
    /// let an archive whose entries each stay under the budget — but which evict each other's
    /// packs from the cache — multiply total decompression without bound: N entries × budget of
    /// CPU work from a sub-megabyte file. The anti-bomb budget must cover the whole extraction.
    decompressed: u64,
}

impl Archive {
    fn open(path: &Path, password: Option<&str>) -> R<Archive> {
        let mut file = File::open(path).map_err(io_err)?;
        let file_len = file.metadata().map_err(io_err)?.len();
        if file_len < HEADER_LEN + TRAILER_LEN {
            return err("file too small to be a .cram archive");
        }

        // Header (§2).
        let head = read_at(&mut file, 0, HEADER_LEN as usize)?;
        if &head[..6] != MAGIC {
            return err("bad .cram header magic");
        }
        if head[6] != VERSION {
            return err(format!("unsupported .cram version {}", head[6]));
        }
        if head[7] & !FLAG_ENCRYPTED != 0 {
            return err("unknown .cram header flags");
        }
        let encrypted = head[7] & FLAG_ENCRYPTED != 0;

        // Trailer (§7) → index location. Validate its magic BEFORE deriving any key: this proves the
        // file really is a .cram, so a forged header prefix can't force an (expensive) Argon2 pass on
        // an interactive open. The size gate above guarantees file_len >= HEADER_LEN + TRAILER_LEN.
        let trailer = read_at(&mut file, file_len - TRAILER_LEN, TRAILER_LEN as usize)?;
        if &trailer[16..22] != MAGIC {
            return err("bad .cram trailer magic");
        }

        // Crypto block (§3) + derive the cipher.
        let (cipher, packs_start) = if encrypted {
            if file_len < HEADER_LEN + CRYPTO_BLOCK_LEN + TRAILER_LEN {
                return err("encrypted .cram too small");
            }
            let cb = read_at(&mut file, HEADER_LEN, CRYPTO_BLOCK_LEN as usize)?;
            let salt = &cb[..SALT_LEN];
            let g = |i: usize| u32::from_le_bytes(cb[i..i + 4].try_into().unwrap());
            let (m, t, p) = (g(SALT_LEN), g(SALT_LEN + 4), g(SALT_LEN + 8));
            let password = password.ok_or("archive is encrypted; a password is required (-p)")?;
            let key = derive_key(password, salt, m, t, p)?;
            let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
            (Some(cipher), HEADER_LEN + CRYPTO_BLOCK_LEN)
        } else {
            (None, HEADER_LEN)
        };

        let index_offset = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let index_len = u64::from_le_bytes(trailer[8..16].try_into().unwrap());
        let packs_end = file_len - TRAILER_LEN;
        // Overflow-safe range check by subtraction (§9.4).
        if index_offset < packs_start
            || index_len > packs_end
            || index_offset > packs_end - index_len
        {
            return err("index location out of range");
        }

        // Read + (if encrypted) decrypt the index (§5, §8). The GCM tag is the password verifier.
        let index_blob = read_at(&mut file, index_offset, index_len as usize)?;
        let index_bytes = match &cipher {
            Some(c) => gcm_open(c, &index_blob, INDEX_AAD)?,
            None => index_blob,
        };

        let (packs, chunks, entries) = deserialize_index(&index_bytes)?;

        // Cross-reference validation (§9.5–§9.7).
        for p in &packs {
            if p.file_offset < packs_start
                || p.comp_len > index_offset
                || p.file_offset > index_offset - p.comp_len
                || p.raw_len as u64 > MAX_PACK_RAW
            {
                return err("pack location out of range");
            }
        }
        for c in &chunks {
            let pack = packs
                .get(c.pack_id as usize)
                .ok_or("chunk references unknown pack")?;
            if c.offset as u64 + c.length as u64 > pack.raw_len as u64 {
                return err("chunk out of pack bounds");
            }
        }
        for e in &entries {
            let mut sum = 0u64;
            for &id in &e.chunk_ids {
                let c = chunks
                    .get(id as usize)
                    .ok_or("entry references unknown chunk")?;
                sum = sum.saturating_add(c.length as u64);
            }
            if sum != e.size {
                return err("entry size does not match its chunk lengths");
            }
        }

        // Sanitize names and DROP any entry whose path is unsafe (traversal / device / too deep), so a
        // hostile name is never listed, never printed by --list (no terminal-escape injection), and
        // never extracted — matching the reference reader. Retained entries carry their safe path.
        let total = entries.len();
        let entries: Vec<EntryMeta> = entries
            .into_iter()
            .filter_map(|m| {
                let safe = sanitize(&m.name)?;
                let name = safe.to_string_lossy().replace('\\', "/");
                Some(EntryMeta { name, safe, ..m })
            })
            .collect();
        let dropped = total - entries.len();
        if dropped > 0 {
            eprintln!(
                "warning: dropped {dropped} entr{} with unsafe path(s)",
                if dropped == 1 { "y" } else { "ies" }
            );
        }

        let total_out = entries
            .iter()
            .map(|e| e.size)
            .fold(0u64, u64::saturating_add);

        Ok(Archive {
            file,
            packs,
            chunks,
            entries,
            cipher,
            total_out,
            cache: PackCache::new(),
            decompressed: 0,
        })
    }

    /// Anti-bomb ceiling on total decompression WORK for the whole extraction: `RE_DECODE_FACTOR ×
    /// the bytes extraction will actually write` (Σ entry sizes, each already checked == Σ its chunk
    /// lengths). Bounds work-vs-output amplification without rejecting a legitimately large,
    /// highly-compressible archive — basing it on `file_len × ratio` wrongly refused a sparse /
    /// low-entropy archive compressing >1000:1.
    fn budget(&self) -> u64 {
        self.total_out
            .saturating_mul(RE_DECODE_FACTOR)
            .max(MIN_DECOMP_BUDGET)
    }

    fn get_pack(&mut self, id: u32) -> R<std::rc::Rc<Vec<u8>>> {
        if let Some(hit) = self.cache.map.get(&id) {
            return Ok(hit.clone());
        }
        let p = *self.packs.get(id as usize).ok_or("bad pack id")?;
        let raw = std::rc::Rc::new(decode_pack(&mut self.file, &p, id, self.cipher.as_ref())?);
        self.decompressed = self.decompressed.saturating_add(raw.len() as u64);
        self.cache.insert(id, raw.clone());
        Ok(raw)
    }

    /// Reconstruct one entry's body into `out`, metering decompression against the anti-bomb budget
    /// (cumulative across the whole run — see `Archive::decompressed`).
    fn write_entry(&mut self, idx: usize, out: &mut dyn Write) -> R<()> {
        let ids = std::mem::take(&mut self.entries[idx].chunk_ids);
        let budget = self.budget();
        let mut result = Ok(());
        for &cid in &ids {
            let c = self.chunks[cid as usize]; // validated in open()
            let raw = match self.get_pack(c.pack_id) {
                Ok(r) => r,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            if self.decompressed > budget {
                result = err("excessive decompression (possible bomb)");
                break;
            }
            let (s, e) = (c.offset as usize, c.offset as usize + c.length as usize);
            match raw.get(s..e) {
                Some(slice) => {
                    if let Err(err) = out.write_all(slice) {
                        result = Err(io_err(err));
                        break;
                    }
                }
                None => {
                    result = err("chunk out of pack bounds");
                    break;
                }
            }
        }
        self.entries[idx].chunk_ids = ids; // restore
        result
    }

    /// Extract every entry under `dest`. Returns (files, dirs). Unsafe entries were already dropped at
    /// open, so every entry here carries a validated relative `safe` path — just join it under `dest`.
    fn extract_all(&mut self, dest: &Path) -> R<(u64, u64)> {
        fs::create_dir_all(dest).map_err(io_err)?;
        let (mut files, mut dirs) = (0u64, 0u64);
        for i in 0..self.entries.len() {
            let path = dest.join(&self.entries[i].safe);
            if self.entries[i].is_dir {
                fs::create_dir_all(&path).map_err(io_err)?;
                dirs += 1;
                continue;
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(io_err)?;
            }
            let f = File::create(&path).map_err(io_err)?;
            let mut w = io::BufWriter::new(f);
            self.write_entry(i, &mut w)?;
            w.flush().map_err(io_err)?;
            files += 1;
        }
        Ok((files, dirs))
    }

    fn list(&self) {
        for e in &self.entries {
            if e.is_dir {
                println!("  [d] {}", e.name);
            } else {
                println!("  [f] {} ({} bytes)", e.name, e.size);
            }
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: cram-extract [-p <password>] [--list] <archive.cram> <dest-dir>");
    eprintln!("       cram-extract [-p <password>] --list <archive.cram>");
    eprintln!("       cram-extract --make-sfx <archive.cram> <out.exe>   build a self-extractor");
    ExitCode::from(2)
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::FAILURE
}

/// Given a whole executable's bytes, return the embedded `.cram` payload slice iff a valid SFX trailer
/// is present at EOF. Pure + bounds-checked so it can be unit-tested and can never panic on junk input.
/// The runtime path is the streaming `embedded_payload` (same validity rule, without loading the whole
/// exe); this in-memory form remains as the unit-testable specification of the trailer parse.
#[cfg(test)]
fn payload_in(bytes: &[u8]) -> Option<&[u8]> {
    let total = bytes.len() as u64;
    if total < SFX_TRAILER_LEN {
        return None;
    }
    let t = &bytes[bytes.len() - SFX_TRAILER_LEN as usize..];
    if &t[16..24] != SFX_MAGIC {
        return None;
    }
    let offset = u64::from_le_bytes(t[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(t[8..16].try_into().unwrap());
    // The payload must sit entirely between the stub and the trailer.
    if len == 0 || offset.checked_add(len)?.checked_add(SFX_TRAILER_LEN)? != total {
        return None;
    }
    bytes.get(offset as usize..(offset + len) as usize)
}

/// If this executable carries an appended `.cram` payload (built with `--make-sfx`), return its bytes.
/// A plain `cram-extract` has no trailer → `None` → it behaves as the ordinary extractor. Any read
/// failure is treated as "no payload" so a normal invocation can never be derailed by this probe.
///
/// Reads ONLY the 24-byte trailer and then exactly the payload range — the old `fs::read` of the
/// whole exe plus a `to_vec` of the payload slice held ~2× the payload in RAM at peak (a 2 GiB SFX
/// transiently needed ~4 GiB).
fn embedded_payload() -> Option<Vec<u8>> {
    let exe = std::env::current_exe().ok()?;
    let mut f = File::open(&exe).ok()?;
    let total = f.metadata().ok()?.len();
    if total < SFX_TRAILER_LEN {
        return None;
    }
    let mut t = [0u8; SFX_TRAILER_LEN as usize];
    f.seek(SeekFrom::Start(total - SFX_TRAILER_LEN)).ok()?;
    f.read_exact(&mut t).ok()?;
    if &t[16..24] != SFX_MAGIC {
        return None;
    }
    let offset = u64::from_le_bytes(t[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(t[8..16].try_into().unwrap());
    // Same validity rule as `payload_in`: the payload sits entirely between stub and trailer.
    if len == 0 || offset.checked_add(len)?.checked_add(SFX_TRAILER_LEN)? != total {
        return None;
    }
    let mut payload = vec![0u8; len as usize];
    f.seek(SeekFrom::Start(offset)).ok()?;
    f.read_exact(&mut payload).ok()?;
    Some(payload)
}

/// If `bytes` is already an SFX (stub+payload+trailer), return just the stub — so re-wrapping doesn't
/// nest payloads. Otherwise return it unchanged.
fn stub_only(mut bytes: Vec<u8>) -> Vec<u8> {
    let total = bytes.len() as u64;
    if total >= SFX_TRAILER_LEN {
        let t = &bytes[bytes.len() - SFX_TRAILER_LEN as usize..];
        if &t[16..24] == SFX_MAGIC {
            let offset = u64::from_le_bytes(t[0..8].try_into().unwrap());
            let len = u64::from_le_bytes(t[8..16].try_into().unwrap());
            if offset
                .checked_add(len)
                .and_then(|s| s.checked_add(SFX_TRAILER_LEN))
                == Some(total)
            {
                bytes.truncate(offset as usize);
            }
        }
    }
    bytes
}

/// Build a self-extracting `.exe`: this tool's own bytes ++ the `.cram` payload ++ the trailer. The
/// result runs anywhere with neither Cram nor this tool installed.
fn run_make_sfx(payload_path: &Path, out_path: &Path) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return fail(&format!("cannot locate own executable: {e}")),
    };
    let stub = match fs::read(&exe) {
        Ok(b) => stub_only(b),
        Err(e) => return fail(&format!("read stub {}: {e}", exe.display())),
    };
    let payload = match fs::read(payload_path) {
        Ok(b) => b,
        Err(e) => return fail(&format!("read {}: {e}", payload_path.display())),
    };
    // The self-extractor only understands `.cram`, so refuse to embed anything else.
    if payload.len() < 6 || &payload[..6] != MAGIC {
        return fail("payload is not a .cram archive");
    }
    let mut out = stub;
    let offset = out.len() as u64;
    out.extend_from_slice(&payload);
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(SFX_MAGIC);
    if let Err(e) = fs::write(out_path, &out) {
        return fail(&format!("write {}: {e}", out_path.display()));
    }
    println!(
        "wrote self-extractor {} ({} bytes = {offset}-byte stub + {}-byte payload)",
        out_path.display(),
        out.len(),
        payload.len()
    );
    ExitCode::SUCCESS
}

/// SFX mode: extract the embedded payload. Stages it as a temp `.cram` and runs the normal reader, so
/// the extractor path (every §9 safety check) is shared verbatim.
fn run_self_extract(payload: Vec<u8>, dest: &Path, password: Option<&str>) -> ExitCode {
    let tmp = std::env::temp_dir().join(format!(
        "cram-sfx-{}-{}.cram",
        std::process::id(),
        payload.len()
    ));
    if let Err(e) = fs::write(&tmp, &payload) {
        return fail(&format!("stage embedded payload: {e}"));
    }
    let outcome = Archive::open(&tmp, password).and_then(|mut arc| arc.extract_all(dest));
    let _ = fs::remove_file(&tmp);
    match outcome {
        Ok((files, dirs)) => {
            println!("extracted {files} files, {dirs} dirs to {}", dest.display());
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 1. If we carry an embedded payload, self-extraction IS the job. Optional `[dest] [-p pw]`;
    //    default destination is the current directory (the payload keeps its own root folder).
    if let Some(payload) = embedded_payload() {
        let mut password: Option<String> = None;
        let mut dest: Option<PathBuf> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-p" | "--password" => {
                    i += 1;
                    match args.get(i) {
                        Some(p) => password = Some(p.clone()),
                        None => {
                            eprintln!("usage: <self-extractor> [<dest-dir>] [-p <password>]");
                            return ExitCode::from(2);
                        }
                    }
                }
                "-h" | "--help" => {
                    eprintln!(
                        "self-extracting archive — run it to extract into the current folder,"
                    );
                    eprintln!("or: <self-extractor> [<dest-dir>] [-p <password>]");
                    return ExitCode::from(2);
                }
                other => dest = Some(PathBuf::from(other)),
            }
            i += 1;
        }
        let dest = dest.unwrap_or_else(|| PathBuf::from("."));
        return run_self_extract(payload, &dest, password.as_deref());
    }

    // 2. Build-SFX mode.
    if let Some(pos) = args.iter().position(|a| a == "--make-sfx") {
        let (Some(payload), Some(out)) = (args.get(pos + 1), args.get(pos + 2)) else {
            eprintln!("usage: cram-extract --make-sfx <archive.cram> <out.exe>");
            return ExitCode::from(2);
        };
        return run_make_sfx(Path::new(payload), Path::new(out));
    }

    // 3. Ordinary extractor CLI.
    let mut password: Option<String> = None;
    let mut list = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" | "--password" => {
                i += 1;
                match args.get(i) {
                    Some(p) => password = Some(p.clone()),
                    None => return usage(),
                }
            }
            "--list" | "-l" => list = true,
            "-h" | "--help" => return usage(),
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    // Validate positional arity BEFORE opening the archive, so we never run the (expensive) Argon2
    // KDF on an encrypted archive only to then reject the command line. --list wants exactly the
    // archive; extract wants archive + dest. Extra positionals are an error, not silently ignored.
    let want = if list { 1 } else { 2 };
    if positional.len() != want {
        return usage();
    }
    let archive = PathBuf::from(&positional[0]);
    let dest = (!list).then(|| PathBuf::from(&positional[1]));

    let mut arc = match Archive::open(&archive, password.as_deref()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if list {
        println!("{} — {} entries", archive.display(), arc.entries.len());
        arc.list();
        return ExitCode::SUCCESS;
    }

    let dest = dest.expect("dest is present in extract mode (arity checked above)");
    match arc.extract_all(&dest) {
        Ok((files, dirs)) => {
            println!("extracted {files} files, {dirs} dirs to {}", dest.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_traversal_and_absolute() {
        // Traversal, drive-letter, and NUL names are refused outright.
        assert!(sanitize("../escape").is_none());
        assert!(sanitize("a/../../escape").is_none());
        assert!(sanitize("C:/Windows/system32").is_none()); // drive letter has ':'
        assert!(sanitize("a\0b").is_none());
        assert!(sanitize("").is_none()); // no real component
        assert!(sanitize("./.").is_none());
        // A leading slash is NOT traversal: the empty first component is dropped and the rest is a
        // safe relative path (matches the reference reader trimming leading slashes).
        let abs = sanitize("/etc/passwd").expect("leading slash yields a relative path");
        assert!(abs.ends_with("etc/passwd"));
        assert!(abs.is_relative());
    }

    #[test]
    fn sanitize_accepts_normal_nested_paths_and_caps_depth() {
        let p = sanitize("src/inner/file.txt").expect("normal path is safe");
        assert!(p.ends_with("src/inner/file.txt"));
        // Backslashes are normalized to forward slashes before splitting.
        let p2 = sanitize(r"a\b\c.txt").expect("backslash path is safe");
        assert!(p2.ends_with("a/b/c.txt"));
        // A pathologically deep path is rejected (hostile-archive guard).
        let deep = vec!["a"; MAX_PATH_DEPTH + 1].join("/");
        assert!(sanitize(&deep).is_none());
        assert!(sanitize(&vec!["a"; MAX_PATH_DEPTH].join("/")).is_some());
    }

    #[test]
    fn reserved_dos_names_matched_like_reference() {
        // Full parity with the reference matcher: stem-before-dot, trailing space/dot ignored.
        assert!(is_reserved_dos("CON"));
        assert!(is_reserved_dos("nul"));
        assert!(is_reserved_dos("Com1"));
        assert!(is_reserved_dos("LPT9.txt"));
        assert!(is_reserved_dos("com0")); // COM0 IS reserved
        assert!(is_reserved_dos("CON ")); // trailing space is ignored by Win32
        assert!(is_reserved_dos("CON.")); // trailing dot too
        assert!(is_reserved_dos("CONIN$"));
        assert!(!is_reserved_dos("console"));
        assert!(!is_reserved_dos("com10"));
        // Mangled (prefixed) so the file is kept under a safe name, not lost to the device.
        let p = sanitize("CON").unwrap();
        assert!(p.ends_with("_CON"));
    }

    #[test]
    fn deserialize_rejects_truncated_index() {
        // A bogus/short buffer must fail cleanly (Err), never panic or over-read.
        assert!(deserialize_index(&[]).is_err());
        assert!(deserialize_index(&[1, 0, 0, 0]).is_err()); // claims 1 pack, no bytes follow
    }

    #[test]
    fn deserialize_index_survives_fuzz() {
        // Thousands of random index buffers: every one must be Ok or Err, never a panic. The
        // bounds-checked `Cur` and "never pre-size from an untrusted count" rule are what guarantee it
        // (a huge declared count just fails on the first missing byte instead of allocating).
        let mut x = 0x1357_9BDF_2468_ACE0u64;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..8000 {
            let len = (next() % 512) as usize;
            let buf: Vec<u8> = (0..len).map(|_| (next() >> 24) as u8).collect();
            let _ = deserialize_index(&buf);
        }
    }

    /// Assemble `stub ++ payload ++ trailer` exactly as `run_make_sfx` does.
    fn build_sfx(stub: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut exe = stub.to_vec();
        let offset = exe.len() as u64;
        exe.extend_from_slice(payload);
        exe.extend_from_slice(&offset.to_le_bytes());
        exe.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        exe.extend_from_slice(SFX_MAGIC);
        exe
    }

    #[test]
    fn sfx_payload_is_located_and_junk_is_rejected() {
        let stub = b"MZ...fake windows stub bytes...".to_vec();
        let payload = b"CRAM\x1b\x01 pretend .cram payload bytes".to_vec();
        let exe = build_sfx(&stub, &payload);

        // The embedded payload is recovered exactly.
        assert_eq!(payload_in(&exe), Some(payload.as_slice()));
        // A plain stub (no trailer) carries nothing.
        assert_eq!(payload_in(&stub), None);
        // Too-short input never panics.
        assert_eq!(payload_in(b"short"), None);
        // A corrupt declared length (doesn't line up with the file) is refused.
        let mut bad = exe.clone();
        let n = bad.len();
        bad[n - 16..n - 8].copy_from_slice(&9_999u64.to_le_bytes());
        assert_eq!(payload_in(&bad), None);
    }

    #[test]
    fn stub_only_unwraps_and_is_idempotent() {
        let stub = b"MZ...stub...".to_vec();
        let exe = build_sfx(&stub, b"payload payload payload");
        // Re-wrapping strips the old payload back to the bare stub (no nesting).
        assert_eq!(stub_only(exe), stub);
        // A bare stub is returned unchanged.
        assert_eq!(stub_only(stub.clone()), stub);
    }
}
