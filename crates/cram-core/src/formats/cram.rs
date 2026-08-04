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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use fastcdc::v2020::StreamCDC;
use lzma_rust2::{XzOptions, XzReader, XzWriter};
use zeroize::Zeroizing;

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::hw::{self, HwProfile};
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
    // `CRAM_ZSTD_LEVEL` reaches the --ultra range (20-22), which the preset mapping cannot: it
    // tops out at 19 because that is the last level zstd will use without an explicit opt-in to
    // the larger window and memory. Experiment knob, same footing as CRAM_PACK_TARGET.
    if let Some(n) = env_i32("CRAM_ZSTD_LEVEL") {
        return n.clamp(1, 22);
    }
    (preset as i32 * 2).clamp(1, 19)
}

/// Read a small signed integer from the environment, for the tuning knobs.
fn env_i32(key: &str) -> Option<i32> {
    std::env::var_os(key)?.to_str()?.trim().parse::<i32>().ok()
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
/// The pack size this writer will use, chosen by **effort level and nothing else**. A pack is
/// flushed once its raw contents reach this size, so it may overshoot by up to one chunk.
///
/// The pack is the compressor's whole world, so it is the archive's match window, and raising it is
/// the only lever `.cram` has against a solid-block LZMA archive that matches across hundreds of
/// megabytes. Measured on the kernel tree at `--best`, 8 -> 32 MiB takes the archive from 172.37 MB
/// to 164.61 MB, which is smaller than `7z -mx=5` produces, and drives pack decodes to exactly 1.00
/// per pack so verify and extract roughly triple in speed.
///
/// **Level, not hardware.** This value shapes the bytes on disk, and an unencrypted `.cram` is
/// guaranteed byte-for-byte identical from the same inputs (`tests/reproducible.rs`) so that it can
/// be content-addressed, checked against a published hash, and signed. Deriving it from the
/// machine's RAM would break that, and would leave a small machine with a permanently worse archive
/// no amount of later copying could improve. The machine's constraint is answered by
/// [`hw::create_batch`], which decides how many of these are in flight at once and provably does not
/// change the output.
///
/// The curve flattens here: past 32 MiB, ratio gains fall to 1.5% while create costs 18% more time
/// and 58% more memory, verify stops improving, and extract gets *worse* as too few packs balance
/// unevenly across the workers. `Best` therefore stops at 32 rather than running to the format's
/// limit.
fn pack_target_for(level: Level) -> usize {
    const MIB: usize = 1024 * 1024;
    if let Some(v) = pack_target_override() {
        return v;
    }
    match level {
        // Speed is the whole point; a bigger window buys ratio these levels did not ask for.
        Level::Fastest => 8 * MIB,
        Level::Auto | Level::Balanced | Level::Explicit(_) => 16 * MIB,
        Level::Best => 32 * MIB,
    }
}

/// `CRAM_PACK_TARGET` (in MiB), for sweeping the choice above without a rebuild.
///
/// **Clamped so a pack can never break the format.** §9 check 5 of `docs/CRAM_FORMAT.md` makes
/// `raw_len <= 64 MiB` mandatory reader validation, and `cram-extract` enforces it separately, so an
/// archive above that bound would be rejected as hostile by every conforming reader including our
/// own. The ceiling leaves a whole `CHUNK_MAX` of headroom for the one-chunk overshoot.
fn pack_target_override() -> Option<usize> {
    let mib = std::env::var_os("CRAM_PACK_TARGET")?
        .to_str()?
        .trim()
        .parse::<usize>()
        .ok()?;
    Some(
        mib.saturating_mul(1024 * 1024)
            .clamp(1024 * 1024, MAX_PACK_RAW - CHUNK_MAX as usize),
    )
}
/// Defensive ceiling on a single pack's raw size when reading an untrusted archive (guards the
/// decompression buffer against a corrupt/hostile `raw_len`). This is the format's bound, not a
/// tuning choice: see `docs/CRAM_FORMAT.md` §9 check 5.
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
/// Floor on the bytes of decompressed packs kept in the shared cross-worker cache (see
/// [`PackCache`]), and the exact figure for a default 8 MiB-pack archive: room for 32 of them.
const PACK_CACHE_CAP: usize = 256 * 1024 * 1024;

/// Size the decompressed-pack cache to the archive that was just opened.
///
/// A fixed byte cap is really a *pack* cap, and the two stop agreeing as soon as an archive is
/// written with a larger pack target: 256 MiB holds thirty-two 8 MiB packs but only eight 32 MiB
/// ones, against as many workers as the machine has threads. The read paths give one pack to one
/// worker, so a cache too small to hold one per worker evicts packs that are still in use and they
/// are decompressed again.
///
/// So keep the ratio the byte cap already implied -- room for 32 packs -- with the old constant as
/// the floor, so a default archive caches exactly what it always did. The ceiling stops a
/// pathological `raw_len` from making this the thing that exhausts memory.
/// The ceiling is the machine's rather than a constant: a quarter of available RAM, capped at 1 GiB.
/// That is the read-side half of the principle the writer follows, where the archive's shape is
/// fixed by the level that made it and only the memory spent handling it bends to the hardware. A
/// 4 GB machine reading a 32 MiB-pack archive caches less and re-decodes a little more; it still
/// reads the archive, and it reads exactly the same archive a large machine does.
fn pack_cache_cap(packs: &[PackLoc]) -> usize {
    const CEILING: usize = 1024 * 1024 * 1024;
    let largest = packs.iter().map(|p| p.raw_len as usize).max().unwrap_or(0);
    let ram_avail = HwProfile::detect().ram_avail;
    // A machine that will not report its memory keeps the old fixed ceiling rather than a guess.
    let ceiling = if ram_avail > 0 {
        ((ram_avail / 4) as usize).min(CEILING)
    } else {
        CEILING
    };
    // Room for one pack always wins: a cache too small to hold the pack being decoded would make
    // every single entry a fresh decompression.
    let floor = PACK_CACHE_CAP.min(ceiling).max(largest);
    largest.saturating_mul(32).clamp(floor, ceiling.max(floor))
}

/// Read-side pack accounting, printed when `CRAM_PROFILE` is set.
///
/// A pack is decompressed whole, so the only figure that matters on the read side is how many times
/// each one is decoded. **Once per pack is the floor**, and anything above it is CPU thrown away.
/// The shared cache hides that cost rather than reporting it: two workers that miss on the same pack
/// both decompress it and `PackCache::insert` silently discards the loser, so the wasted work shows
/// up only as unexplained CPU time.
///
/// Process-global and `Relaxed`, which is right for one CLI invocation and would need revisiting if
/// a caller ever ran two operations at once.
mod packprof {
    use std::sync::atomic::AtomicU64;
    /// Packs read off disk and decompressed, i.e. cache misses.
    pub static DECODES: AtomicU64 = AtomicU64::new(0);
    /// Requests served from the shared cache.
    pub static HITS: AtomicU64 = AtomicU64::new(0);
    /// Decompressed bytes produced by those decodes.
    pub static BYTES: AtomicU64 = AtomicU64::new(0);
}
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
    // `CRAM_XZ_PRESET` overrides the level's preset. This is the knob for the open question of
    // whether `--best` still needs preset 9 now that a pack is 32 MiB rather than 8: ratio bought
    // by a wider window is ratio the match search no longer has to earn, and nothing had re-tuned
    // the search after the window moved.
    if let Some(n) = env_i32("CRAM_XZ_PRESET") {
        return n.clamp(0, 9) as u32;
    }
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
    /// Behind an `Arc` so a batch handed to the background compressor can hold it without
    /// borrowing the writer, which stays on the chunking thread.
    crypter: Option<Arc<Crypter>>,
    /// Id the currently-filling `pack_buf` will get (chunks reference this before the pack is written).
    next_pack_id: u32,
    /// The compressors, running continuously. Sealed packs go in as they are produced and come back
    /// out strictly in id order, with no batch boundary in between (see [`PackQueue`]).
    queue: PackQueue,
    /// Raw bytes to accumulate before sealing a pack. Resolved once at construction rather than
    /// read per chunk (see [`pack_target`]).
    pack_target: usize,
    start: Instant,
    /// Phase accounting, printed at `finish` only when `CRAM_PROFILE` is set in the environment.
    /// The create path alternates between a serial chunk phase and a parallel compress phase, and
    /// the split between them is the thing worth knowing: a barrier shows up as a large
    /// `flush` share with idle cores either side of it. Cost is a handful of `Instant` reads per
    /// chunk, which is vDSO on Linux and QPC on Windows, tens of nanoseconds against chunks that
    /// average tens of kilobytes.
    prof: Prof,
}

/// Serial-vs-parallel accounting for one create. Nanoseconds, so a whole run cannot overflow.
/// One compressed pack as the compressor hands it back: `(payload, raw_len, codec)`.
type CompressedPack = (Vec<u8>, u32, u8);
/// One pack as a worker hands it back, with what it cost to produce.
struct DonePack {
    id: u32,
    payload: Vec<u8>,
    raw_len: u32,
    codec: u8,
    nanos: u128,
}

/// The pack compressor: a fixed set of worker threads pulling from one bounded queue, with no batch
/// boundary anywhere.
///
/// Packs used to compress a batch at a time under `into_par_iter().collect()`, which is a barrier --
/// the batch ends only when its slowest pack does. Packs are equal in raw size and unequal in
/// compress time (6.9 s against 18.9 s inside a single batch on the kernel tree), so a worker that
/// drew an easy pack idled until the straggler landed: 196 core-seconds of it, about a fifth of the
/// pool's time. Which batch size won was also an accident of arithmetic rather than a property of
/// the machine -- 47 packs split 24+23 beat both 20+20+7 and 32+15 on the same corpus at identical
/// output bytes, because a short final batch leaves most of the pool with nothing to do.
///
/// A queue has no such quantisation: a worker that finishes early takes the next pack immediately,
/// and the only idle left is at the very end of the job.
///
/// **The archive does not change.** Ids are assigned in `queue_pack` in chunk order, and packs leave
/// through `pop_in_order` strictly by id out of a reorder buffer, so completion order -- the one
/// thing scheduling actually varies -- never reaches the file. `tests/batch_invariance.rs` is what
/// holds this down.
struct PackQueue {
    /// Bounded, so a chunker that outruns the compressors blocks rather than buffering the whole
    /// archive into memory. This is the cap on raw packs in flight, and with it the cap on `ready`:
    /// at most `capacity + workers` packs can be outstanding, so neither can grow without limit.
    work: Option<SyncSender<(u32, Vec<u8>)>>,
    done: Receiver<Result<DonePack>>,
    workers: Vec<JoinHandle<()>>,
    /// Packs that finished ahead of their turn, waiting on the ids in front of them.
    ready: BTreeMap<u32, CompressedPack>,
    /// The next id the file expects.
    next_write: u32,
}

impl PackQueue {
    fn new(
        workers: usize,
        capacity: usize,
        level: u32,
        use_zstd: bool,
        store: bool,
        crypter: Option<Arc<Crypter>>,
    ) -> Self {
        let (work_tx, work_rx) = sync_channel::<(u32, Vec<u8>)>(capacity.max(1));
        let (done_tx, done_rx) = sync_channel::<Result<DonePack>>(capacity.max(1) + workers);
        // One receiver shared by every worker. Contention is a lock per pack against a pack that
        // takes whole seconds to compress, so it never shows up.
        let work_rx = Arc::new(Mutex::new(work_rx));

        let handles = (0..workers.max(1))
            .map(|_| {
                let rx = Arc::clone(&work_rx);
                let tx = done_tx.clone();
                let crypter = crypter.clone();
                std::thread::spawn(move || loop {
                    let job = {
                        let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
                        guard.recv()
                    };
                    let Ok((id, raw)) = job else { break };
                    let t0 = Instant::now();
                    let out = compress_pack(raw, id, level, use_zstd, store, crypter.as_deref())
                        .map(|(payload, raw_len, codec)| DonePack {
                            id,
                            payload,
                            raw_len,
                            codec,
                            nanos: t0.elapsed().as_nanos(),
                        });
                    // A closed receiver means the writer is gone (an error elsewhere); stop quietly.
                    if tx.send(out).is_err() {
                        break;
                    }
                })
            })
            .collect();

        Self {
            work: Some(work_tx),
            done: done_rx,
            workers: handles,
            ready: BTreeMap::new(),
            next_write: 0,
        }
    }

    /// Hand a raw pack over, blocking while the queue is full. Blocking here is the backpressure
    /// that bounds memory, and the time spent in it is the chunker waiting on the compressors.
    fn send(&mut self, id: u32, raw: Vec<u8>) -> Result<()> {
        let Some(tx) = self.work.as_ref() else {
            return Err(ArchiveError::Backend("pack queue already closed".into()));
        };
        // Try first so the common case never touches the clock, then fall back to a blocking send.
        match tx.try_send((id, raw)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full((id, raw))) => tx
                .send((id, raw))
                .map_err(|_| ArchiveError::Backend("pack compression workers died".into())),
            Err(TrySendError::Disconnected(_)) => Err(ArchiveError::Backend(
                "pack compression workers died".into(),
            )),
        }
    }

    /// Absorb every completion that is already waiting, without blocking.
    fn collect_available(&mut self, prof: &mut Prof) -> Result<()> {
        while let Ok(done) = self.done.try_recv() {
            self.absorb(done?, prof);
        }
        Ok(())
    }

    /// Block for one completion. `Ok(false)` means every worker has finished and hung up.
    fn collect_one(&mut self, prof: &mut Prof) -> Result<bool> {
        match self.done.recv() {
            Ok(done) => {
                self.absorb(done?, prof);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    fn absorb(&mut self, d: DonePack, prof: &mut Prof) {
        prof.pack_nanos.push(d.nanos);
        prof.last_complete = Some(Instant::now());
        self.ready.insert(d.id, (d.payload, d.raw_len, d.codec));
    }

    /// The next pack in file order, if it has arrived. Returning owned bytes keeps the borrow off
    /// the writer, which needs `&mut self` to write them.
    fn pop_in_order(&mut self) -> Option<CompressedPack> {
        let out = self.ready.remove(&self.next_write)?;
        self.next_write += 1;
        Some(out)
    }

    /// Stop accepting work. Workers drain what is queued, then exit on the closed channel.
    fn close(&mut self) {
        self.work = None;
    }

    fn join(&mut self) {
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

#[derive(Default)]
struct Prof {
    /// Inside `chunk_stream`, excluding any nested `flush_batch`.
    chunk_nanos: u128,
    /// Pulling bytes from the source and running FastCDC over them.
    read_cdc_nanos: u128,
    /// BLAKE3 over chunk bodies.
    hash_nanos: u128,
    /// Writing completed packs out in id order, on the chunking thread.
    flush_nanos: u128,
    /// The part of `flush_nanos` spent standing still: backpressure on a full work queue, plus
    /// blocking waits for a pack the file needs next. Near zero means the compressors keep up.
    drain_nanos: u128,
    /// How many times the chunker had to block rather than find the work already done.
    stalls: u64,
    chunks: u64,
    dedup_hits: u64,
    /// Serialising the index and writing it, at `finish`. Grows with entry and chunk count rather
    /// than with bytes, so it is the tail a many-small-files corpus pays.
    index_nanos: u128,
    /// Every pack's compress time. The spread across these is what made the old batch barrier
    /// expensive, and it is worth watching whether the queue actually absorbs it.
    pack_nanos: Vec<u128>,
    /// First pack handed to a worker, and the last one handed back: the compress phase's wall.
    first_dispatch: Option<Instant>,
    last_complete: Option<Instant>,
    /// How many workers were running, so idle core-time can be read off the two above.
    workers: usize,
}

/// Compress (and, when encrypting, seal) one pack's raw bytes into its on-disk payload. Pure and
/// thread-safe, packs are independent, so a whole batch compresses in parallel. Returns
/// `(payload, raw_len, codec)`; a pack that the codec doesn't shrink is stored raw so it never grows.
fn compress_pack(
    raw: Vec<u8>,
    pack_id: u32,
    level: u32,
    use_zstd: bool,
    store: bool,
    crypter: Option<&Crypter>,
) -> Result<(Vec<u8>, u32, u8)> {
    let raw_len = raw.len() as u32;
    let (codec, plaintext) = pack_compress(raw, level, use_zstd, store)?;
    // Encrypt AFTER compression (compress-then-encrypt); AAD binds the pack to its id.
    let payload = match crypter {
        Some(cr) => cr.seal(&plaintext, &pack_id.to_le_bytes())?,
        None => plaintext,
    };
    Ok((payload, raw_len, codec))
}

/// Codec choice for one pack: zstd (fast, `zstd-c` build only) or XZ (default, best ratio). Returns
/// `(codec, bytes)`; stores raw if compression didn't shrink it.
fn pack_compress(raw: Vec<u8>, level: u32, use_zstd: bool, store: bool) -> Result<(u8, Vec<u8>)> {
    // `--store` asked for no compression, so do none. This used to fall through to the codec and
    // produce an archive byte-identical to the default at full XZ cost, because `--store` sets
    // `format::Codec::None` -- the *stream wrapper* codec, which `.cram` never consults, since it
    // compresses per pack rather than wrapping a stream. Measured on the mixed corpus: `--store`
    // took 53.52 s and returned ratio 0.635, which is not storing anything.
    if store {
        return Ok((CODEC_STORE, raw));
    }
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
    //
    // Sampled ACROSS the pack, not from its head: a pack that only begins with high-entropy bytes
    // was being stored whole, which cost 14 MB on one silesia pack. See `probe::spread_verdict`.
    if crate::probe::spread_verdict(&raw).is_store() {
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
                (Some(Arc::new(crypter)), HEADER_LEN + CRYPTO_BLOCK_LEN)
            }
        };

        // Two separate questions, deliberately answered by two separate inputs: how big a pack is
        // (the archive's match window, decided by the level the user asked for) and how many are in
        // flight (this machine's memory, invisible in the output).
        let pack_target = pack_target_for(opts.level);
        // One worker per slot the machine can afford, and the same number again queued behind them,
        // so the outstanding-pack ceiling matches what the old two-batch scheme held.
        let slots = hw::create_batch(pack_target, &HwProfile::detect());
        // In a `zstd-c` build, use the fast C zstd for packs by default, but honor `--best` by
        // falling back to XZ's stronger ratio. A pure-Rust build always uses XZ (flag stays false).
        // `CRAM_FORCE_ZSTD=1` overrides that, to test whether zstd at --ultra over a 32 MiB pack
        // lands near XZ's size for a fraction of the encode time -- a different point on the
        // frontier rather than a cheaper route to the same one.
        let use_zstd = cfg!(feature = "zstd-c")
            && (!matches!(opts.level, Level::Best) || env_i32("CRAM_FORCE_ZSTD") == Some(1));
        // `--store` reaches here as `format::Codec::None`. For a container that wraps a stream that
        // means "no wrapper"; for `.cram`, which compresses per pack, nothing consulted it at all
        // and the flag did nothing. It now means what it says.
        let store_packs = matches!(opts.codec, Some(crate::format::Codec::None));
        let queue = PackQueue::new(
            slots,
            slots,
            preset(opts.level),
            use_zstd,
            store_packs,
            crypter.clone(),
        );
        let prof = Prof {
            workers: slots,
            ..Prof::default()
        };

        Ok(Self {
            out,
            pos,
            seen: HashMap::new(),
            packs: Vec::new(),
            chunks: Vec::new(),
            entries: Vec::new(),
            pack_buf: Vec::new(),
            in_bytes: 0,
            dedup_saved: 0,
            // Storing and transforming are contradictory instructions: a Lepton pass is the most
            // expensive thing this writer does, and `--store` is a request not to spend that.
            recompress_images: opts.recompress_images && !store_packs,
            used_transform: false,
            crypter,
            next_pack_id: 0,
            queue,
            pack_target,
            start: Instant::now(),
            prof,
        })
    }

    /// Chunk everything `src` yields into the dedup table and the current pack, returning the chunk
    /// ids. Shared by the plain path and the recompressed path so both dedup identically, two
    /// copies of one photo still collapse to a single stored copy.
    fn chunk_stream(&mut self, src: &mut dyn Read) -> Result<(Vec<u32>, u64)> {
        let mut chunk_ids = Vec::new();
        let mut size = 0u64;
        let mut chunker = StreamCDC::new(src, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX);
        // Timed as an explicit loop rather than `for chunk in chunker` so the iterator's own work
        // -- reading from `src` and running the content-defined boundary search -- is separable from
        // hashing and from the flush barrier. Those three are the candidates for the serial ceiling
        // and guessing between them is what this exists to avoid.
        let stream_t0 = Instant::now();
        let flush_at_entry = self.prof.flush_nanos;
        let drain_at_entry = self.prof.drain_nanos;
        loop {
            let cdc_t0 = Instant::now();
            let next = chunker.next();
            self.prof.read_cdc_nanos += cdc_t0.elapsed().as_nanos();
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| ArchiveError::Backend(format!("chunker: {e}")))?;
            let data = chunk.data;
            size += data.len() as u64;
            self.in_bytes += data.len() as u64;
            self.prof.chunks += 1;
            let hash_t0 = Instant::now();
            let key = *blake3::hash(&data).as_bytes();
            self.prof.hash_nanos += hash_t0.elapsed().as_nanos();
            if let Some(&id) = self.seen.get(&key) {
                self.dedup_saved += data.len() as u64;
                self.prof.dedup_hits += 1;
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
            if self.pack_buf.len() >= self.pack_target {
                self.queue_pack()?;
            }
        }
        // Serial time for this stream: everything above, minus the pack handling nested inside it.
        // Both parts have to come out -- writing packs out, and blocking on a full queue -- or
        // `chunk_nanos` absorbs the wait and reads as though FastCDC had got slower, which is
        // exactly how a 3.1 s chunk phase first appeared as 26.7 s.
        let nested = self.prof.flush_nanos.saturating_sub(flush_at_entry)
            + self.prof.drain_nanos.saturating_sub(drain_at_entry);
        self.prof.chunk_nanos += stream_t0.elapsed().as_nanos().saturating_sub(nested);
        Ok((chunk_ids, size))
    }

    fn write_out(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Move the filled `pack_buf` into the pending batch under its id.
    fn queue_pack(&mut self) -> Result<()> {
        if self.pack_buf.is_empty() {
            return Ok(());
        }
        let id = self.next_pack_id;
        self.next_pack_id += 1;
        if self.prof.first_dispatch.is_none() {
            self.prof.first_dispatch = Some(Instant::now());
        }
        // A full queue blocks here, and that block is the chunker waiting on the compressors, so it
        // is charged as such instead of disappearing into the chunk phase.
        let send_t0 = Instant::now();
        self.queue.send(id, std::mem::take(&mut self.pack_buf))?;
        self.prof.drain_nanos += send_t0.elapsed().as_nanos();
        self.pump()
    }

    /// Take back whatever the compressors have finished and write everything the file can now write.
    ///
    /// Called after every pack is queued, and it never blocks: what has not arrived is left for the
    /// next call, so chunking continues while compression runs behind it. Peak raw memory is bounded
    /// by the queue rather than by a batch -- at most `capacity + workers` packs are outstanding, and
    /// `hw::create_batch` sizes both so the product fits the machine.
    fn pump(&mut self) -> Result<()> {
        let t0 = Instant::now();
        self.queue.collect_available(&mut self.prof)?;
        self.write_ready()?;
        self.prof.flush_nanos += t0.elapsed().as_nanos();
        Ok(())
    }

    /// Write every pack whose turn has come, in id order, and stop at the first gap.
    ///
    /// The gap is the whole point: a pack that finished early waits in the reorder buffer until the
    /// ids in front of it land, so what reaches the file is a function of the input alone and never
    /// of which worker happened to finish first.
    fn write_ready(&mut self) -> Result<()> {
        while let Some((payload, raw_len, codec)) = self.queue.pop_in_order() {
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

    /// Close the queue and block until every pack has come back and been written.
    ///
    /// Only `finish` needs this: the index records each pack's file offset, so nothing can be
    /// serialized until the last one is on disk. Time spent here is the chunker with no work left,
    /// waiting on compressors that are still going, which is the tail of the job and nothing else.
    fn drain_all(&mut self) -> Result<()> {
        self.queue.close();
        while self.queue.next_write < self.next_pack_id {
            let drain_t0 = Instant::now();
            let alive = self.queue.collect_one(&mut self.prof)?;
            self.prof.drain_nanos += drain_t0.elapsed().as_nanos();
            self.prof.stalls += 1;
            self.write_ready()?;
            // Every worker hung up with packs still missing: one of them died without reporting.
            // Failing here rather than serializing an index is deliberate -- the trailer would
            // otherwise point at pack offsets for packs that were never written.
            if !alive && self.queue.next_write < self.next_pack_id {
                return Err(ArchiveError::Backend(
                    "pack compression ended before every pack was returned".into(),
                ));
            }
        }
        self.queue.join();
        Ok(())
    }

    /// Print the serial/parallel split to stderr when `CRAM_PROFILE` is set. Diagnostic only: no
    /// caller parses this and nothing about the archive depends on it.
    fn report_profile(&self) {
        if std::env::var_os("CRAM_PROFILE").is_none() {
            return;
        }
        let p = &self.prof;
        let wall = self.start.elapsed().as_nanos().max(1);
        let ms = |n: u128| n as f64 / 1e6;
        let pct = |n: u128| (n as f64 / wall as f64) * 100.0;
        eprintln!("--- cram create profile ---");
        eprintln!("wall            {:9.1} ms", ms(wall));
        eprintln!(
            "chunk (serial)  {:9.1} ms  {:5.1}%   read+cdc {:.1} ms, hash {:.1} ms, other {:.1} ms",
            ms(p.chunk_nanos),
            pct(p.chunk_nanos),
            ms(p.read_cdc_nanos),
            ms(p.hash_nanos),
            ms(p.chunk_nanos
                .saturating_sub(p.read_cdc_nanos)
                .saturating_sub(p.hash_nanos)),
        );
        eprintln!(
            "write packs     {:9.1} ms  {:5.1}%   {} workers, of which BLOCKED {:.1} ms ({:.1}%) over {} stalls",
            ms(p.flush_nanos),
            pct(p.flush_nanos),
            p.workers,
            ms(p.drain_nanos),
            pct(p.drain_nanos),
            p.stalls,
        );
        // Costs the writer cannot see: the source files are opened by `engine::create`, and the tree
        // walk and the store-vs-compress probe both run to completion before this writer exists, so
        // those two sit outside `wall` entirely rather than inside the residual.
        use crate::engine::prof as eprof;
        use std::sync::atomic::Ordering::Relaxed;
        let open = eprof::OPEN_NANOS.load(Relaxed) as u128;
        let opens = eprof::OPEN_COUNT.load(Relaxed);
        let walk = eprof::WALK_NANOS.load(Relaxed) as u128;
        let probe = eprof::PROBE_NANOS.load(Relaxed) as u128;
        eprintln!(
            "open (serial)   {:9.1} ms  {:5.1}%   {} files, {:.1} us each",
            ms(open),
            pct(open),
            opens,
            if opens > 0 {
                open as f64 / opens as f64 / 1e3
            } else {
                0.0
            },
        );
        eprintln!(
            "index+trailer   {:9.1} ms  {:5.1}%",
            ms(p.index_nanos),
            pct(p.index_nanos)
        );
        let residual = wall
            .saturating_sub(p.chunk_nanos)
            .saturating_sub(p.flush_nanos)
            .saturating_sub(open)
            .saturating_sub(p.index_nanos);
        eprintln!(
            "residual        {:9.1} ms  {:5.1}%",
            ms(residual),
            pct(residual)
        );
        eprintln!(
            "before the writer existed: walk {:.1} ms, probe {:.1} ms  (outside `wall`)",
            ms(walk),
            ms(probe),
        );
        eprintln!(
            "chunks {}  dedup hits {}  packs {}  rayon threads {}",
            p.chunks,
            p.dedup_hits,
            self.packs.len(),
            rayon::current_num_threads(),
        );
        self.report_queue();
    }

    /// How well the pack workers were kept fed.
    ///
    /// The batch scheme this replaced ended each batch at a barrier, and per-pack times spanning
    /// 6.9 s to 18.9 s inside one batch made that expensive: 196 core-seconds of workers sitting on
    /// a finished pack waiting for a straggler, on the kernel tree alone. A queue can only leave
    /// idle at the tail of the whole job, so occupancy is the number worth watching -- busy
    /// core-time against what the pool had available between first dispatch and last completion.
    fn report_queue(&self) {
        let p = &self.prof;
        let (Some(t0), Some(t1)) = (p.first_dispatch, p.last_complete) else {
            return;
        };
        if p.pack_nanos.is_empty() {
            return;
        }
        let ms = |n: u128| n as f64 / 1e6;
        let wall = t1.duration_since(t0).as_nanos().max(1);
        let busy: u128 = p.pack_nanos.iter().sum();
        let workers = p.workers.max(1) as u128;
        let capacity = wall * workers;
        let mut sorted = p.pack_nanos.clone();
        sorted.sort_unstable();
        let (fastest, slowest) = (sorted[0], sorted[sorted.len() - 1]);
        let median = sorted[sorted.len() / 2];
        eprintln!("--- pack queue ---");
        eprintln!(
            "{} packs over {} workers: slowest {:.1} ms, median {:.1} ms, fastest {:.1} ms",
            p.pack_nanos.len(),
            p.workers,
            ms(slowest),
            ms(median),
            ms(fastest),
        );
        eprintln!(
            "compress wall {:.1} ms, busy {:.1} ms of {:.1} ms available -- occupancy {:.1}%, idle {:.1} ms",
            ms(wall),
            ms(busy),
            ms(capacity),
            (busy as f64 / capacity as f64) * 100.0,
            ms(capacity.saturating_sub(busy)),
        );
        // The floor nothing can go under: the work spread perfectly across the pool, or the single
        // longest pack, whichever is larger. When the longest pack is the larger of the two, more
        // workers cannot help and only a smaller pack can.
        eprintln!(
            "floor {:.1} ms balanced / {:.1} ms longest pack, whichever is larger",
            ms(busy / workers),
            ms(slowest),
        );
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
                    let (chunk_ids, _) = self.chunk_stream(&mut Cursor::new(&encoded))?;
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
            // `chunk_stream` accumulates the bytes it actually consumed. Summing the ids' recorded
            // lengths gave the same answer, since a deduplicated id still contributes its length
            // once per appearance, but it walked the chunk table for a figure already in hand.
            let (chunk_ids, size) = self.chunk_stream(&mut rest)?;
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
        //
        // This used to be a second copy of `chunk_stream`'s loop, byte-for-byte identical apart from
        // accumulating `size`, while `chunk_stream`'s own doc comment claimed both paths shared it.
        // They did not, so a change to one would silently not reach the other -- and this is the copy
        // every ordinary file takes.
        let (chunk_ids, size) = self.chunk_stream(body)?;
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
        // Queue the final partial pack, hand it off, then wait for it: the index records every
        // pack's file offset, so nothing can be serialized until the last one has been written.
        self.queue_pack()?;
        self.drain_all()?;
        let index_t0 = Instant::now();
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

        self.prof.index_nanos = index_t0.elapsed().as_nanos();
        self.report_profile();

        // `pos` counted every byte written (header + packs + index + trailer) = the final size.
        Ok(CreateReport {
            entries: self.entries.len() as u64,
            in_bytes: self.in_bytes,
            out_bytes: self.pos,
            stored: 0,
            dedup_saved: self.dedup_saved,
            elapsed: self.start.elapsed(),
            // Filled in by the engine walk, which is the only thing that sees the source tree.
            skipped_links: Vec::new(),
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
    /// One lock per pack, held across that pack's decode so it is decompressed **once** even when
    /// several workers reach for it at the same instant. The shared cache alone does not give this:
    /// each racing worker misses, each decompresses, and `PackCache::insert` throws all but one
    /// result away *after* the CPU has been spent. Measured at 2.31 decodes per pack on a 186-pack
    /// archive, so more than half the decompression was wasted.
    pack_locks: Vec<Mutex<()>>,
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
                // sizes differ and the equality above cannot apply. The declared size is
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
            pack_locks: (0..packs.len()).map(|_| Mutex::new(())).collect(),
            pack_cache: Mutex::new(PackCache::new(pack_cache_cap(&packs))),
            packs,
            chunks,
            entries,
            entry_chunks,
            entry_transforms,
            crypter,
            budget,
            decompressed: AtomicU64::new(0),
            cursor: 0,
        })
    }

    /// Fetch a decompressed pack, sharing it across workers. Returns `(bytes, decompressed_now)`;
    /// `decompressed_now` is true only on a cache miss (so callers charge the anti-bomb budget once).
    /// The pack is decompressed WITHOUT holding the cache lock, so workers decompress in parallel.
    fn get_pack(&self, file: &mut File, pack_id: u32) -> Result<(Arc<Vec<u8>>, bool)> {
        use std::sync::atomic::Ordering::Relaxed;
        if let Some(hit) = self.pack_cache.lock().unwrap().get(pack_id) {
            packprof::HITS.fetch_add(1, Relaxed);
            return Ok((hit, false));
        }
        // Single-flight: hold this pack's own lock across the decode, so a second worker that wants
        // it waits for these bytes instead of decompressing its own copy for nothing. Locks are per
        // pack, so unrelated packs never serialise, and a worker holds at most one at a time (the
        // decode reads the file and touches no other pack), so they cannot deadlock.
        let _flight = self.pack_locks.get(pack_id as usize).map(|m| {
            // The lock guards exclusion and nothing else, so a decoder panic leaves no invariant
            // broken and every later worker can carry on through the poison.
            m.lock().unwrap_or_else(|e| e.into_inner())
        });
        // Whoever held it ahead of us has published by now.
        if let Some(hit) = self.pack_cache.lock().unwrap().get(pack_id) {
            packprof::HITS.fetch_add(1, Relaxed);
            return Ok((hit, false));
        }
        let raw = Arc::new(self.read_pack(file, pack_id)?);
        packprof::DECODES.fetch_add(1, Relaxed);
        packprof::BYTES.fetch_add(raw.len() as u64, Relaxed);
        self.pack_cache.lock().unwrap().insert(pack_id, raw.clone());
        Ok((raw, true))
    }

    /// Print read-side pack accounting to stderr when `CRAM_PROFILE` is set. Diagnostic only:
    /// nothing parses this and nothing about the archive depends on it.
    ///
    /// `decodes / packs` is the number to read. 1.0 means every pack was decompressed exactly once,
    /// which is the floor; above that is redundant work, either a worker racing another onto the
    /// same pack or a pack evicted and fetched again.
    fn report_pack_profile(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if std::env::var_os("CRAM_PROFILE").is_none() {
            return;
        }
        let (d, h, b) = (
            packprof::DECODES.load(Relaxed),
            packprof::HITS.load(Relaxed),
            packprof::BYTES.load(Relaxed),
        );
        if d == 0 && h == 0 {
            return;
        }
        let packs = self.packs.len().max(1) as f64;
        eprintln!("--- cram pack profile ---");
        eprintln!("packs in archive  {:9}", self.packs.len());
        eprintln!(
            "pack decodes      {d:9}   {:.2} per pack   ({:.0} MiB decompressed)",
            d as f64 / packs,
            b as f64 / (1024.0 * 1024.0),
        );
        eprintln!(
            "cache hits        {h:9}   {:.1}% of {} requests",
            (h as f64 / (h + d).max(1) as f64) * 100.0,
            h + d,
        );
        // How each pack was actually encoded. A pack the writer judged incompressible is kept raw,
        // and pack sizing changes that judgement: on silesia a 16 MiB target produced an archive 26%
        // larger than either 8 or 32 MiB, which a shift toward STORE would explain and nothing about
        // window size would.
        let (mut store, mut xz, mut zstd) = (0usize, 0usize, 0usize);
        let mut store_raw = 0u64;
        for p in &self.packs {
            match p.codec {
                CODEC_STORE => {
                    store += 1;
                    store_raw += p.raw_len as u64;
                }
                CODEC_XZ => xz += 1,
                _ => zstd += 1,
            }
        }
        eprintln!(
            "pack codecs       store {store}, xz {xz}, zstd {zstd}   ({:.0} MiB stored raw)",
            store_raw as f64 / (1024.0 * 1024.0),
        );
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

/// Every operation builds its own reader and drops it at the end, so this is the one place that sees
/// a whole run's pack accounting regardless of which verb ran.
impl Drop for CramReader {
    fn drop(&mut self) {
        self.report_pack_profile();
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

    /// The entry's FIRST pack. An entry large enough to span several packs still reports one key,
    /// which is fine: the point is that the many small entries sharing a pack are visited together,
    /// and a multi-pack entry decodes its packs consecutively on one worker regardless.
    ///
    /// An entry with no chunks -- an empty file -- has no pack and returns `None`, so it is not
    /// clustered with pack 0 for no reason.
    fn locality_key(&self, index: usize) -> Option<u64> {
        let first = self.entry_chunks.get(index)?.first()?;
        let chunk = self.chunks.get(*first as usize)?;
        Some(chunk.pack_id as u64)
    }

    /// One pack is one independently decodable unit, so the pack count is exactly how wide the
    /// engine can fan out. The entry count is the wrong number here and so is `1`: a 94,753-entry
    /// archive holds around 200 packs, and nothing is gained by asking for more workers than there
    /// are packs to decode.
    fn decode_units(&self) -> Option<usize> {
        Some(self.packs.len())
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
        let (codec, comp) = pack_compress(compressible.clone(), 6, true, false).unwrap();
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
        let (codec2, comp2) = pack_compress(incompressible.clone(), 6, true, false).unwrap();
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
            pack_locks: (0..packs.len()).map(|_| Mutex::new(())).collect(),
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
