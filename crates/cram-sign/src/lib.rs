//! Detached **ed25519** signatures for an arbitrary file (built for `.cram`, but format-agnostic).
//!
//! A checksum tells a recipient the download wasn't corrupted; a *signature* tells them **who** made it
//! and that it hasn't been altered since. The signer holds a private key; anyone with the matching
//! public key can verify. The signature lives in a separate `<file>.cramsig`, so it never touches the
//! (frozen) `.cram` format and works on any file.
//!
//! Sidecar byte layout (all fixed-size):
//! ```text
//!   magic(8) = "CRAMSIG\x01"
//!   public_key(32)     ed25519 verifying key of the signer
//!   file_hash(32)      BLAKE3 of the signed file (what the signature commits to)
//!   signature(64)      ed25519 over  DOMAIN || file_hash
//! ```
//! The signature covers a **domain-separated** message (`DOMAIN || file_hash`) so a `.cramsig` can
//! never be replayed as a signature for some other protocol that also signs raw hashes.

use std::fs;
use std::io::Read;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

pub mod cli;

/// Signature-sidecar magic + version.
pub const SIG_MAGIC: &[u8; 8] = b"CRAMSIG\x01";
/// Key-file magic + version.
pub const KEY_MAGIC: &[u8; 8] = b"CRAMKEY\x01";
/// Domain-separation tag mixed into every signed message.
const DOMAIN: &[u8] = b"cram-signature-v1";

const SIG_LEN: usize = 8 + 32 + 32 + 64;
const KEY_LEN: usize = 8 + 32;

/// A signing error. Kept as strings — this is a small standalone tool.
pub type SigResult<T> = Result<T, String>;

fn hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// The exact bytes an ed25519 signature commits to for a given file hash.
fn signing_message(file_hash: &[u8; 32]) -> Vec<u8> {
    let mut m = Vec::with_capacity(DOMAIN.len() + 32);
    m.extend_from_slice(DOMAIN);
    m.extend_from_slice(file_hash);
    m
}

/// Generate a fresh signing key. Returns `(key_file_bytes, public_key_hex)` — persist the key file
/// privately and publish/pin the hex public key so verifiers can confirm the signer.
pub fn generate_key() -> SigResult<(Vec<u8>, String)> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| format!("rng: {e}"))?;
    let sk = SigningKey::from_bytes(&seed);
    let pk_hex = hex(sk.verifying_key().as_bytes());
    let mut out = Vec::with_capacity(KEY_LEN);
    out.extend_from_slice(KEY_MAGIC);
    out.extend_from_slice(&seed);
    Ok((out, pk_hex))
}

/// Load a signing key produced by [`generate_key`].
pub fn load_key(bytes: &[u8]) -> SigResult<SigningKey> {
    if bytes.len() != KEY_LEN || &bytes[..8] != KEY_MAGIC {
        return Err("not a cram key file (bad magic or length)".into());
    }
    let seed: [u8; 32] = bytes[8..40].try_into().unwrap();
    Ok(SigningKey::from_bytes(&seed))
}

/// Assemble the `.cramsig` sidecar bytes for an already-computed file hash.
fn assemble_sidecar(key: &SigningKey, file_hash: &[u8; 32]) -> Vec<u8> {
    let sig = key.sign(&signing_message(file_hash));
    let mut out = Vec::with_capacity(SIG_LEN);
    out.extend_from_slice(SIG_MAGIC);
    out.extend_from_slice(key.verifying_key().as_bytes());
    out.extend_from_slice(file_hash);
    out.extend_from_slice(&sig.to_bytes());
    out
}

/// Sign `data`, returning the `.cramsig` sidecar bytes.
pub fn sign(data: &[u8], key: &SigningKey) -> Vec<u8> {
    assemble_sidecar(key, &hash(data))
}

/// BLAKE3 a file by streaming it in bounded chunks — never loads the whole file into RAM, so an
/// archive far larger than memory (hundreds of GB) can still be signed/verified.
fn hash_file(path: &Path) -> SigResult<[u8; 32]> {
    let mut f = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Sign the file at `path` (streaming its hash), returning the `.cramsig` sidecar bytes. Memory-bounded,
/// so unlike [`sign`] (which needs the whole file in memory) it works on archives larger than RAM.
pub fn sign_file(path: &Path, key: &SigningKey) -> SigResult<Vec<u8>> {
    Ok(assemble_sidecar(key, &hash_file(path)?))
}

/// A successful verification: who signed it and what was signed.
pub struct Verified {
    pub public_key_hex: String,
    pub file_hash_hex: String,
}

/// Verify `data` against its `sidecar`. `Ok` iff the signature is cryptographically valid **and** the
/// file's current bytes match what was signed. `expect_pubkey` (hex) optionally pins the acceptable
/// signer, so a valid signature from the *wrong* key is still rejected.
pub fn verify(data: &[u8], sidecar: &[u8], expect_pubkey: Option<&str>) -> SigResult<Verified> {
    verify_hashed(&hash(data), sidecar, expect_pubkey)
}

/// Verify the file at `path` against `sidecar` bytes, streaming the file's hash (memory-bounded) —
/// the large-file counterpart of [`verify`].
pub fn verify_file(
    path: &Path,
    sidecar: &[u8],
    expect_pubkey: Option<&str>,
) -> SigResult<Verified> {
    verify_hashed(&hash_file(path)?, sidecar, expect_pubkey)
}

/// Shared verification core over a pre-computed file hash.
fn verify_hashed(
    file_hash: &[u8; 32],
    sidecar: &[u8],
    expect_pubkey: Option<&str>,
) -> SigResult<Verified> {
    if sidecar.len() != SIG_LEN || &sidecar[..8] != SIG_MAGIC {
        return Err("not a cram signature (bad magic or length)".into());
    }
    let pk_bytes: [u8; 32] = sidecar[8..40].try_into().unwrap();
    let stored_hash: [u8; 32] = sidecar[40..72].try_into().unwrap();
    let sig_bytes: [u8; 64] = sidecar[72..136].try_into().unwrap();

    let pk = VerifyingKey::from_bytes(&pk_bytes).map_err(|e| format!("bad public key: {e}"))?;
    // Pin the signer first, if requested — don't even bother verifying a key we won't accept.
    if let Some(exp) = expect_pubkey {
        if !exp.trim().eq_ignore_ascii_case(&hex(&pk_bytes)) {
            return Err("signer's public key does not match the expected key".into());
        }
    }
    // Integrity: the file's current bytes must match the hash that was signed.
    if file_hash != &stored_hash {
        return Err(
            "file does not match its signature (modified, truncated, or wrong file)".into(),
        );
    }
    let sig = Signature::from_bytes(&sig_bytes);
    pk.verify(&signing_message(&stored_hash), &sig)
        .map_err(|_| "signature is invalid".to_string())?;
    Ok(Verified {
        public_key_hex: hex(&pk_bytes),
        file_hash_hex: hex(&stored_hash),
    })
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
    fn sign_then_verify_round_trips() {
        let (keyfile, pk_hex) = generate_key().unwrap();
        let key = load_key(&keyfile).unwrap();
        let data = sample(50_000);
        let sig = sign(&data, &key);

        let v = verify(&data, &sig, None).expect("valid signature verifies");
        assert_eq!(v.public_key_hex, pk_hex, "reports the signer");
        // Pinning the correct key also passes; pinning a different key fails.
        assert!(verify(&data, &sig, Some(&pk_hex)).is_ok());
        assert!(verify(&data, &sig, Some(&"00".repeat(32))).is_err());
    }

    #[test]
    fn tamper_and_wrong_key_are_rejected() {
        let (keyfile, _) = generate_key().unwrap();
        let key = load_key(&keyfile).unwrap();
        let data = sample(20_000);
        let sig = sign(&data, &key);

        // Flip one byte of the file → integrity check fails.
        let mut tampered = data.clone();
        tampered[10_000] ^= 0x80;
        assert!(
            verify(&tampered, &sig, None).is_err(),
            "modified file rejected"
        );

        // A signature from a DIFFERENT key over the same file must not verify against a mutated pubkey.
        let mut forged = sig.clone();
        forged[8] ^= 0x01; // corrupt the embedded public key
        assert!(verify(&data, &forged, None).is_err());

        // Corrupt signature bytes → invalid.
        let mut badsig = sign(&data, &key);
        let n = badsig.len();
        badsig[n - 1] ^= 0xFF;
        assert!(verify(&data, &badsig, None).is_err());
    }

    #[test]
    fn parse_survives_fuzzed_sidecars() {
        // A malformed signature/key file must be a clean `Err`, never a panic.
        let data = sample(500);
        let mut x = 0xF00D_BABE_1234_5678u64;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..4000 {
            let len = (next() % 200) as usize;
            let s: Vec<u8> = (0..len).map(|_| (next() >> 24) as u8).collect();
            let _ = verify(&data, &s, None);
            let _ = load_key(&s);
        }
    }
}
