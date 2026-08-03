//! Adaptive create-side probe: decide, per file, whether compressing it is worth the CPU; the
//! "store the incompressible" optimization. Already-compressed media/archives (JPEG, MP4, ZIP, …)
//! do not shrink under DEFLATE/LZMA; running the compressor over them only burns CPU and often
//! grows the data slightly. Detecting them and storing them verbatim is the single biggest
//! real-world archiver win, and it is what [`Level::Auto`](crate::writer::Level) turns on.
//!
//! Two-tier classification, cheapest first:
//!   1. **Extension**, a hard list of formats that are essentially always incompressible (store)
//!      or reliably compressible (compress), decided with zero file I/O.
//!   2. **Content sample**, for unknown extensions, read a small head sample and measure how far a
//!      fast DEFLATE pass shrinks it (with a Shannon-entropy short-circuit for obvious noise).
//!
//! The whole-archive [`ProbeSummary`] aggregates the per-file verdicts so a whole-stream backend
//! (`tar.gz`/`tar.xz`, which cannot vary the codec per entry) can drop to a fast level when the
//! input is dominated by already-compressed bytes.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use flate2::write::DeflateEncoder;
use flate2::Compression;

/// Per-file compression decision from the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compressibility {
    /// Already-compressed / high-entropy → store verbatim (no codec).
    Store,
    /// Worth compressing.
    Compress,
}

impl Compressibility {
    pub fn is_store(self) -> bool {
        matches!(self, Compressibility::Store)
    }
}

/// Bytes read from the head of a file to judge compressibility.
const SAMPLE_BYTES: u64 = 64 * 1024;
/// Below this size, don't sample; just compress (per-entry overhead is tiny and the store/compress
/// choice barely matters for a sub-512-byte file).
const MIN_SAMPLE: u64 = 512;
/// DEFLATE shrink ratio (compressed/original) at or above which the data is deemed incompressible.
const INCOMPRESSIBLE_RATIO: f64 = 0.95;
/// Shannon entropy (bits/byte) at or above which we skip the DEFLATE probe and call it incompressible.
const HIGH_ENTROPY_BITS: f64 = 7.8;

/// Lowercased final extension of a path (without the dot), or "" if none. For `foo.tar.gz` this is
/// `gz` (→ store), for `foo.tar` it is `tar` (→ compress), exactly the right granularity.
fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Formats that are already compressed or encrypted, DEFLATE/LZMA cannot shrink them. Covers
/// raster images, audio, video, already-compressed archives/installers (note `.tar.gz` → ext `gz`,
/// so tarballs land here), zip/deflate-based containers (office, apk, jar…), web fonts, compressed
/// disk images, and encrypted blobs.
#[rustfmt::skip]
const STORE_EXTS: &[&str] = &[
    // images
    "jpg", "jpeg", "jpe", "png", "gif", "webp", "heic", "heif", "avif", "jxl",
    // audio
    "mp3", "aac", "m4a", "m4b", "ogg", "oga", "opus", "flac", "wma", "ape",
    // video (NB: `.ts` is NOT here, it is overwhelmingly TypeScript source, which is
    // highly compressible; a genuine MPEG transport stream is high-entropy and the content sample
    // stores it anyway. `.m2ts` is unambiguously video.)
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "flv", "m2ts", "vob", "mpg", "mpeg",
    // compressed archives / installers
    "zip", "7z", "rar", "gz", "tgz", "xz", "txz", "zst", "tzst", "bz2", "tbz2", "tbz", "lz4", "br",
    "cab", "arj", "lzh", "lha", "z",
    // zip/deflate-based containers
    "jar", "war", "ear", "apk", "aab", "ipa", "xpi", "crx", "nupkg", "whl", "egg", "vsix",
    "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "kra", "ora",
    // fonts / disk images / encrypted
    "woff", "woff2", "dmg", "gpg", "pgp", "aes",
];

/// Formats that reliably compress well, skip the content sample for these. Covers text/code/markup/
/// data and uncompressed media (bmp/tiff/wav…). Deliberately excludes ambiguous binaries
/// (exe/dll/pdf/iso/db…): those fall through to the content sample, which judges them correctly
/// instead of guessing from the extension.
#[rustfmt::skip]
const COMPRESS_EXTS: &[&str] = &[
    // text / code / markup / data
    "txt", "text", "log", "csv", "tsv", "json", "ndjson", "xml", "html", "htm", "css", "js", "mjs",
    "jsx", "tsx", "md", "markdown", "rst", "yaml", "yml", "toml", "ini", "cfg", "conf", "c", "h",
    "cpp", "cxx", "cc", "hpp", "hh", "rs", "go", "py", "rb", "java", "kt", "cs", "php", "pl", "pm",
    "lua", "sh", "bash", "zsh", "bat", "cmd", "ps1", "sql", "svg", "tex", "rtf",
    // uncompressed media / raw data
    "bmp", "dib", "tif", "tiff", "wav", "aiff", "aif", "pcm", "tar",
];

/// Extension-only verdict, or `None` when the extension is unknown/ambiguous (→ sample the content).
fn ext_verdict(ext: &str) -> Option<Compressibility> {
    if STORE_EXTS.contains(&ext) {
        Some(Compressibility::Store)
    } else if COMPRESS_EXTS.contains(&ext) {
        Some(Compressibility::Compress)
    } else {
        None
    }
}

/// Shannon entropy in bits/byte of a sample (0..=8). Near 8 ⇒ close to random ⇒ incompressible.
fn shannon_bits(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let n = data.len() as f64;
    let mut h = 0.0;
    for &c in counts.iter() {
        if c > 0 {
            let p = c as f64 / n;
            h -= p * p.log2();
        }
    }
    h
}

/// Fraction a fast DEFLATE pass shrinks `data` to (compressed/original). ~1.0 = incompressible.
fn deflate_ratio(data: &[u8]) -> f64 {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
    if enc.write_all(data).is_err() {
        return 1.0;
    }
    match enc.finish() {
        Ok(out) => out.len() as f64 / data.len().max(1) as f64,
        Err(_) => 1.0,
    }
}

/// Measured compression ratio of a raw byte sample (compressed/original, so 0.25 = shrinks to a
/// quarter, ~1.0 = incompressible). Uses the same cheap entropy short-circuit as [`sample_verdict`],
/// then a fast DEFLATE pass.
///
/// This is a **conservative** signal for estimating a final archive size: the real writers (XZ /
/// zstd, and `.cram`'s global dedup) do better than fast-DEFLATE on a head sample, so a size derived
/// from this will usually be a little larger than what actually lands on disk.
pub fn sample_ratio(sample: &[u8]) -> f64 {
    if (sample.len() as u64) < MIN_SAMPLE {
        return 1.0; // too little to judge, assume no gain rather than promise one
    }
    if shannon_bits(sample) >= HIGH_ENTROPY_BITS {
        return 1.0;
    }
    deflate_ratio(sample).clamp(0.02, 1.0)
}

/// Read a head sample of `path` and return its measured compression ratio (see [`sample_ratio`]).
/// Any I/O problem yields `1.0` (assume incompressible) so an estimate never over-promises.
pub fn file_ratio(path: &Path, size: u64) -> f64 {
    if size < MIN_SAMPLE {
        return 1.0;
    }
    // A known-compressed extension is incompressible without touching the disk.
    if matches!(ext_verdict(&ext_of(path)), Some(Compressibility::Store)) {
        return 1.0;
    }
    let mut buf = Vec::with_capacity(SAMPLE_BYTES.min(size) as usize);
    match File::open(path).and_then(|f| f.take(SAMPLE_BYTES).read_to_end(&mut buf)) {
        Ok(_) if !buf.is_empty() => sample_ratio(&buf),
        _ => 1.0,
    }
}

/// Judge a raw byte sample: entropy short-circuit first (cheap), then a fast DEFLATE ratio.
pub fn sample_verdict(sample: &[u8]) -> Compressibility {
    if (sample.len() as u64) < MIN_SAMPLE {
        return Compressibility::Compress;
    }
    if shannon_bits(sample) >= HIGH_ENTROPY_BITS {
        return Compressibility::Store;
    }
    if deflate_ratio(sample) >= INCOMPRESSIBLE_RATIO {
        Compressibility::Store
    } else {
        Compressibility::Compress
    }
}

/// Classify a file on disk. Extension first (no I/O); for unknown extensions read a small head
/// sample. Any I/O error falls back to `Compress` (safe: worst case we just spend the codec CPU).
/// The verdict the extension alone settles, if it settles one.
///
/// [`classify_file`] consults this before opening anything, and exposing it lets a caller that has
/// already opened the file reproduce the same decision without a second open. The create loop does
/// exactly that: it is holding the handle, so paying for another one to ask the same question is
/// waste, and `File::open` is the largest single cost in create.
pub fn ext_only_verdict(path: &Path) -> Option<Compressibility> {
    ext_verdict(&ext_of(path))
}

/// How many bytes [`classify_file`] samples, and the size below which it does not bother.
/// Public so an inline classifier can match its behaviour exactly rather than approximate it.
pub const PROBE_SAMPLE_BYTES: u64 = SAMPLE_BYTES;
pub const PROBE_MIN_SAMPLE: u64 = MIN_SAMPLE;

pub fn classify_file(path: &Path, size: u64) -> Compressibility {
    if size == 0 {
        return Compressibility::Compress; // empty: store/compress are equivalent; keep it simple
    }
    if let Some(v) = ext_verdict(&ext_of(path)) {
        return v;
    }
    if size < MIN_SAMPLE {
        return Compressibility::Compress;
    }
    let mut buf = Vec::with_capacity(SAMPLE_BYTES.min(size) as usize);
    match File::open(path).and_then(|f| f.take(SAMPLE_BYTES).read_to_end(&mut buf)) {
        Ok(_) if !buf.is_empty() => sample_verdict(&buf),
        _ => Compressibility::Compress,
    }
}

/// Aggregate compressibility of a whole input set, lets a whole-stream backend pick a level.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeSummary {
    pub files: u64,
    pub total_bytes: u64,
    pub store_files: u64,
    pub store_bytes: u64,
}

impl ProbeSummary {
    /// Fold one file's verdict into the running totals.
    pub fn add(&mut self, size: u64, verdict: Compressibility) {
        self.files += 1;
        self.total_bytes += size;
        if verdict.is_store() {
            self.store_files += 1;
            self.store_bytes += size;
        }
    }

    /// Fraction of bytes judged incompressible (0.0 if empty).
    pub fn store_fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.store_bytes as f64 / self.total_bytes as f64
        }
    }

    /// The input is dominated by already-compressed data → whole-stream codecs should not spend
    /// effort on it.
    pub fn mostly_incompressible(&self) -> bool {
        self.store_fraction() >= 0.9
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes (xorshift), incompressible, no `rand` dep.
    fn noise(len: usize) -> Vec<u8> {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn extension_fast_paths() {
        assert_eq!(ext_verdict("jpg"), Some(Compressibility::Store));
        assert_eq!(ext_verdict("mp4"), Some(Compressibility::Store));
        assert_eq!(ext_verdict("gz"), Some(Compressibility::Store)); // .tar.gz → ext "gz"
        assert_eq!(ext_verdict("txt"), Some(Compressibility::Compress));
        assert_eq!(ext_verdict("rs"), Some(Compressibility::Compress));
        assert_eq!(ext_verdict("tar"), Some(Compressibility::Compress));
        assert_eq!(ext_verdict("unknownext"), None);
        // `.ts` is TypeScript far more often than MPEG-TS → sample it, don't hard-store.
        assert_eq!(ext_verdict("ts"), None);
        assert_eq!(ext_verdict("tsx"), Some(Compressibility::Compress));
        assert_eq!(ext_verdict("m2ts"), Some(Compressibility::Store));
    }

    #[test]
    fn sample_noise_is_store() {
        let data = noise(64 * 1024);
        assert!(shannon_bits(&data) >= HIGH_ENTROPY_BITS);
        assert_eq!(sample_verdict(&data), Compressibility::Store);
    }

    #[test]
    fn sample_text_is_compress() {
        let unit = b"the quick brown fox jumps over the lazy dog. ";
        let mut data = Vec::new();
        while data.len() < 64 * 1024 {
            data.extend_from_slice(unit);
        }
        assert!(shannon_bits(&data) < HIGH_ENTROPY_BITS);
        assert!(deflate_ratio(&data) < INCOMPRESSIBLE_RATIO);
        assert_eq!(sample_verdict(&data), Compressibility::Compress);
    }

    #[test]
    fn tiny_sample_defaults_to_compress() {
        assert_eq!(sample_verdict(b"short"), Compressibility::Compress);
    }

    #[test]
    fn summary_tracks_fraction() {
        let mut s = ProbeSummary::default();
        s.add(900, Compressibility::Store);
        s.add(100, Compressibility::Compress);
        assert_eq!(s.files, 2);
        assert_eq!(s.store_files, 1);
        assert!((s.store_fraction() - 0.9).abs() < 1e-9);
        assert!(s.mostly_incompressible());
    }
}
