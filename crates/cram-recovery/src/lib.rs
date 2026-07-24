//! Reed-Solomon **recovery sidecar** for an arbitrary file (built for `.cram`, but format-agnostic).
//!
//! The original file is split into `N` equal **data shards** (the last zero-padded); Reed-Solomon
//! over GF(2^8) computes `M` **parity shards**. The sidecar `<file>.cramrec` stores only the parity
//! shards, a BLAKE3 hash of every shard, and a little metadata — the data shards stay in the original
//! file, so the sidecar costs only about `M/N` of the file size. If the original later suffers bit-rot
//! or truncation, `repair` recomputes each data shard's hash to find the damaged/missing ones, then
//! RS-reconstructs them from the surviving data and parity shards (recovering up to `M` lost shards).
//!
//! This is a **sidecar**: it is computed over the file's bytes and never changes the `.cram` format
//! (which is frozen at v1), so it composes with any archive — or any file at all.
//!
//! Sidecar byte layout (all integers little-endian):
//! ```text
//!   magic(8) = "CRAMREC\x01" | version(1)
//!   data_shards(u32=N) | parity_shards(u32=M) | shard_size(u64) | orig_len(u64)
//!   orig_hash(32)                       BLAKE3 of the whole original file (repair success check)
//!   shard_hashes: (N+M) × blake3(32)    data shards 0..N, then parity shards N..N+M
//!   parity: M × shard_size bytes        the parity shard payloads
//! ```

use reed_solomon_erasure::galois_8::ReedSolomon;

pub mod cli;

/// GF(2^8) Reed-Solomon caps total shards at 255; keep headroom.
const MAX_TOTAL_SHARDS: usize = 255;
/// Target data-shard size — N scales so shards land near this, keeping the sidecar and per-shard
/// hashing overhead reasonable across tiny and large files.
const TARGET_SHARD: u64 = 256 * 1024;
/// Never more than this many data shards (bounds parity room + shard count).
const MAX_DATA_SHARDS: usize = 200;
/// Largest single shard the reader will accept from an untrusted sidecar (1 GiB). Bounds per-shard
/// allocations; `create` refuses anything that would exceed it.
const MAX_SHARD_SIZE: u64 = 1 << 30;

pub const MAGIC: &[u8; 8] = b"CRAMREC\x01";
pub const VERSION: u8 = 1;
const HASH_LEN: usize = 32;

/// A recovery error. Kept as strings — this is a small standalone tool.
pub type RecResult<T> = Result<T, String>;

fn hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// Choose the shard geometry for a file of `len` bytes at the given redundancy fraction (0.0–1.0).
/// Returns `(data_shards, parity_shards, shard_size)`. Always `data+parity ≤ 255` and `parity ≥ 1`.
pub fn geometry(len: u64, redundancy: f64) -> (usize, usize, u64) {
    let redundancy = redundancy.clamp(0.01, 1.0);
    // Number of data shards: aim for ~TARGET_SHARD each (ceil-divide), at least 1, capped.
    let n = len.div_ceil(TARGET_SHARD).clamp(1, MAX_DATA_SHARDS as u64) as usize;
    // Bytes per shard: ceil(len / n), at least 1 (a zero-length file still needs one 1-byte shard).
    let shard_size = len.div_ceil(n as u64).max(1);
    // Parity shards: a fraction of the data shards, ≥ 1, bounded so n+m ≤ 255.
    let m = ((n as f64 * redundancy).round() as usize).clamp(1, MAX_TOTAL_SHARDS - n);
    (n, m, shard_size)
}

/// Split `data` into `n` shards of `shard_size` bytes (last zero-padded), plus `m` zeroed parity
/// shards ready for `encode`.
fn build_shards(data: &[u8], n: usize, m: usize, shard_size: usize) -> Vec<Vec<u8>> {
    let mut shards = Vec::with_capacity(n + m);
    for i in 0..n {
        let start = i * shard_size;
        let end = (start + shard_size).min(data.len());
        let mut shard = vec![0u8; shard_size];
        if start < data.len() {
            shard[..end - start].copy_from_slice(&data[start..end]);
        }
        shards.push(shard);
    }
    for _ in 0..m {
        shards.push(vec![0u8; shard_size]);
    }
    shards
}

fn put_u32(b: &mut Vec<u8>, x: u32) {
    b.extend_from_slice(&x.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, x: u64) {
    b.extend_from_slice(&x.to_le_bytes());
}

/// Build a recovery sidecar for `data` at the given `redundancy` (e.g. 0.1 = 10% parity).
pub fn create_sidecar(data: &[u8], redundancy: f64) -> RecResult<Vec<u8>> {
    let (n, m, shard_size) = geometry(data.len() as u64, redundancy);
    // Keep create and parse symmetric: a shard the reader would reject must never be written. With
    // MAX_DATA_SHARDS data shards this only trips on absurdly large (≳200 GiB) inputs.
    if shard_size > MAX_SHARD_SIZE {
        return Err("file too large for a recovery sidecar".into());
    }
    let ss = shard_size as usize;
    let mut shards = build_shards(data, n, m, ss);

    let rs = ReedSolomon::new(n, m).map_err(|e| format!("reed-solomon init: {e}"))?;
    rs.encode(&mut shards).map_err(|e| format!("encode: {e}"))?;

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    put_u32(&mut out, n as u32);
    put_u32(&mut out, m as u32);
    put_u64(&mut out, shard_size);
    put_u64(&mut out, data.len() as u64);
    out.extend_from_slice(&hash(data)); // whole-file hash
    for shard in &shards {
        out.extend_from_slice(&hash(shard)); // per-shard hashes (data then parity)
    }
    for shard in &shards[n..] {
        out.extend_from_slice(shard); // parity payloads only
    }
    Ok(out)
}

struct Sidecar {
    n: usize,
    m: usize,
    shard_size: usize,
    orig_len: usize,
    orig_hash: [u8; 32],
    shard_hashes: Vec<[u8; 32]>, // len n+m
    parity: Vec<Vec<u8>>,        // len m, each shard_size
}

fn read_u32(b: &[u8], p: &mut usize) -> RecResult<u32> {
    let s = b.get(*p..*p + 4).ok_or("sidecar truncated (u32)")?;
    *p += 4;
    Ok(u32::from_le_bytes(s.try_into().unwrap()))
}
fn read_u64(b: &[u8], p: &mut usize) -> RecResult<u64> {
    let s = b.get(*p..*p + 8).ok_or("sidecar truncated (u64)")?;
    *p += 8;
    Ok(u64::from_le_bytes(s.try_into().unwrap()))
}
fn read_hash(b: &[u8], p: &mut usize) -> RecResult<[u8; 32]> {
    let s = b.get(*p..*p + HASH_LEN).ok_or("sidecar truncated (hash)")?;
    *p += HASH_LEN;
    Ok(s.try_into().unwrap())
}

fn parse_sidecar(bytes: &[u8]) -> RecResult<Sidecar> {
    if bytes.len() < 9 || &bytes[..8] != MAGIC {
        return Err("not a cram recovery sidecar (bad magic)".into());
    }
    if bytes[8] != VERSION {
        return Err(format!("unsupported sidecar version {}", bytes[8]));
    }
    let mut p = 9;
    let n = read_u32(bytes, &mut p)? as usize;
    let m = read_u32(bytes, &mut p)? as usize;
    let shard_size = read_u64(bytes, &mut p)? as usize;
    let orig_len = read_u64(bytes, &mut p)? as usize;
    // Reject absurd geometry from an untrusted sidecar before allocating anything from it.
    if n == 0
        || m == 0
        || n + m > MAX_TOTAL_SHARDS
        || shard_size == 0
        || shard_size as u64 > MAX_SHARD_SIZE
    {
        return Err("sidecar has invalid geometry".into());
    }
    // Geometry must be internally consistent: `n` is exactly ⌈orig_len / shard_size⌉, so the file is
    // `(n-1)*shard_size < orig_len ≤ n*shard_size`. This ties the reconstructed buffer size
    // (`n*shard_size`, allocated in `repair`) to `orig_len`, so a hostile sidecar can't claim a tiny
    // parity payload yet force a multi-hundred-GiB allocation. (The empty file — n=1, orig_len=0 — is
    // the one legitimate exception.)
    let total = (n as u64)
        .checked_mul(shard_size as u64)
        .ok_or("sidecar geometry overflow")?;
    let ol = orig_len as u64;
    let min_len = total - shard_size as u64; // (n-1) * shard_size
    if ol > total || (ol <= min_len && !(ol == 0 && n == 1)) {
        return Err("sidecar geometry inconsistent with original length".into());
    }
    let orig_hash = read_hash(bytes, &mut p)?;
    let mut shard_hashes = Vec::with_capacity(n + m);
    for _ in 0..n + m {
        shard_hashes.push(read_hash(bytes, &mut p)?);
    }
    let mut parity = Vec::with_capacity(m);
    for _ in 0..m {
        let s = bytes
            .get(p..p + shard_size)
            .ok_or("sidecar truncated (parity)")?;
        parity.push(s.to_vec());
        p += shard_size;
    }
    Ok(Sidecar {
        n,
        m,
        shard_size,
        orig_len,
        orig_hash,
        shard_hashes,
        parity,
    })
}

/// The outcome of a repair attempt.
pub struct Repair {
    /// The reconstructed original file bytes.
    pub data: Vec<u8>,
    /// How many data shards had to be reconstructed (0 = the file was already intact).
    pub repaired_shards: usize,
}

/// Attempt to repair `corrupt` using `sidecar`. Returns the reconstructed bytes (verified against the
/// stored whole-file hash) or an error if too many shards are damaged to recover.
pub fn repair(corrupt: &[u8], sidecar: &[u8]) -> RecResult<Repair> {
    let sc = parse_sidecar(sidecar)?;
    let ss = sc.shard_size;

    // Rebuild each DATA shard from the (possibly damaged/truncated) file and check it against its
    // stored hash; a mismatch (bit-rot) or a missing tail (truncation) marks the shard for recovery.
    let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(sc.n + sc.m);
    let mut damaged = 0usize;
    for i in 0..sc.n {
        let start = i * ss;
        let mut shard = vec![0u8; ss];
        let mut present = false;
        if start < corrupt.len() {
            let end = (start + ss).min(corrupt.len());
            shard[..end - start].copy_from_slice(&corrupt[start..end]);
            present = true;
        }
        if present && hash(&shard) == sc.shard_hashes[i] {
            shards.push(Some(shard));
        } else {
            shards.push(None);
            damaged += 1;
        }
    }
    // PARITY shards come from the sidecar; still hash-check them (the sidecar itself can rot).
    for j in 0..sc.m {
        let par = &sc.parity[j];
        if hash(par) == sc.shard_hashes[sc.n + j] {
            shards.push(Some(par.clone()));
        } else {
            shards.push(None);
        }
    }

    let intact = shards.iter().filter(|s| s.is_some()).count();
    if intact < sc.n {
        return Err(format!(
            "unrecoverable: {intact} intact shards but {} are needed ({} data shards damaged, only {} parity available)",
            sc.n, damaged, sc.m
        ));
    }

    let rs = ReedSolomon::new(sc.n, sc.m).map_err(|e| format!("reed-solomon init: {e}"))?;
    rs.reconstruct_data(&mut shards)
        .map_err(|e| format!("reconstruct: {e}"))?;

    // Reassemble the file from the (now complete) data shards and truncate to the original length.
    let mut data = Vec::with_capacity(sc.n * ss);
    for shard in shards.iter().take(sc.n) {
        data.extend_from_slice(
            shard
                .as_ref()
                .ok_or("internal: data shard missing after reconstruct")?,
        );
    }
    data.truncate(sc.orig_len);

    if hash(&data) != sc.orig_hash {
        return Err("repair failed: reconstructed file does not match the recorded hash".into());
    }
    Ok(Repair {
        data,
        repaired_shards: damaged,
    })
}

/// Verify `data` against its `sidecar`: returns `Ok(true)` if the file is fully intact, `Ok(false)` if
/// it is damaged but recoverable, or `Err` if unrecoverable / the sidecar is invalid.
pub fn verify(data: &[u8], sidecar: &[u8]) -> RecResult<bool> {
    let sc = parse_sidecar(sidecar)?;
    if data.len() == sc.orig_len && hash(data) == sc.orig_hash {
        return Ok(true);
    }
    // Not byte-identical → see whether repair *would* succeed.
    repair(data, sidecar).map(|_| false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let mut x = 0x1234_5678u32;
        for _ in 0..len {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push((x >> 24) as u8);
        }
        v
    }

    #[test]
    fn round_trip_no_damage() {
        let data = sample(1_000_000);
        let side = create_sidecar(&data, 0.1).unwrap();
        assert!(verify(&data, &side).unwrap(), "intact file verifies");
        let r = repair(&data, &side).unwrap();
        assert_eq!(r.data, data);
        assert_eq!(r.repaired_shards, 0);
    }

    #[test]
    fn repairs_bit_rot_within_parity_budget() {
        let data = sample(3_000_000); // large enough that n is big → several parity shards
        let side = create_sidecar(&data, 0.25).unwrap(); // ~25% parity
        let (n, m, ss) = geometry(data.len() as u64, 0.25);
        assert!(m >= 2, "test needs at least 2 parity shards (n={n}, m={m})");

        // Corrupt bytes spread across the first `m` shards (each hit falls in a distinct shard).
        let mut corrupt = data.clone();
        for k in 0..m {
            let pos = k * (ss as usize) + 3;
            corrupt[pos] ^= 0xFF;
        }
        assert_ne!(corrupt, data);

        let r = repair(&corrupt, &side).expect("repairable within parity budget");
        assert_eq!(r.data, data, "repaired bytes match the original");
        assert_eq!(
            r.repaired_shards, m,
            "exactly the corrupted shards were rebuilt"
        );
    }

    #[test]
    fn repairs_truncation() {
        let data = sample(3_000_000);
        let side = create_sidecar(&data, 0.25).unwrap();
        let (_n, m, ss) = geometry(data.len() as u64, 0.25);
        // Truncate away the last (m-1) shards' worth of bytes (recoverable: ≤ m shards lost).
        let cut = data.len() - (m.saturating_sub(1)) * ss as usize - 10;
        let corrupt = data[..cut.max(1)].to_vec();
        let r = repair(&corrupt, &side).expect("truncation within parity budget is recoverable");
        assert_eq!(r.data, data);
    }

    #[test]
    fn rejects_geometry_bomb_sidecar() {
        // Take a well-formed sidecar (n>1) and shrink its declared orig_len to 1. The buffer `repair`
        // reconstructs is n*shard_size, which is now wildly larger than the claimed original — exactly
        // the "tiny declared size, huge allocation" shape. The geometry-consistency check must reject
        // it at parse time. Mutating orig_len (not n) keeps every hash/parity read in-bounds, so the
        // parse reaches the geometry check rather than tripping the earlier truncation guard.
        let data = sample(1_000_000);
        let (n, _m, _ss) = geometry(data.len() as u64, 0.1);
        assert!(n > 1, "test needs multiple data shards (n={n})");
        let mut side = create_sidecar(&data, 0.1).unwrap();
        // Header: magic(8) ver(1) n(u32 @9) m(u32 @13) shard_size(u64 @17) orig_len(u64 @25).
        side[25..33].copy_from_slice(&1u64.to_le_bytes());
        assert!(
            repair(&data, &side).is_err(),
            "an inconsistent-geometry sidecar must be rejected, not allocated from"
        );
        assert!(verify(&data, &side).is_err());
    }

    #[test]
    fn parse_survives_fuzzed_sidecars() {
        // A malformed / hostile `.cramrec` must always come back as `Err` — never a panic (OOB index,
        // integer overflow, `unwrap` on `None`) and never a giant allocation. Feeds pure-random and
        // mutated-from-valid sidecars to both public entry points.
        let mut x = 0xDEAD_BEEF_1234_5678u64;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let data = sample(1200);

        for _ in 0..4000 {
            let len = (next() % 400) as usize;
            let side: Vec<u8> = (0..len).map(|_| (next() >> 24) as u8).collect();
            let _ = verify(&data, &side); // Ok/Err, never panic
            let _ = repair(&data, &side);
        }
        // Mutate a VALID sidecar so the fuzzer reaches past the magic/version gate into the geometry and
        // shard-table parsing (byte-presence + geometry checks keep any allocation bounded).
        let base = create_sidecar(&data, 0.25).unwrap();
        for _ in 0..4000 {
            let mut s = base.clone();
            let ops = 1 + (next() % 6) as usize;
            for _ in 0..ops {
                let i = (next() as usize) % s.len();
                s[i] = (next() >> 24) as u8;
            }
            let _ = verify(&data, &s);
            let _ = repair(&data, &s);
        }
    }

    #[test]
    fn rejects_unrecoverable_damage() {
        let data = sample(500_000);
        let side = create_sidecar(&data, 0.1).unwrap(); // small parity
        let (_n, m, ss) = geometry(data.len() as u64, 0.1);
        // Damage m+1 distinct shards → beyond the parity budget → must fail cleanly (no panic).
        let mut corrupt = data.clone();
        for k in 0..m + 1 {
            corrupt[k * ss as usize + 1] ^= 0xFF;
        }
        assert!(repair(&corrupt, &side).is_err());
    }
}
