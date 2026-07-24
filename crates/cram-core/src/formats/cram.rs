//! The native **`.cram`** content-defined-dedup format.
//!
//! Unlike the classic containers (which store each file's bytes once, compressed in isolation),
//! `.cram` splits every file body into **content-defined chunks** (FastCDC v2020), identifies each
//! chunk by its **BLAKE3 hash**, and stores each *unique* chunk exactly once, so identical files,
//! or files that merely share regions (versioned assets, repacked game data), collapse to a single
//! copy. Unique chunks are grouped into **solid packs** (~8 MiB) compressed as a whole (XZ/LZMA2,
//! pure-Rust; a pack that doesn't shrink is stored raw), and a **footer index** maps entries → chunk
//! lists and chunks → (pack, offset, length). Because chunks are individually addressable, `.cram`
//! implements [`RandomAccessReader`], extraction fans out on the parallel per-entry engine, and
//! [`read_range`](RandomAccessReader::read_range) is the on-access / mount primitive.
//!
//! The on-disk byte layout is **frozen** and specified normatively in
//! [`docs/CRAM_FORMAT.md`](../../../../docs/CRAM_FORMAT.md) (format version 1); the summary below is
//! a map, that document is the contract a third-party reader implements against.
//!
//! On-disk layout:
//! ```text
//!   [header]   magic(6) = CRAM\x1b\x01 | version(1) | flags(1)
//!   [packs]    pack0 bytes | pack1 bytes | …            (each = a compressed-or-stored blob)
//!   [index]    serialized pack/chunk/entry tables       (see `serialize_index`)
//!   [trailer]  index_offset(u64) | index_len(u64) | magic(6)
//! ```
//! The index sits at EOF so the writer can stream packs out single-pass as it chunks each body.
//!
//! **Encryption** (`flags` bit 0): a password derives a key via **Argon2id** over a
//! random per-archive salt (stored, with the cost params, in a crypto block after the header), and
//! every pack **and** the footer index are sealed with **AES-256-GCM** (compress-then-encrypt; a
//! fresh random nonce per blob; the pack's id / an index tag as AAD). The index's own GCM tag is the
//! password verifier, so a wrong password fails cleanly on open. `.cram` v1 always encrypts the index
//! too (the listing needs the password), the ContentsOnly/NamesToo split isn't exposed yet. A ProjFS
//! mount builds on `read_range`.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use fastcdc::v2020::StreamCDC;
use lzma_rust2::{XzOptions, XzReader, XzWriter};
use rayon::prelude::*;
use zeroize::Zeroizing;

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream, RandomAccessReader};
use crate::secret::{PasswordProvider, PasswordRequest};
use crate::sniff::CRAM_MAGIC;
use crate::writer::{ArchiveWriter, CreateOptions, CreateReport, Level, WriteHint};

const VERSION: u8 = 1;
/// Written in place of [`VERSION`] **only** when the archive actually contains a per-entry transform
/// (see [`XFORM_LEPTON`]). Both this crate's reader and the standalone `cram-extract` reject a version
/// they don't know, so an older build refuses such an archive outright instead of writing out the
/// transformed bytes as if they were the file, a clean failure rather than silent corruption. An
/// archive with no transformed entries stays v1 and older readers keep working.
const VERSION_XFORM: u8 = 2;
/// Offset of the version byte, patched at `finish` once it is known whether any transform was used.
const VERSION_OFFSET: u64 = 6;
const HEADER_LEN: u64 = 8; // magic(6) + version(1) + flags(1)

/// Entry stored exactly as it was read.
const XFORM_NONE: u8 = 0;
/// Entry stored as a Lepton stream; extraction reconstructs the original JPEG byte-for-byte.
const XFORM_LEPTON: u8 = 1;
/// Files above this are never transformed: the recompressor needs the whole image in memory, and a
/// huge mis-named file must not be able to balloon the writer's footprint.
const MAX_XFORM_INPUT: u64 = 256 * 1024 * 1024;
/// Largest ratio a stored transformed stream may claim to expand to. Real Lepton output is ~0.77× the
/// JPEG, so this is enormously generous; it exists only to keep a hostile index from declaring a
/// vast size to weaken the decompression-bomb budget.
const MAX_XFORM_EXPANSION: u64 = 64;
const TRAILER_LEN: u64 = 22; // index_offset(8) + index_len(8) + magic(6)

/// `flags` byte bit 0, the archive's packs + index are AES-256-GCM encrypted.
const FLAG_ENCRYPTED: u8 = 0x01;

// Pack compression codecs.
const CODEC_STORE: u8 = 0;
const CODEC_XZ: u8 = 1;
/// zstd packs, written only by a `zstd-c` build (C encoder, full levels), but **decodable by ANY
/// build** via the always-present pure-Rust `ruzstd` decoder, so `.cram` files stay cross-compatible.
const CODEC_ZSTD: u8 = 2;

/// Map the abstract 0–9 preset onto a zstd compression level (1–19; higher = smaller/slower).
#[cfg(feature = "zstd-c")]
fn zstd_level(preset: u32) -> i32 {
    (preset as i32 * 2).clamp(1, 19)
}

// --- encryption: per-pack AES-256-GCM, key = Argon2id(password, salt) ---
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Crypto block written right after the header when encrypted: salt(16) + m_cost/t_cost/p_cost (u32 each).
const CRYPTO_BLOCK_LEN: u64 = (SALT_LEN + 12) as u64;
/// Argon2id cost parameters (stored in the archive so they remain tunable). ~19 MiB / 2 passes = the
/// OWASP-recommended minimum, strong at rest, still fast enough to open interactively.
const ARGON_M_COST: u32 = 19_456;
const ARGON_T_COST: u32 = 2;
const ARGON_P_COST: u32 = 1;
/// Upper bounds on the Argon2 params READ from an (untrusted) archive. `Params::new` only enforces
/// minimums, so without these a hostile `.cram` could set `m_cost` to ~4 TiB and OOM the opener the
/// moment a password is supplied. These also bound the CPU/memory a *single* password attempt can
/// cost: generous vs our 19 MiB / t=2 defaults (13× / 4×) so future tuning stays valid, but far below
/// the old 1 GiB / t=64 ceiling that let a crafted header burn ~64 GiB of memory traffic per attempt.
const MAX_ARGON_M: u32 = 262_144; // 256 MiB (in KiB)
const MAX_ARGON_T: u32 = 8;
const MAX_ARGON_P: u32 = 16;
/// AAD tag binding the index blob to its role (packs use their `pack_id` as AAD).
const INDEX_AAD: &[u8] = b"cram-index";

/// Fill `buf` with OS CSPRNG bytes (BCryptGenRandom on Windows).
fn random_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| ArchiveError::Backend(format!("rng: {e}")))
}

/// Derive a 32-byte key from the password with Argon2id, returned in a [`Zeroizing`] wrapper so the
/// key bytes are wiped on drop wherever they land, the derived material never survives in an
/// un-zeroized copy (a bare `[u8; 32]` return is `Copy`, leaving the local behind after `Ok`).
fn derive_key(password: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<Zeroizing<[u8; 32]>> {
    // Reject absurd (attacker-supplied) costs before Argon2 tries to allocate `m` KiB of memory.
    if m > MAX_ARGON_M || t > MAX_ARGON_T || p > MAX_ARGON_P {
        return Err(corrupt("unreasonable KDF parameters"));
    }
    let params = Params::new(m, t, p, Some(32))
        .map_err(|e| ArchiveError::Backend(format!("argon2: {e}")))?;
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    a2.hash_password_into(password.as_bytes(), salt, key.as_mut_slice())
        .map_err(|e| ArchiveError::Backend(format!("argon2: {e}")))?;
    Ok(key)
}

/// AES-256-GCM sealer/opener. `seal` prepends a fresh random nonce; `open` maps a GCM auth failure
/// (wrong key / tampered blob) to [`ArchiveError::WrongPassword`]. Send+Sync (RustCrypto cipher).
struct Crypter {
    cipher: Aes256Gcm,
}

impl Crypter {
    fn new(key: &[u8; 32]) -> Self {
        Crypter {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)),
        }
    }

    /// plaintext → `nonce(12) || ciphertext || tag(16)`; `aad` binds context (pack id / index role).
    fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        random_bytes(&mut nonce_bytes)?;
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| ArchiveError::Backend("encryption failed".into()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// `nonce(12) || ciphertext || tag` → plaintext; auth failure → `WrongPassword`.
    fn open(&self, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        if blob.len() < NONCE_LEN + TAG_LEN {
            return Err(corrupt("encrypted blob too short"));
        }
        let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
        self.cipher
            .decrypt(Nonce::from_slice(nonce_bytes), Payload { msg: ct, aad })
            .map_err(|_| ArchiveError::WrongPassword)
    }
}

// FastCDC v2020 chunk sizing: ~64 KiB average (16 KiB min, 256 KiB max); a balance of dedup
// granularity against index size for a general-purpose archiver.
pub(crate) const CHUNK_MIN: u32 = 16 * 1024;
pub(crate) const CHUNK_AVG: u32 = 64 * 1024;
pub(crate) const CHUNK_MAX: u32 = 256 * 1024;
/// Flush a pack once its raw contents reach this size (it may overshoot by up to one chunk).
const PACK_TARGET: usize = 8 * 1024 * 1024;
/// Defensive ceiling on a single pack's raw size when reading an untrusted archive (guards the
/// decompression buffer against a corrupt/hostile `raw_len`). Comfortably above `PACK_TARGET`.
const MAX_PACK_RAW: usize = 64 * 1024 * 1024;
/// Anti-amplification: total decompression WORK for a whole extraction may be at most
/// `max(MIN_DECOMP_BUDGET, RE_DECODE_FACTOR × total_output)`, where `total_output` is the sum of the
/// entries' declared (and verified `size == Σ chunk length`) sizes, i.e. the bytes extraction will
/// actually write. This bounds *work relative to output*, so it catches the real bomb (a hostile
/// chunk list that re-decompresses the same packs so decompression ≫ output) WITHOUT rejecting a
/// legitimately large, highly-compressible archive (whose work ≈ output). Basing the bound on the
/// file size instead (`ratio × file_len`) wrongly rejected a sparse/low-entropy archive that
/// compresses >1000:1, so it is not used. The factor allows for FIFO-cache re-decompression during a
/// real extraction.
///
/// Known limitation (accepted): any work-vs-output bound must reject an archive that legitimately
/// decompresses ≫ its output. A heavy content-defined-dedup archive whose unique pack working set
/// exceeds [`PACK_CACHE_CAP`] AND whose per-entry references scatter across many packs with poor
/// locality can FIFO-thrash the cache and re-decode packs more than `RE_DECODE_FACTOR×`, tripping this
/// guard. Such archives are rare (normal, local, or under-cache-sized working sets stay far under the
/// factor), the failure is a clean error (never corruption), and lowering the guard to admit them
/// would re-open the amplification DoS, so the bound is kept.
const RE_DECODE_FACTOR: u64 = 16;
const MIN_DECOMP_BUDGET: u64 = 256 * 1024 * 1024;
/// Total bytes of decompressed packs kept in the shared cross-worker cache (see [`PackCache`]).
const PACK_CACHE_CAP: usize = 256 * 1024 * 1024;
/// Ceiling on a single entry buffered **whole in RAM** by the sequential reader ([`next_entry`],
/// which must materialize the body to hand back a `Read`). Extraction of real archives goes through
/// the random-access [`copy_entry`] path, which streams to disk unbounded, so this only bounds the
/// in-memory fallback, stopping a huge or hostile entry (e.g. a tiny archive whose chunk list
/// repeats one chunk millions of times) from OOM-aborting the process.
const MAX_INMEM_ENTRY: u64 = 512 * 1024 * 1024;

// Index model (shared by writer + reader)

/// Where one pack lives in the file and how it's encoded.
#[derive(Clone, Copy)]
struct PackLoc {
    file_offset: u64,
    comp_len: u64,
    raw_len: u32,
    codec: u8,
}

/// Where one unique chunk lives *within its pack's raw (decompressed) bytes*.
#[derive(Clone, Copy)]
struct ChunkLoc {
    pack_id: u32,
    offset: u32,
    length: u32,
}

/// Metadata for one archive member (the chunk list reconstructs a file body in order).
struct EntryMeta {
    name: String,
    is_dir: bool,
    /// **Logical** size, the length of the file the user gets back. For a transformed entry this is
    /// the original JPEG's length, not the length of the stored (smaller) stream, so listings and
    /// extraction report what the user actually has.
    size: u64,
    mode: u32,
    chunk_ids: Vec<u32>,
    /// Which reversible transform the stored bytes went through ([`XFORM_NONE`]/[`XFORM_LEPTON`]).
    transform: u8,
}

/// Does this name look like a JPEG? Only a hint for *whether to try*, the recompressor validates the
/// actual bytes and a mislabelled file simply falls back to being stored as-is.
fn looks_like_jpeg(name: &str) -> bool {
    let n = name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(n.as_str(), "jpg" | "jpeg" | "jpe" | "jfif")
}

/// Losslessly recompress a JPEG, returning `None` if it can't be (not a JPEG, an unsupported
/// variant, or the library declines).
///
/// Uses the verifying encoder, which decodes its own output and compares it to the input before
/// returning. Nothing is ever stored transformed unless it has already been proven to reconstruct
/// byte-for-byte, for irreplaceable photos, "probably reversible" is not good enough. Any failure is
/// simply a `None` and the caller stores the original bytes.
fn jpeg_recompress(data: &[u8]) -> Option<Vec<u8>> {
    let feats = lepton_jpeg::EnabledFeatures::compat_lepton_vector_write();
    let pool = lepton_jpeg::SingleThreadPool {};
    match lepton_jpeg::encode_lepton_verify(data, &feats, &pool) {
        // Only worth it if it actually got smaller.
        Ok((out, _)) if out.len() < data.len() => Some(out),
        _ => None,
    }
}

/// Reverse [`jpeg_recompress`], reconstructing the original JPEG exactly.
fn jpeg_restore(data: &[u8]) -> Result<Vec<u8>> {
    let feats = lepton_jpeg::EnabledFeatures::compat_lepton_vector_write();
    let pool = lepton_jpeg::SingleThreadPool {};
    let mut out = Vec::with_capacity(data.len() * 2);
    lepton_jpeg::decode_lepton(&mut Cursor::new(data), &mut out, &feats, &pool)
        .map_err(|e| corrupt(&format!("could not reconstruct a recompressed JPEG: {e}")))?;
    Ok(out)
}

/// Map the abstract [`Level`] onto the XZ 0–9 preset used for packs.
fn preset(level: Level) -> u32 {
    match level {
        Level::Auto | Level::Balanced => 6,
        Level::Fastest => 1,
        Level::Best => 9,
        Level::Explicit(n) => n.clamp(0, 9),
    }
}

// ---- index serialization (compact little-endian; the format is ours, kept inspectable) ----

fn put_u32(b: &mut Vec<u8>, x: u32) {
    b.extend_from_slice(&x.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, x: u64) {
    b.extend_from_slice(&x.to_le_bytes());
}

/// `version` selects the entry-record shape: v1 records carry no transform byte, so an archive that
/// used no transforms serializes byte-identically to before and older readers still accept it.
fn serialize_index(
    packs: &[PackLoc],
    chunks: &[ChunkLoc],
    entries: &[EntryMeta],
    version: u8,
) -> Vec<u8> {
    let mut b = Vec::new();
    put_u32(&mut b, packs.len() as u32);
    for p in packs {
        put_u64(&mut b, p.file_offset);
        put_u64(&mut b, p.comp_len);
        put_u32(&mut b, p.raw_len);
        b.push(p.codec);
    }
    put_u32(&mut b, chunks.len() as u32);
    for c in chunks {
        put_u32(&mut b, c.pack_id);
        put_u32(&mut b, c.offset);
        put_u32(&mut b, c.length);
    }
    put_u32(&mut b, entries.len() as u32);
    for e in entries {
        b.push(e.is_dir as u8);
        let name = e.name.as_bytes();
        put_u32(&mut b, name.len() as u32);
        b.extend_from_slice(name);
        put_u64(&mut b, e.size);
        put_u32(&mut b, e.mode);
        put_u32(&mut b, e.chunk_ids.len() as u32);
        for &id in &e.chunk_ids {
            put_u32(&mut b, id);
        }
        if version >= VERSION_XFORM {
            b.push(e.transform);
        }
    }
    b
}

/// A bounds-checked cursor over the index bytes, every read validates length so a truncated or
/// hostile index yields [`ArchiveError::Corrupt`] rather than a panic.
struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .p
            .checked_add(n)
            .ok_or_else(|| corrupt("index overflow"))?;
        let s = self
            .b
            .get(self.p..end)
            .ok_or_else(|| corrupt("index truncated"))?;
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

fn corrupt(msg: &str) -> ArchiveError {
    ArchiveError::Corrupt(msg.to_string())
}

/// `version` must be the archive header's version byte: it selects the entry-record shape, so a v1
/// index is read exactly as before and only a v2 index looks for the trailing transform byte.
fn deserialize_index(
    buf: &[u8],
    version: u8,
) -> Result<(Vec<PackLoc>, Vec<ChunkLoc>, Vec<EntryMeta>)> {
    let mut c = Cur::new(buf);

    // Counts come from untrusted bytes → never pre-allocate from them; push in a loop so a bogus
    // count simply runs the cursor out of bytes (Corrupt) instead of OOM'ing.
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
        let name = String::from_utf8(c.take(name_len)?.to_vec())
            .map_err(|_| corrupt("entry name is not utf-8"))?;
        let size = c.u64()?;
        let mode = c.u32()?;
        let nci = c.u32()?;
        let mut chunk_ids = Vec::new();
        for _ in 0..nci {
            chunk_ids.push(c.u32()?);
        }
        let transform = if version >= VERSION_XFORM {
            let t = c.u8()?;
            // An unknown transform means bytes this build cannot reverse. Refusing here is the
            // difference between a clear error and handing the caller a corrupt file.
            if t != XFORM_NONE && t != XFORM_LEPTON {
                return Err(corrupt(&format!("unknown entry transform {t}")));
            }
            t
        } else {
            XFORM_NONE
        };
        entries.push(EntryMeta {
            name,
            is_dir,
            size,
            mode,
            chunk_ids,
            transform,
        });
    }
    Ok((packs, chunks, entries))
}

/// The archive-relative name for an entry (normalized-safe, forward slashes).
fn cram_name(entry: &Entry) -> String {
    entry.path.safe().to_string_lossy().replace('\\', "/")
}

// Writer

pub struct CramArchiveWriter {
    out: BufWriter<File>,
    /// Bytes written so far = current file offset (header is written up front).
    pos: u64,
    /// XZ preset for pack compression.
    level: u32,
    /// Dedup table: BLAKE3 chunk hash → chunk id.
    seen: HashMap<[u8; 32], u32>,
    packs: Vec<PackLoc>,
    chunks: Vec<ChunkLoc>,
    entries: Vec<EntryMeta>,
    /// Raw bytes of the pack currently being filled.
    pack_buf: Vec<u8>,
    /// Total logical (pre-dedup) file bytes ingested.
    in_bytes: u64,
    /// Bytes eliminated by dedup (chunks that matched an already-stored chunk).
    dedup_saved: u64,
    /// Try lossless JPEG recompression on image entries (see [`jpeg_recompress`]).
    recompress_images: bool,
    /// Set once an entry is actually stored transformed; decides whether the header is patched to
    /// [`VERSION_XFORM`] at `finish`, so archives that used no transform stay v1-readable.
    used_transform: bool,
    /// `Some` when the archive is encrypted: packs and the index are AES-256-GCM sealed.
    crypter: Option<Crypter>,
    /// Id the currently-filling `pack_buf` will get (chunks reference this before the pack is written).
    next_pack_id: u32,
    /// Filled raw packs `(id, bytes)` awaiting a parallel-compression batch flush.
    pending: Vec<(u32, Vec<u8>)>,
    /// How many packs to compress in parallel per batch (bounds peak memory).
    batch: usize,
    /// Use zstd for packs (a `zstd-c` build) instead of XZ. Reader decodes either via ruzstd/lzma.
    use_zstd: bool,
    start: Instant,
}

/// Compress (and, when encrypting, seal) one pack's raw bytes into its on-disk payload. Pure and
/// thread-safe, packs are independent, so a whole batch compresses in parallel. Returns
/// `(payload, raw_len, codec)`; a pack that the codec doesn't shrink is stored raw so it never grows.
fn compress_pack(
    raw: Vec<u8>,
    pack_id: u32,
    level: u32,
    use_zstd: bool,
    crypter: Option<&Crypter>,
) -> Result<(Vec<u8>, u32, u8)> {
    let raw_len = raw.len() as u32;
    let (codec, plaintext) = pack_compress(raw, level, use_zstd)?;
    // Encrypt AFTER compression (compress-then-encrypt); AAD binds the pack to its id.
    let payload = match crypter {
        Some(cr) => cr.seal(&plaintext, &pack_id.to_le_bytes())?,
        None => plaintext,
    };
    Ok((payload, raw_len, codec))
}

/// Codec choice for one pack: zstd (fast, `zstd-c` build only) or XZ (default, best ratio). Returns
/// `(codec, bytes)`; stores raw if compression didn't shrink it.
fn pack_compress(raw: Vec<u8>, level: u32, use_zstd: bool) -> Result<(u8, Vec<u8>)> {
    #[cfg(feature = "zstd-c")]
    if use_zstd {
        let comp = zstd::bulk::compress(&raw, zstd_level(level))
            .map_err(|e| ArchiveError::Backend(format!("zstd encode: {e}")))?;
        return Ok(if comp.len() < raw.len() {
            (CODEC_ZSTD, comp)
        } else {
            (CODEC_STORE, raw)
        });
    }
    #[cfg(not(feature = "zstd-c"))]
    let _ = use_zstd; // XZ-only build: the flag is always false

    // Adaptive: skip the (slow) LZMA pass on incompressible packs. High-entropy data, already-
    // compressed media, game `.scs`/`.pak` archives, encrypted blobs; won't shrink, so LZMA just
    // burns CPU exhaustively searching for matches that aren't there, then stores it raw anyway. A
    // cheap sample verdict (entropy + a fast-deflate trial) catches that up front and stores
    // immediately, turning an incompressible `.cram` create from LZMA-bound into read-bound. (Same
    // store-the-incompressible policy the zip/7z auto-codec already applies per entry.)
    let sample = &raw[..raw.len().min(64 * 1024)];
    if crate::probe::sample_verdict(sample).is_store() {
        return Ok((CODEC_STORE, raw));
    }

    let mut w = XzWriter::new(Vec::new(), XzOptions::with_preset(level))?;
    w.write_all(&raw)?;
    let comp = w.finish()?;
    Ok(if comp.len() < raw.len() {
        (CODEC_XZ, comp)
    } else {
        (CODEC_STORE, raw)
    })
}

impl CramArchiveWriter {
    pub fn create(path: &Path, opts: &CreateOptions) -> Result<Self> {
        let mut out = BufWriter::new(File::create(path)?);

        // Encryption: derive a per-archive key with Argon2id over a random salt, and write the
        // header flag + crypto block (salt + Argon2 params) so the reader can re-derive it.
        // `.cram` v1 always encrypts the index too (the listing needs the password), the
        // ContentsOnly/NamesToo split isn't exposed yet, so HeaderMode is not consulted here.
        let (crypter, pos) = match &opts.encrypt {
            None => {
                out.write_all(CRAM_MAGIC)?;
                out.write_all(&[VERSION, 0u8])?;
                (None, HEADER_LEN)
            }
            Some(spec) => {
                let mut salt = [0u8; SALT_LEN];
                random_bytes(&mut salt)?;
                let key = derive_key(
                    spec.password.expose(),
                    &salt,
                    ARGON_M_COST,
                    ARGON_T_COST,
                    ARGON_P_COST,
                )?;
                let crypter = Crypter::new(&key);
                // `key` (Zeroizing) is wiped when it drops at the end of this arm.
                out.write_all(CRAM_MAGIC)?;
                out.write_all(&[VERSION, FLAG_ENCRYPTED])?;
                out.write_all(&salt)?;
                out.write_all(&ARGON_M_COST.to_le_bytes())?;
                out.write_all(&ARGON_T_COST.to_le_bytes())?;
                out.write_all(&ARGON_P_COST.to_le_bytes())?;
                (Some(crypter), HEADER_LEN + CRYPTO_BLOCK_LEN)
            }
        };

        Ok(Self {
            out,
            pos,
            level: preset(opts.level),
            seen: HashMap::new(),
            packs: Vec::new(),
            chunks: Vec::new(),
            entries: Vec::new(),
            pack_buf: Vec::new(),
            in_bytes: 0,
            dedup_saved: 0,
            recompress_images: opts.recompress_images,
            used_transform: false,
            crypter,
            next_pack_id: 0,
            pending: Vec::new(),
            // Compress up to this many packs in parallel; clamped so peak memory stays bounded
            // (~batch × PACK_TARGET of raw bytes buffered before a flush).
            batch: rayon::current_num_threads().clamp(1, 16),
            // In a `zstd-c` build, use the fast C zstd for packs by default, but honor `--best` by
            // falling back to XZ's stronger ratio. A pure-Rust build always uses XZ (flag stays false).
            use_zstd: cfg!(feature = "zstd-c") && !matches!(opts.level, Level::Best),
            start: Instant::now(),
        })
    }

    /// Chunk everything `src` yields into the dedup table and the current pack, returning the chunk
    /// ids. Shared by the plain path and the recompressed path so both dedup identically, two
    /// copies of one photo still collapse to a single stored copy.
    fn chunk_stream(&mut self, src: &mut dyn Read) -> Result<Vec<u32>> {
        let mut chunk_ids = Vec::new();
        let chunker = StreamCDC::new(src, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX);
        for chunk in chunker {
            let chunk = chunk.map_err(|e| ArchiveError::Backend(format!("chunker: {e}")))?;
            let data = chunk.data;
            self.in_bytes += data.len() as u64;
            let key = *blake3::hash(&data).as_bytes();
            if let Some(&id) = self.seen.get(&key) {
                self.dedup_saved += data.len() as u64;
                chunk_ids.push(id);
                continue;
            }
            let id = self.chunks.len() as u32;
            let loc = ChunkLoc {
                pack_id: self.next_pack_id,
                offset: self.pack_buf.len() as u32,
                length: data.len() as u32,
            };
            self.pack_buf.extend_from_slice(&data);
            self.chunks.push(loc);
            self.seen.insert(key, id);
            chunk_ids.push(id);
            if self.pack_buf.len() >= PACK_TARGET {
                self.queue_pack();
                if self.pending.len() >= self.batch {
                    self.flush_batch()?;
                }
            }
        }
        Ok(chunk_ids)
    }

    fn write_out(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Move the filled `pack_buf` into the pending batch under its id.
    fn queue_pack(&mut self) {
        if self.pack_buf.is_empty() {
            return;
        }
        self.pending
            .push((self.next_pack_id, std::mem::take(&mut self.pack_buf)));
        self.next_pack_id += 1;
    }

    /// Compress the pending batch **in parallel**, then write the results in id order (assigning
    /// each pack's file offset as it's written). Packs are byte-identical to the serial path, only
    /// the compression is parallelized, so the archive layout is unchanged.
    fn flush_batch(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        let level = self.level;
        let use_zstd = self.use_zstd;
        let crypter = self.crypter.as_ref();
        let results: Vec<(Vec<u8>, u32, u8)> = pending
            .into_par_iter()
            .map(|(id, raw)| compress_pack(raw, id, level, use_zstd, crypter))
            .collect::<Result<Vec<_>>>()?;
        // `pending` was id-ordered and `into_par_iter().collect()` preserves order, so `packs`
        // grows in id order → `packs[id]` is pack `id`.
        for (payload, raw_len, codec) in results {
            let loc = PackLoc {
                file_offset: self.pos,
                comp_len: payload.len() as u64,
                raw_len,
                codec,
            };
            self.write_out(&payload)?;
            self.packs.push(loc);
        }
        Ok(())
    }
}

impl ArchiveWriter for CramArchiveWriter {
    fn add_file(&mut self, entry: &Entry, body: &mut dyn Read, _hint: WriteHint) -> Result<()> {
        let name = cram_name(entry);

        // A JPEG is stored as a Lepton stream when that round-trips provably: zip and 7z get ~0% on
        // photos because the data is already entropy-coded, whereas re-doing that coding is worth
        // ~23% with the original file still reconstructable byte-for-byte. The whole image has to be
        // in memory for this, so it is bounded, and anything that isn't really a JPEG, is too big,
        // or fails to verify simply streams through unchanged.
        // The planned size is a cheap first gate purely to avoid buffering: without it, a 10 GB file
        // misnamed `.jpg` would still be read 256 MiB into memory before being rejected. It is only a
        // hint, the source can have changed since planning, and a source that reports 0 is treated as
        // unknown, so the read cap below remains the real guard.
        let plausible_size = entry.size == 0 || entry.size <= MAX_XFORM_INPUT;
        if self.recompress_images && plausible_size && looks_like_jpeg(&name) {
            let mut head = Vec::new();
            body.take(MAX_XFORM_INPUT + 1).read_to_end(&mut head)?;
            if head.len() as u64 <= MAX_XFORM_INPUT {
                if let Some(encoded) = jpeg_recompress(&head) {
                    let original_len = head.len() as u64;
                    let chunk_ids = self.chunk_stream(&mut Cursor::new(&encoded))?;
                    // Account the logical bytes, not the stored ones, so the report's ratio reflects
                    // what the user actually put in.
                    self.in_bytes += original_len.saturating_sub(encoded.len() as u64);
                    self.entries.push(EntryMeta {
                        name,
                        is_dir: false,
                        size: original_len, // what extraction will produce
                        mode: entry.unix_mode.unwrap_or(0),
                        chunk_ids,
                        transform: XFORM_LEPTON,
                    });
                    self.used_transform = true;
                    return Ok(());
                }
            }
            // Not transformable: chunk what was already read, then the rest of the body.
            let mut rest = Cursor::new(head).chain(body);
            let chunk_ids = self.chunk_stream(&mut rest)?;
            let size = chunk_ids
                .iter()
                .map(|&id| self.chunks[id as usize].length as u64)
                .sum();
            self.entries.push(EntryMeta {
                name,
                is_dir: false,
                size,
                mode: entry.unix_mode.unwrap_or(0),
                chunk_ids,
                transform: XFORM_NONE,
            });
            return Ok(());
        }

        // Chunk the *live* body; the stored size is the actual bytes chunked (not the plan-time
        // `entry.size`), so a source file that changed since planning can never desync the archive.
        let mut chunk_ids = Vec::new();
        let mut size = 0u64;
        let chunker = StreamCDC::new(body, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX);
        for chunk in chunker {
            let chunk = chunk.map_err(|e| ArchiveError::Backend(format!("chunker: {e}")))?;
            let data = chunk.data;
            size += data.len() as u64;
            self.in_bytes += data.len() as u64;
            let key = *blake3::hash(&data).as_bytes();
            if let Some(&id) = self.seen.get(&key) {
                self.dedup_saved += data.len() as u64;
                chunk_ids.push(id);
                continue;
            }
            let id = self.chunks.len() as u32;
            let loc = ChunkLoc {
                pack_id: self.next_pack_id, // the pack this buffer will become
                offset: self.pack_buf.len() as u32,
                length: data.len() as u32,
            };
            self.pack_buf.extend_from_slice(&data);
            self.chunks.push(loc);
            self.seen.insert(key, id);
            chunk_ids.push(id);
            if self.pack_buf.len() >= PACK_TARGET {
                self.queue_pack();
                if self.pending.len() >= self.batch {
                    self.flush_batch()?;
                }
            }
        }
        self.entries.push(EntryMeta {
            name: cram_name(entry),
            is_dir: false,
            size,
            mode: entry.unix_mode.unwrap_or(0),
            chunk_ids,
            transform: XFORM_NONE,
        });
        Ok(())
    }

    fn add_dir(&mut self, entry: &Entry) -> Result<()> {
        self.entries.push(EntryMeta {
            name: cram_name(entry),
            is_dir: true,
            size: 0,
            mode: entry.unix_mode.unwrap_or(0),
            chunk_ids: Vec::new(),
            transform: XFORM_NONE,
        });
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<CreateReport> {
        // Queue the final partial pack, then compress every remaining pack in parallel.
        self.queue_pack();
        self.flush_batch()?;
        let index_offset = self.pos;
        // Only an archive that actually used a transform is written as v2; everything else stays v1
        // and remains readable by older builds.
        let version = if self.used_transform {
            VERSION_XFORM
        } else {
            VERSION
        };
        let index = serialize_index(&self.packs, &self.chunks, &self.entries, version);
        // When encrypted, the index is sealed too; the listing needs the password, and the index's
        // own GCM tag doubles as the password verifier on open.
        let index = match &self.crypter {
            Some(cr) => cr.seal(&index, INDEX_AAD)?,
            None => index,
        };
        let index_len = index.len() as u64;
        self.write_out(&index)?;
        // Trailer: where the index starts + how long, then the magic as an end marker.
        let mut trailer = Vec::with_capacity(TRAILER_LEN as usize);
        put_u64(&mut trailer, index_offset);
        put_u64(&mut trailer, index_len);
        trailer.extend_from_slice(CRAM_MAGIC);
        self.write_out(&trailer)?;
        self.out.flush()?; // surface any deferred write error now

        // The header went out before the first file, so only now is it known whether any entry was
        // actually transformed. Patch the version byte in place: an older reader then refuses this
        // archive outright rather than handing back Lepton streams as if they were JPEGs.
        if self.used_transform {
            use std::io::Seek;
            let f = self.out.get_mut();
            f.seek(SeekFrom::Start(VERSION_OFFSET))?;
            f.write_all(&[VERSION_XFORM])?;
            f.flush()?;
        }

        // `pos` counted every byte written (header + packs + index + trailer) = the final size.
        Ok(CreateReport {
            entries: self.entries.len() as u64,
            in_bytes: self.in_bytes,
            out_bytes: self.pos,
            stored: 0,
            dedup_saved: self.dedup_saved,
            elapsed: self.start.elapsed(),
        })
    }
}

// Reader

/// A bounded, thread-safe cache of **decompressed** packs, shared across all extraction workers.
/// `.cram` dedup means many entries reference the same packs; without a shared cache, every worker
/// re-decompresses them, the extract CPU bottleneck. FIFO eviction bounded by total bytes.
struct PackCache {
    map: HashMap<u32, Arc<Vec<u8>>>,
    order: VecDeque<u32>,
    bytes: usize,
    cap: usize,
}

impl PackCache {
    fn new(cap: usize) -> Self {
        PackCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            cap,
        }
    }
    fn get(&self, id: u32) -> Option<Arc<Vec<u8>>> {
        self.map.get(&id).cloned()
    }
    fn insert(&mut self, id: u32, data: Arc<Vec<u8>>) {
        if self.map.contains_key(&id) {
            return; // another worker already cached it
        }
        self.bytes = self.bytes.saturating_add(data.len());
        self.order.push_back(id);
        self.map.insert(id, data);
        // Evict oldest until under cap (always keep at least one entry).
        while self.bytes > self.cap && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front() {
                if let Some(d) = self.map.remove(&old) {
                    self.bytes -= d.len();
                }
            }
        }
    }
}

pub struct CramReader {
    path: PathBuf,
    packs: Vec<PackLoc>,
    chunks: Vec<ChunkLoc>,
    /// Public entry list (index-aligned with `entry_chunks`); unsafe-named entries are dropped.
    entries: Vec<Entry>,
    /// Chunk-id list per entry, aligned to `entries`.
    entry_chunks: Vec<Vec<u32>>,
    /// Per-entry transform to reverse on the way out (index-aligned with `entries`).
    entry_transforms: Vec<u8>,
    /// `Some` when the archive is encrypted (packs are AES-256-GCM sealed).
    crypter: Option<Crypter>,
    /// Decompressed packs shared across concurrent `copy_entry` workers (kills re-decompression).
    pack_cache: Mutex<PackCache>,
    /// Anti-bomb ceiling on total decompression WORK for a whole extraction (see [`RE_DECODE_FACTOR`]).
    budget: u64,
    /// Cumulative bytes decompressed by the EXTRACTION paths (`reconstruct` / `copy_entry`) over this
    /// reader's life, charged once per cache-miss decode (re-decodes of evicted packs count too). The
    /// mount path (`read_range`) does NOT touch this, it meters each call independently, and every
    /// extraction/verify uses a fresh reader, so this is effectively per-operation and never starves a
    /// long-lived mount.
    decompressed: AtomicU64,
    cursor: usize,
}

impl CramReader {
    pub fn open(path: &Path, pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < HEADER_LEN + TRAILER_LEN {
            return Err(corrupt("file too small to be a .cram archive"));
        }

        // Validate header magic, version, and flags. The format is frozen at v1: an unknown version
        // or an unknown (reserved) flag bit means the archive was written by a newer/other producer
        // whose layout this reader can't assume, so reject cleanly rather than silently misparse it
        // as v1 (forward-compatibility guarantee of the frozen `.cram` spec; see docs/CRAM_FORMAT.md).
        let mut head = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut head)?;
        if &head[..CRAM_MAGIC.len()] != CRAM_MAGIC {
            return Err(corrupt("bad .cram header magic"));
        }
        // v1 and v2 differ only in the entry record's trailing transform byte. Anything else is a
        // format this build cannot read, and refusing is what keeps an older reader from mistaking a
        // transformed stream for the file itself.
        let version = head[6];
        if version != VERSION && version != VERSION_XFORM {
            return Err(corrupt("unsupported .cram version"));
        }
        if head[7] & !FLAG_ENCRYPTED != 0 {
            return Err(corrupt("unknown .cram header flags"));
        }
        let encrypted = head[7] & FLAG_ENCRYPTED != 0;

        // Crypto block (encrypted only): salt + Argon2 params. Packs then start after it.
        let (salt, argon_m, argon_t, argon_p, packs_start) = if encrypted {
            if file_len < HEADER_LEN + CRYPTO_BLOCK_LEN + TRAILER_LEN {
                return Err(corrupt("encrypted .cram too small"));
            }
            let mut cb = [0u8; CRYPTO_BLOCK_LEN as usize];
            file.read_exact(&mut cb)?;
            let mut salt = [0u8; SALT_LEN];
            salt.copy_from_slice(&cb[..SALT_LEN]);
            let g = |i: usize| u32::from_le_bytes(cb[i..i + 4].try_into().unwrap());
            (
                salt,
                g(SALT_LEN),
                g(SALT_LEN + 4),
                g(SALT_LEN + 8),
                HEADER_LEN + CRYPTO_BLOCK_LEN,
            )
        } else {
            ([0u8; SALT_LEN], 0, 0, 0, HEADER_LEN)
        };

        // Read the trailer to find the index.
        file.seek(SeekFrom::Start(file_len - TRAILER_LEN))?;
        let mut trailer = [0u8; TRAILER_LEN as usize];
        file.read_exact(&mut trailer)?;
        if &trailer[16..22] != CRAM_MAGIC {
            return Err(corrupt("bad .cram trailer magic"));
        }
        let index_offset = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let index_len = u64::from_le_bytes(trailer[8..16].try_into().unwrap());
        // The index must sit wholly within the packs region [packs_start, packs_end). Compare via
        // subtraction (never `index_offset + index_len`, which is attacker-controlled u64 that could
        // wrap past the check and drive a huge `vec![0u8; index_len]`). `packs_end` can't underflow,
        // the size gate above guarantees `file_len >= HEADER_LEN + TRAILER_LEN`.
        let packs_end = file_len - TRAILER_LEN;
        if index_offset < packs_start
            || index_len > packs_end
            || index_offset > packs_end - index_len
        {
            return Err(corrupt("index location out of range"));
        }

        // Read the index blob (encrypted archives seal it too).
        file.seek(SeekFrom::Start(index_offset))?;
        let mut index_blob = vec![0u8; index_len as usize];
        file.read_exact(&mut index_blob)?;

        // Decrypt the index if encrypted: derive the key (Argon2id over the stored salt/params) and
        // open the sealed blob; the GCM auth tag IS the password check. Re-ask on a wrong password.
        let (crypter, index_bytes) = if encrypted {
            let archive = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let mut attempt = 0;
            loop {
                let req = PasswordRequest {
                    archive,
                    entry: None,
                    for_header: true,
                    attempt,
                };
                let Some(secret) = pw.password(&req) else {
                    return Err(if attempt == 0 {
                        ArchiveError::PasswordRequired
                    } else {
                        ArchiveError::WrongPassword
                    });
                };
                let key = derive_key(secret.expose(), &salt, argon_m, argon_t, argon_p)?;
                let cr = Crypter::new(&key);
                // `key` (Zeroizing) is wiped when it drops at the end of this block.
                match cr.open(&index_blob, INDEX_AAD) {
                    Ok(plain) => break (Some(cr), plain),
                    Err(ArchiveError::WrongPassword) => {
                        attempt += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            (None, index_blob)
        };

        let (packs, chunks, metas) = deserialize_index(&index_bytes, version)?;

        // Validate cross-references so extraction can index without bounds-panicking, and so a
        // hostile archive can't point a pack read outside the pack region or OOM the decompressor.
        for p in &packs {
            // Overflow-safe bound: the pack's [file_offset, file_offset+comp_len) must lie within
            // [packs_start, index_offset). Subtraction form avoids the wrapping `file_offset+comp_len`
            // that could otherwise defeat the check and drive a hostile `vec![0u8; comp_len]`.
            if p.file_offset < packs_start
                || p.comp_len > index_offset
                || p.file_offset > index_offset - p.comp_len
                || p.raw_len as usize > MAX_PACK_RAW
            {
                return Err(corrupt("pack location out of range"));
            }
        }
        for c in &chunks {
            let pack = packs
                .get(c.pack_id as usize)
                .ok_or_else(|| corrupt("chunk references unknown pack"))?;
            if c.offset as u64 + c.length as u64 > pack.raw_len as u64 {
                return Err(corrupt("chunk out of pack bounds"));
            }
        }

        // Build the public entry list, dropping any unsafe names (keeps entries/entry_chunks aligned).
        let mut entries = Vec::new();
        let mut entry_chunks = Vec::new();
        let mut entry_transforms = Vec::new();
        for m in metas {
            // Validate chunk-id bounds AND that the chunk lengths sum to the declared size. The
            // writer guarantees `size == Σ chunk.length` (it does `size += data.len()` and pushes an
            // id per chunk, deduped or not), so enforcing it here makes `entry.size` authoritative,
            // reconstruction yields exactly that many bytes, and rejects an inconsistent/hostile
            // index whose declared size disagrees with its chunk list. Overflow-safe (saturating).
            let mut sum = 0u64;
            for &id in &m.chunk_ids {
                let c = chunks
                    .get(id as usize)
                    .ok_or_else(|| corrupt("entry references unknown chunk"))?;
                sum = sum.saturating_add(c.length as u64);
            }
            if m.transform == XFORM_NONE {
                if sum != m.size {
                    return Err(corrupt("entry size does not match its chunk lengths"));
                }
            } else {
                // A transformed entry stores a *smaller* stream than the file it reconstructs, so the
                // sizes deliberately differ and the equality above cannot apply. The declared size is
                // still bounded here, otherwise a hostile index could claim an enormous one purely to
                // inflate the anti-bomb budget below, and it is checked exactly against the
                // reconstructed length at extraction time, which is the real guarantee.
                if sum == 0 || m.size > sum.saturating_mul(MAX_XFORM_EXPANSION) {
                    return Err(corrupt("implausible size for a recompressed entry"));
                }
            }
            let Some(path) = EntryPath::from_raw(&m.name) else {
                continue; // path-traversal / unsafe name → not listed, not extracted
            };
            let idx = entries.len();
            entries.push(Entry {
                index: idx,
                path,
                kind: if m.is_dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                size: m.size,
                compressed_size: None,
                modified: None,
                unix_mode: (m.mode != 0).then_some(m.mode),
                crc32: None,
                encrypted: false,
            });
            entry_chunks.push(m.chunk_ids);
            entry_transforms.push(m.transform);
        }

        // Output-scaled anti-bomb budget: total decompression work ≤ RE_DECODE_FACTOR × the bytes we
        // will actually write (Σ entry sizes, each already checked == Σ its chunk lengths). Bounds
        // work-vs-output amplification without rejecting a legitimately large, highly-compressible
        // archive.
        let total_out: u64 = entries
            .iter()
            .map(|e| e.size)
            .fold(0u64, u64::saturating_add);
        let budget = total_out
            .saturating_mul(RE_DECODE_FACTOR)
            .max(MIN_DECOMP_BUDGET);

        Ok(Self {
            path: path.to_path_buf(),
            packs,
            chunks,
            entries,
            entry_chunks,
            entry_transforms,
            crypter,
            pack_cache: Mutex::new(PackCache::new(PACK_CACHE_CAP)),
            budget,
            decompressed: AtomicU64::new(0),
            cursor: 0,
        })
    }

    /// Fetch a decompressed pack, sharing it across workers. Returns `(bytes, decompressed_now)`;
    /// `decompressed_now` is true only on a cache miss (so callers charge the anti-bomb budget once).
    /// The pack is decompressed WITHOUT holding the cache lock, so workers decompress in parallel.
    fn get_pack(&self, file: &mut File, pack_id: u32) -> Result<(Arc<Vec<u8>>, bool)> {
        if let Some(hit) = self.pack_cache.lock().unwrap().get(pack_id) {
            return Ok((hit, false));
        }
        let raw = Arc::new(self.read_pack(file, pack_id)?);
        self.pack_cache.lock().unwrap().insert(pack_id, raw.clone());
        Ok((raw, true))
    }

    /// The whole-extraction decompression budget (see [`RE_DECODE_FACTOR`]).
    fn decomp_budget(&self) -> u64 {
        self.budget
    }

    /// Read (decrypt if needed) + decompress pack `pack_id` into its raw bytes.
    fn read_pack(&self, file: &mut File, pack_id: u32) -> Result<Vec<u8>> {
        let p = self
            .packs
            .get(pack_id as usize)
            .ok_or_else(|| corrupt("bad pack id"))?;
        file.seek(SeekFrom::Start(p.file_offset))?;
        // `comp_len` is bounded to the packs region (< index_offset < file_len) by `open`, so this
        // allocation can't exceed the real file size.
        let mut on_disk = vec![0u8; p.comp_len as usize];
        file.read_exact(&mut on_disk)?;
        // Decrypt first (compress-then-encrypt on write → decrypt-then-decompress on read); the
        // pack's `pack_id` is the AAD, so a reordered/foreign pack fails the GCM check.
        let comp = match &self.crypter {
            Some(cr) => cr.open(&on_disk, &pack_id.to_le_bytes())?,
            None => on_disk,
        };
        let raw = match p.codec {
            CODEC_STORE => comp,
            CODEC_XZ => {
                // BOUND the decompressor to `raw_len + 1` (raw_len itself is capped at MAX_PACK_RAW in
                // `open`): a decompression bomb otherwise grows `raw` without limit (OOM). The one
                // extra byte is a sentinel, a stream that decodes to MORE than raw_len yields
                // raw_len+1 bytes and is rejected by the exact-length check below, so an over-long
                // pack is refused rather than silently truncated (matches the "exactly raw_len" spec).
                let mut raw = Vec::with_capacity(p.raw_len as usize);
                XzReader::new(comp.as_slice(), false)
                    .take(p.raw_len as u64 + 1)
                    .read_to_end(&mut raw)?;
                raw
            }
            // zstd packs decode with the always-present pure-Rust ruzstd (one frame per pack), bounded
            // to raw_len(+1 sentinel) like XZ, so any build reads a zstd-c-written `.cram`.
            CODEC_ZSTD => {
                let mut raw = Vec::with_capacity(p.raw_len as usize);
                ruzstd::decoding::StreamingDecoder::new(comp.as_slice())
                    .map_err(|e| corrupt(&format!("zstd decode: {e}")))?
                    .take(p.raw_len as u64 + 1)
                    .read_to_end(&mut raw)?;
                raw
            }
            other => return Err(corrupt(&format!("unknown pack codec {other}"))),
        };
        // A pack must decompress to EXACTLY the raw_len its index claims: a shorter result means a
        // chunk offset validated against raw_len could read past the real bytes; a longer result (the
        // +1 sentinel above makes it observable) means the archive is malformed/hostile. Reject both.
        if raw.len() != p.raw_len as usize {
            return Err(corrupt("pack decompressed to unexpected length"));
        }
        Ok(raw)
    }

    /// Reconstruct an entry's body (given its chunk-id list) into `out`, pulling each pack from the
    /// shared [`PackCache`] (decompressed once, reused across all workers) with a one-deep per-call
    /// front cache for consecutive same-pack chunks.
    /// The transform recorded for an entry (`XFORM_NONE` when the index predates transforms).
    fn transform_of(&self, index: usize) -> u8 {
        self.entry_transforms
            .get(index)
            .copied()
            .unwrap_or(XFORM_NONE)
    }

    /// Reassemble a transformed entry's stored stream and reverse the transform, yielding the exact
    /// original file.
    ///
    /// The reconstructed length is checked against the size the index declared. That is the promise
    /// this whole feature rests on, the bytes handed back are the bytes that went in, so it is
    /// verified on the way out rather than assumed from the writer having verified on the way in.
    fn restore_entry(&self, index: usize, chunk_ids: &[u32]) -> Result<Vec<u8>> {
        let mut stored = Vec::new();
        self.reconstruct(chunk_ids, &mut stored)?;
        let restored = jpeg_restore(&stored)?;
        let expected = self.entries.get(index).map(|e| e.size).unwrap_or(0);
        if restored.len() as u64 != expected {
            return Err(corrupt(&format!(
                "recompressed entry reconstructed to {} bytes, expected {expected}",
                restored.len()
            )));
        }
        Ok(restored)
    }

    fn reconstruct(&self, chunk_ids: &[u32], out: &mut dyn Write) -> Result<u64> {
        let mut file = File::open(&self.path)?;
        // One-deep per-call cache (an `Arc` into the shared cache) avoids re-locking the shared
        // cache for consecutive chunks in the same pack.
        let mut cached: Option<(u32, Arc<Vec<u8>>)> = None;
        let mut written = 0u64;
        let budget = self.decomp_budget();
        for &cid in chunk_ids {
            let c = self.chunks[cid as usize]; // validated in `open`
            let raw = match &cached {
                Some((id, arc)) if *id == c.pack_id => arc.clone(),
                _ => {
                    let (arc, decompressed_now) = self.get_pack(&mut file, c.pack_id)?;
                    if decompressed_now {
                        // Charge against the CUMULATIVE, whole-extraction budget (see `decompressed`):
                        // a per-call reset would let a many-entry archive re-decompress the same packs
                        // without bound (work ≫ output).
                        let total = self
                            .decompressed
                            .fetch_add(arc.len() as u64, Ordering::Relaxed)
                            .saturating_add(arc.len() as u64);
                        if total > budget {
                            return Err(corrupt("excessive decompression (possible bomb)"));
                        }
                    }
                    cached = Some((c.pack_id, arc.clone()));
                    arc
                }
            };
            let (s, e) = (c.offset as usize, c.offset as usize + c.length as usize);
            let slice = raw
                .get(s..e)
                .ok_or_else(|| corrupt("chunk out of pack bounds"))?;
            out.write_all(slice)?;
            written += slice.len() as u64;
        }
        Ok(written)
    }
}

impl ArchiveReader for CramReader {
    fn format(&self) -> Format {
        Format::cram(crate::format::Codec::None)
    }

    fn entries(&self) -> Result<&[Entry]> {
        Ok(&self.entries)
    }

    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>> {
        if self.cursor >= self.entries.len() {
            return Ok(None);
        }
        let i = self.cursor;
        self.cursor += 1;
        let entry = self.entries[i].clone();
        let mut buf = Vec::new();
        if !entry.is_dir() {
            // The sequential path buffers the whole body in RAM to hand back a `Read`. `entry.size`
            // is authoritative (validated at open to equal the chunk-length sum), so refuse an entry
            // too large to hold in memory before reconstructing, it would otherwise OOM the process
            // (a hostile chunk list can force huge output from a tiny archive). Big legitimate
            // entries extract via the random-access `copy_entry` path, which streams to disk.
            if entry.size > MAX_INMEM_ENTRY {
                return Err(corrupt(
                    "entry too large to buffer in memory; extract via random access",
                ));
            }
            let ids = self.entry_chunks[i].clone();
            self.reconstruct(&ids, &mut buf)?;
        }
        Ok(Some(EntryStream {
            entry,
            body: Box::new(Cursor::new(buf)),
            meta_final: true,
        }))
    }

    fn as_random_access(&self) -> Option<&dyn RandomAccessReader> {
        Some(self)
    }
}

impl RandomAccessReader for CramReader {
    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn copy_entry(&self, index: usize, out: &mut dyn Write) -> Result<u64> {
        let ids = self
            .entry_chunks
            .get(index)
            .ok_or_else(|| corrupt("bad entry index"))?;
        if self.transform_of(index) == XFORM_LEPTON {
            let restored = self.restore_entry(index, ids)?;
            out.write_all(&restored)?;
            return Ok(restored.len() as u64);
        }
        self.reconstruct(ids, out)
    }

    fn read_range(&self, index: usize, off: u64, len: u64) -> Result<Vec<u8>> {
        let ids = self
            .entry_chunks
            .get(index)
            .ok_or_else(|| corrupt("bad entry index"))?;
        // A Lepton stream cannot be seeked into, the whole image is one arithmetic-coded unit, so a
        // ranged read reconstructs the entry and slices the result. That keeps the ProjFS mount and
        // every other random-access consumer working transparently on recompressed photos; the cost is
        // whole-file work per range, which is acceptable for image-sized entries.
        if self.transform_of(index) == XFORM_LEPTON {
            let restored = self.restore_entry(index, ids)?;
            let start = (off as usize).min(restored.len());
            let stop = start.saturating_add(len as usize).min(restored.len());
            return Ok(restored[start..stop].to_vec());
        }
        let end = off.saturating_add(len);
        let mut out = Vec::new();
        let mut file = File::open(&self.path)?;
        let mut cached: Option<(u32, Arc<Vec<u8>>)> = None;
        let mut cur = 0u64; // running offset within the entry's uncompressed stream
        let budget = self.decomp_budget();
        let mut decompressed = 0u64;
        for &cid in ids {
            if cur >= end {
                break;
            }
            let c = self.chunks[cid as usize];
            let clen = c.length as u64;
            let (cstart, cend) = (cur, cur + clen);
            cur = cend;
            if cend <= off {
                continue; // chunk entirely before the requested range
            }
            if cached.as_ref().map(|(id, _)| *id) != Some(c.pack_id) {
                let (arc, decompressed_now) = self.get_pack(&mut file, c.pack_id)?;
                if decompressed_now {
                    decompressed = decompressed.saturating_add(arc.len() as u64);
                    if decompressed > budget {
                        return Err(corrupt("excessive decompression (possible bomb)"));
                    }
                }
                cached = Some((c.pack_id, arc));
            }
            let raw = &cached.as_ref().unwrap().1;
            let base = c.offset as usize;
            let lo = (off.max(cstart) - cstart) as usize;
            let hi = (end.min(cend) - cstart) as usize;
            let slice = raw
                .get(base + lo..base + hi)
                .ok_or_else(|| corrupt("chunk out of pack bounds"))?;
            out.extend_from_slice(slice);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::NoPassword;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// In a `zstd-c` build, packs are compressed with zstd (codec 2); incompressible packs are stored
    /// raw (never grown). Cross-compat decode is covered by the always-on round-trip tests + the
    /// pure-Rust `cram-extract`, which decodes zstd via `ruzstd`.
    #[cfg(feature = "zstd-c")]
    #[test]
    fn zstd_pack_compress_selects_codec_and_never_grows() {
        let compressible = b"the quick brown fox ".repeat(5000);
        let (codec, comp) = pack_compress(compressible.clone(), 6, true).unwrap();
        assert_eq!(codec, CODEC_ZSTD, "zstd-c build uses the zstd pack codec");
        assert!(
            comp.len() < compressible.len(),
            "zstd shrank compressible data"
        );

        // Incompressible bytes → stored raw, byte-identical, never grown.
        let mut x = 0x1234_5678u32;
        let mut incompressible = Vec::with_capacity(20_000);
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            incompressible.push((x >> 24) as u8);
        }
        let (codec2, comp2) = pack_compress(incompressible.clone(), 6, true).unwrap();
        assert_eq!(
            codec2, CODEC_STORE,
            "incompressible pack is stored, not grown"
        );
        assert_eq!(comp2, incompressible);
    }

    fn tmp(bytes: &[u8]) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("cram-ut-{}-{}.cram", std::process::id(), n));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// A no-password provider (these craft unencrypted hostile archives).
    fn np() -> Arc<dyn PasswordProvider> {
        Arc::new(NoPassword)
    }

    /// Assemble a `.cram` file from a raw packs region + an index + trailer (the index sits right
    /// after the region; `region` are the literal bytes the packs point into).
    fn assemble(
        region: &[u8],
        packs: &[PackLoc],
        chunks: &[ChunkLoc],
        entries: &[EntryMeta],
    ) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(CRAM_MAGIC);
        f.extend_from_slice(&[VERSION, 0]);
        f.extend_from_slice(region);
        let index_offset = f.len() as u64;
        let index = serialize_index(packs, chunks, entries, VERSION);
        f.extend_from_slice(&index);
        put_u64(&mut f, index_offset);
        put_u64(&mut f, index.len() as u64);
        f.extend_from_slice(CRAM_MAGIC);
        f
    }

    #[test]
    fn crypter_seals_and_opens() {
        let key = [7u8; 32];
        let cr = Crypter::new(&key);
        let sealed = cr.seal(b"the treasure is buried here", b"aad-1").unwrap();
        // Ciphertext must not contain the plaintext, and must carry nonce + tag overhead.
        assert!(!naive_contains(&sealed, b"treasure"));
        assert_eq!(
            sealed.len(),
            NONCE_LEN + b"the treasure is buried here".len() + TAG_LEN
        );
        // Round-trips with the right key + AAD.
        assert_eq!(
            cr.open(&sealed, b"aad-1").unwrap(),
            b"the treasure is buried here"
        );
        // Wrong AAD, tampered byte, and a wrong key all fail authentication.
        assert!(matches!(
            cr.open(&sealed, b"aad-2"),
            Err(ArchiveError::WrongPassword)
        ));
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(matches!(
            cr.open(&tampered, b"aad-1"),
            Err(ArchiveError::WrongPassword)
        ));
        let other = Crypter::new(&[9u8; 32]);
        assert!(matches!(
            other.open(&sealed, b"aad-1"),
            Err(ArchiveError::WrongPassword)
        ));
    }

    /// Small helper: does `hay` contain `needle`?
    fn naive_contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    /// Build a bare reader with a chosen budget (for the cumulative-budget test); packs/chunks/entries
    /// filled per case.
    fn reader_with(budget: u64, packs: Vec<PackLoc>, chunks: Vec<ChunkLoc>) -> CramReader {
        CramReader {
            path: PathBuf::new(),
            packs,
            chunks,
            entries: vec![],
            entry_chunks: vec![],
            entry_transforms: vec![],
            crypter: None,
            pack_cache: Mutex::new(PackCache::new(PACK_CACHE_CAP)),
            budget,
            decompressed: AtomicU64::new(0),
            cursor: 0,
        }
    }

    #[test]
    fn decomp_budget_is_output_scaled_with_floor() {
        // Budget = max(floor, RE_DECODE_FACTOR × Σ entry sizes). Tiny archive → floor.
        let r = reader_with(MIN_DECOMP_BUDGET, vec![], vec![]);
        assert_eq!(r.decomp_budget(), MIN_DECOMP_BUDGET);
        // A larger output scales the budget above the floor.
        let big = 100 * 1024 * 1024u64;
        let r2 = reader_with(big.saturating_mul(RE_DECODE_FACTOR), vec![], vec![]);
        assert_eq!(r2.decomp_budget(), big * RE_DECODE_FACTOR);
    }

    #[test]
    fn cumulative_budget_trips_across_entries_not_per_entry() {
        // Three distinct STORE packs of 50 bytes each; a tiny budget of 100. Reconstructing entry 0
        // then entry 1 (each decodes ONE 50-byte pack) stays under budget individually, but the
        // CUMULATIVE counter reaches 150 by the third distinct pack and must trip, a per-entry reset
        // would let all three through. This is the amplification the fix closes.
        let region = vec![0u8; 150]; // three 50-byte packs laid end to end
        let packs: Vec<PackLoc> = (0..3)
            .map(|i| PackLoc {
                file_offset: HEADER_LEN + i as u64 * 50,
                comp_len: 50,
                raw_len: 50,
                codec: CODEC_STORE,
            })
            .collect();
        let chunks: Vec<ChunkLoc> = (0..3)
            .map(|i| ChunkLoc {
                pack_id: i,
                offset: 0,
                length: 50,
            })
            .collect();
        // Assemble a real file so `get_pack` can read the packs from disk.
        let entries = vec![
            EntryMeta {
                name: "a".into(),
                is_dir: false,
                size: 50,
                mode: 0,
                chunk_ids: vec![0],
                transform: XFORM_NONE,
            },
            EntryMeta {
                name: "b".into(),
                is_dir: false,
                size: 50,
                mode: 0,
                chunk_ids: vec![1],
                transform: XFORM_NONE,
            },
            EntryMeta {
                name: "c".into(),
                is_dir: false,
                size: 50,
                mode: 0,
                chunk_ids: vec![2],
                transform: XFORM_NONE,
            },
        ];
        let f = assemble(&region, &packs, &chunks, &entries);
        let path = tmp(&f);
        let mut reader = reader_with(100, packs, chunks);
        reader.path = path;

        // Entry 0 (chunk 0 → pack 0): 50 ≤ 100, OK.
        assert!(reader.reconstruct(&[0], &mut Vec::new()).is_ok());
        // Entry 1 (pack 1): cumulative 100 ≤ 100, still OK.
        assert!(reader.reconstruct(&[1], &mut Vec::new()).is_ok());
        // Entry 2 (pack 2): cumulative 150 > 100 → refused.
        assert!(matches!(
            reader.reconstruct(&[2], &mut Vec::new()),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn rejects_absurd_kdf_params() {
        // An untrusted archive can't force Argon2 to allocate ~4 TiB via a hostile m_cost.
        assert!(matches!(
            derive_key("pw", &[0u8; SALT_LEN], u32::MAX, 2, 1),
            Err(ArchiveError::Corrupt(_))
        ));
        assert!(matches!(
            derive_key("pw", &[0u8; SALT_LEN], 19_456, u32::MAX, 1),
            Err(ArchiveError::Corrupt(_))
        ));
        // The real defaults still derive a key fine.
        assert!(derive_key(
            "pw",
            &[0u8; SALT_LEN],
            ARGON_M_COST,
            ARGON_T_COST,
            ARGON_P_COST
        )
        .is_ok());
    }

    #[test]
    fn index_serialize_round_trips() {
        let packs = vec![PackLoc {
            file_offset: 8,
            comp_len: 123,
            raw_len: 456,
            codec: CODEC_XZ,
        }];
        let chunks = vec![ChunkLoc {
            pack_id: 0,
            offset: 5,
            length: 7,
        }];
        let entries = vec![EntryMeta {
            name: "dir/файл.bin".into(),
            is_dir: false,
            size: 7,
            mode: 0o644,
            chunk_ids: vec![0, 0],
            transform: XFORM_NONE,
        }];
        let bytes = serialize_index(&packs, &chunks, &entries, VERSION);
        let (p, c, e) = deserialize_index(&bytes, VERSION).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(c[0].length, 7);
        assert_eq!(e[0].name, "dir/файл.bin");
        assert_eq!(e[0].chunk_ids, vec![0, 0]);
    }

    #[test]
    fn rejects_too_small_and_bad_magic() {
        assert!(CramReader::open(&tmp(b"tiny"), np()).is_err());
        let mut bad = vec![0u8; (HEADER_LEN + TRAILER_LEN) as usize];
        bad[..CRAM_MAGIC.len()].copy_from_slice(b"NOTCRM");
        assert!(matches!(
            CramReader::open(&tmp(&bad), np()),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    /// A small real photo-like JPEG. Generated, not sampled from anywhere, and committed so the
    /// round-trip test always runs rather than depending on an image encoder being compiled in.
    const SAMPLE_JPEG: &[u8] = include_bytes!("../../tests/data/sample.jpg");

    /// The promise the whole feature rests on: a JPEG put into a `.cram` comes back **byte-for-byte**.
    /// Anything less makes the space saving worthless, so this asserts the exact bytes, not just that
    /// extraction succeeded.
    #[test]
    fn jpeg_recompression_round_trips_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!("cram-jxform-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let jpg = dir.join("src/photo.jpg");
        std::fs::write(&jpg, SAMPLE_JPEG).unwrap();
        // A non-image alongside it, to prove mixed archives still work.
        std::fs::write(dir.join("src/notes.txt"), b"plain text".repeat(50)).unwrap();

        let archive = dir.join("out.cram");
        crate::engine::create::create(
            &archive,
            Format::cram(crate::format::Codec::None),
            &[dir.join("src")],
            CreateOptions::default(), // recompression is ON by default
            &crate::progress::NullSink,
        )
        .unwrap();

        // The transform was actually used, so the header must have been patched to v2; which is what
        // makes an older reader refuse rather than emit Lepton bytes as if they were a JPEG.
        let head = std::fs::read(&archive).unwrap();
        assert_eq!(
            head[6], VERSION_XFORM,
            "an archive containing a transform must declare v2"
        );

        let reader = CramReader::open(&archive, np()).unwrap();
        let listed = RandomAccessReader::entries(&reader);
        let idx = listed
            .iter()
            .position(|e| e.name().ends_with("photo.jpg"))
            .expect("photo is listed");
        // The listed size is the ORIGINAL file's size, not the stored stream's.
        assert_eq!(listed[idx].size, SAMPLE_JPEG.len() as u64);

        let mut got = Vec::new();
        reader.copy_entry(idx, &mut got).unwrap();
        assert_eq!(got, SAMPLE_JPEG, "the exact original JPEG must come back");

        // Random access (what the mount uses) must also see the reconstructed file.
        let mid = reader.read_range(idx, 100, 64).unwrap();
        assert_eq!(
            mid,
            &SAMPLE_JPEG[100..164],
            "ranged read matches the original"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With recompression off, the same photo is stored verbatim and the archive stays v1; so
    /// disabling the feature really does opt out of the new format, not just the saving.
    #[test]
    fn recompression_can_be_disabled_and_then_stays_v1() {
        let dir = std::env::temp_dir().join(format!("cram-jnox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/photo.jpg"), SAMPLE_JPEG).unwrap();

        let archive = dir.join("out.cram");
        crate::engine::create::create(
            &archive,
            Format::cram(crate::format::Codec::None),
            &[dir.join("src")],
            CreateOptions {
                recompress_images: false,
                ..Default::default()
            },
            &crate::progress::NullSink,
        )
        .unwrap();

        let head = std::fs::read(&archive).unwrap();
        assert_eq!(head[6], VERSION, "no transform used → still a v1 archive");

        let reader = CramReader::open(&archive, np()).unwrap();
        let idx = RandomAccessReader::entries(&reader)
            .iter()
            .position(|e| e.name().ends_with("photo.jpg"))
            .unwrap();
        let mut got = Vec::new();
        reader.copy_entry(idx, &mut got).unwrap();
        assert_eq!(got, SAMPLE_JPEG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unknown_version_and_flags() {
        // Frozen-format forward guarantee: a newer version byte or an unknown (reserved) flag bit is
        // rejected at open, not silently misparsed as v1. Build a minimal well-formed empty archive,
        // then flip the version / a reserved flag byte and confirm each is refused.
        let base = assemble(&[], &[], &[], &[]);
        assert!(
            CramReader::open(&tmp(&base), np()).is_ok(),
            "baseline opens"
        );

        // v2 (transformed entries) is now also a version this build understands, so the "unknown
        // version" case has to be one beyond it.
        let mut bad_ver = base.clone();
        bad_ver[6] = VERSION_XFORM + 1;
        assert!(matches!(
            CramReader::open(&tmp(&bad_ver), np()),
            Err(ArchiveError::Corrupt(_))
        ));

        let mut bad_flag = base.clone();
        bad_flag[7] = 0x02; // a reserved flag bit (bit 0 = encrypted is the only defined one)
        assert!(matches!(
            CramReader::open(&tmp(&bad_flag), np()),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn rejects_index_len_overflow() {
        // Wrapping `index_offset + index_len` must not defeat the range check (finding: DoS via
        // huge `vec![0u8; index_len]`).
        let mut f = Vec::new();
        f.extend_from_slice(CRAM_MAGIC);
        f.extend_from_slice(&[VERSION, 0]);
        put_u64(&mut f, HEADER_LEN); // index_offset = 8
        put_u64(&mut f, u64::MAX - 3); // index_len that would wrap when added to index_offset
        f.extend_from_slice(CRAM_MAGIC);
        assert!(matches!(
            CramReader::open(&tmp(&f), np()),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn rejects_pack_comp_len_overflow() {
        // A pack whose `file_offset + comp_len` wraps u64 must be rejected, not pass validation and
        // later drive a huge `vec![0u8; comp_len]` in read_pack.
        let packs = vec![PackLoc {
            file_offset: HEADER_LEN,
            comp_len: u64::MAX - 3,
            raw_len: 10,
            codec: CODEC_STORE,
        }];
        let f = assemble(&[], &packs, &[], &[]);
        assert!(matches!(
            CramReader::open(&tmp(&f), np()),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn rejects_pack_raw_len_mismatch() {
        // XZ payload decompresses to 500 bytes but the index claims raw_len = 1000 → read_pack must
        // reject on the length check rather than serve short/garbage bytes.
        let real = vec![7u8; 500];
        let mut w = XzWriter::new(Vec::new(), XzOptions::with_preset(1)).unwrap();
        w.write_all(&real).unwrap();
        let comp = w.finish().unwrap();

        let packs = vec![PackLoc {
            file_offset: HEADER_LEN,
            comp_len: comp.len() as u64,
            raw_len: 1000, // lie: real decompressed length is 500
            codec: CODEC_XZ,
        }];
        let chunks = vec![ChunkLoc {
            pack_id: 0,
            offset: 0,
            length: 500, // <= raw_len, passes open() bounds
        }];
        let entries = vec![EntryMeta {
            name: "x.bin".into(),
            is_dir: false,
            size: 500,
            mode: 0,
            chunk_ids: vec![0],
            transform: XFORM_NONE,
        }];
        let f = assemble(&comp, &packs, &chunks, &entries);
        let reader =
            CramReader::open(&tmp(&f), np()).expect("open validates bounds, not pack contents");
        // Reconstruction hits read_pack, whose length check rejects the mismatch.
        let mut out = Vec::new();
        assert!(matches!(
            reader.copy_entry(0, &mut out),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn rejects_pack_decompressing_longer_than_raw_len() {
        // A pack whose payload decompresses to MORE than the declared raw_len must be rejected, not
        // silently truncated to raw_len. The +1 sentinel in read_pack makes the over-run observable
        // so the exact-length check catches it (spec §9.8: a pack must decode to EXACTLY raw_len).
        let real = vec![3u8; 1000];
        let mut w = XzWriter::new(Vec::new(), XzOptions::with_preset(1)).unwrap();
        w.write_all(&real).unwrap();
        let comp = w.finish().unwrap();

        let packs = vec![PackLoc {
            file_offset: HEADER_LEN,
            comp_len: comp.len() as u64,
            raw_len: 500, // lie: the real decompressed length is 1000 (longer)
            codec: CODEC_XZ,
        }];
        let chunks = vec![ChunkLoc {
            pack_id: 0,
            offset: 0,
            length: 500, // <= raw_len, passes open() bounds
        }];
        let entries = vec![EntryMeta {
            name: "x.bin".into(),
            is_dir: false,
            size: 500,
            mode: 0,
            chunk_ids: vec![0],
            transform: XFORM_NONE,
        }];
        let f = assemble(&comp, &packs, &chunks, &entries);
        let reader = CramReader::open(&tmp(&f), np()).expect("open validates bounds, not contents");
        let mut out = Vec::new();
        assert!(matches!(
            reader.copy_entry(0, &mut out),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn rejects_entry_size_chunk_length_mismatch() {
        // Anti-amplification integrity: an index whose declared entry size disagrees with the sum of
        // its chunk lengths is rejected at open, so a hostile index can't misreport how much its
        // chunk list expands to (the metered decompression budget alone doesn't catch a chunk list
        // that repeats one already-decompressed chunk).
        let real = vec![9u8; 40];
        let packs = vec![PackLoc {
            file_offset: HEADER_LEN,
            comp_len: real.len() as u64,
            raw_len: real.len() as u32,
            codec: CODEC_STORE,
        }];
        let chunks = vec![ChunkLoc {
            pack_id: 0,
            offset: 0,
            length: 40,
        }];
        let entries = vec![EntryMeta {
            name: "x.bin".into(),
            is_dir: false,
            size: 999, // lie: the chunk list reconstructs only 40 bytes
            mode: 0,
            chunk_ids: vec![0],
            transform: XFORM_NONE,
        }];
        let f = assemble(&real, &packs, &chunks, &entries);
        assert!(matches!(
            CramReader::open(&tmp(&f), np()),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn repeated_chunk_id_is_legit_and_reconstructs() {
        // In-file dedup legitimately repeats a chunk id within one entry; size must equal N×length,
        // and the streaming copy_entry path reconstructs every copy. (Positive control for the
        // size==Σlength invariant so it can't be over-tightened to reject real dedup.)
        let real = vec![7u8; 10];
        let packs = vec![PackLoc {
            file_offset: HEADER_LEN,
            comp_len: 10,
            raw_len: 10,
            codec: CODEC_STORE,
        }];
        let chunks = vec![ChunkLoc {
            pack_id: 0,
            offset: 0,
            length: 10,
        }];
        let entries = vec![EntryMeta {
            name: "r.bin".into(),
            is_dir: false,
            size: 30, // 3 × 10
            mode: 0,
            chunk_ids: vec![0, 0, 0],
            transform: XFORM_NONE,
        }];
        let f = assemble(&real, &packs, &chunks, &entries);
        let reader = CramReader::open(&tmp(&f), np()).unwrap();
        let mut out = Vec::new();
        reader.copy_entry(0, &mut out).unwrap();
        assert_eq!(out, vec![7u8; 30]);
    }
}
