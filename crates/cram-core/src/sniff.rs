//! Format detection by MAGIC BYTES, with the file extension used only as a tiebreaker (and to
//! disambiguate a compressed stream that is really a `.tar.*`). Magic always wins over extension,
//! so a `.zip` that is actually a RAR is handled correctly.
//!
//! NOTE: for a bare compressed stream (`{Raw|Tar, codec}`) telling `Tar` from `Raw` ideally means
//! decoding a prefix and checking for the `ustar` magic, that "peek inside the codec" upgrade
//! lands with the codec layer. Until then we disambiguate `.tar.gz`/`.tgz`-style names by extension.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{ArchiveError, Result};
use crate::format::{Codec, Container, Format};

/// Native Cram chunk-store magic (cross-file dedup format).
pub const CRAM_MAGIC: &[u8] = b"CRAM\x1b\x01";

/// Detect a format from a leading byte prefix (≥ 512 bytes recommended for the tar `ustar` check).
/// Returns `None` if no container/codec magic matches (caller then tries the extension).
pub fn sniff_bytes(head: &[u8]) -> Option<Format> {
    // --- containers with their own magic ---
    if head.starts_with(b"PK\x03\x04")
        || head.starts_with(b"PK\x05\x06")
        || head.starts_with(b"PK\x07\x08")
    {
        return Some(Format::zip());
    }
    if head.starts_with(b"Rar!\x1a\x07") {
        return Some(Format::rar());
    }
    if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Some(Format::sevenz());
    }
    if head.starts_with(CRAM_MAGIC) {
        return Some(Format::cram(Codec::None));
    }
    // tar "ustar" magic sits at offset 257.
    if head.len() >= 262 && &head[257..262] == b"ustar" {
        return Some(Format::tar(Codec::None));
    }

    // --- whole-stream codecs (container = Raw for now; may be upgraded to Tar by extension/peek) ---
    let codec = if head.starts_with(&[0x1f, 0x8b]) {
        Codec::Gzip
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        Codec::Xz
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Codec::Zstd
    } else if head.starts_with(b"BZh") {
        Codec::Bzip2
    } else if head.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        Codec::Lz4
    } else {
        return None; // brotli has no magic; unknown → let the extension decide
    };
    Some(Format::raw(codec))
}

/// A tar-family extension → the wrapping codec, if the name looks like `.tar.gz`/`.tgz`/…
fn tar_codec_from_ext(name: &str) -> Option<Codec> {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(Codec::Gzip)
    } else if n.ends_with(".tar.xz") || n.ends_with(".txz") {
        Some(Codec::Xz)
    } else if n.ends_with(".tar.zst") || n.ends_with(".tzst") {
        Some(Codec::Zstd)
    } else if n.ends_with(".tar.bz2") || n.ends_with(".tbz2") || n.ends_with(".tbz") {
        Some(Codec::Bzip2)
    } else if n.ends_with(".tar.lz4") {
        Some(Codec::Lz4)
    } else if n.ends_with(".tar.br") {
        Some(Codec::Brotli)
    } else {
        None
    }
}

/// Pure extension-based fallback (for magicless formats like brotli, or truncated heads).
fn sniff_ext(name: &str) -> Option<Format> {
    let n = name.to_ascii_lowercase();
    if let Some(c) = tar_codec_from_ext(&n) {
        return Some(Format::tar(c));
    }
    let f = match () {
        _ if n.ends_with(".zip") => Format::zip(),
        _ if n.ends_with(".7z") => Format::sevenz(),
        _ if n.ends_with(".rar") => Format::rar(),
        _ if n.ends_with(".tar") => Format::tar(Codec::None),
        _ if n.ends_with(".cram") => Format::cram(Codec::None),
        _ if n.ends_with(".iso") => Format::iso(),
        _ if n.ends_with(".gz") => Format::raw(Codec::Gzip),
        _ if n.ends_with(".xz") => Format::raw(Codec::Xz),
        _ if n.ends_with(".zst") => Format::raw(Codec::Zstd),
        _ if n.ends_with(".bz2") => Format::raw(Codec::Bzip2),
        _ if n.ends_with(".lz4") => Format::raw(Codec::Lz4),
        _ if n.ends_with(".br") => Format::raw(Codec::Brotli),
        _ => return None,
    };
    Some(f)
}

/// Detect the format of a file: read a prefix, match magic, then use the name to (a) upgrade a
/// bare codec stream to a `.tar.*`, and (b) fall back when there's no magic.
pub fn sniff_path(path: &Path) -> Result<Format> {
    // Checked before the open, not after: every read verb funnels through here, and a directory
    // handed to any of them would otherwise surface as the platform's error for opening a folder as
    // a file. Naming the mistake is the whole point.
    if path.is_dir() {
        return Err(ArchiveError::NotAnArchive(path.display().to_string()));
    }
    let mut head = [0u8; 512];
    let n = File::open(path)?.read(&mut head).unwrap_or(0);
    let head = &head[..n];
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");

    if let Some(fmt) = sniff_bytes(head) {
        // A bare {Raw, codec} whose name says `.tar.*` is really a tar wrapped in that codec.
        if fmt.container == Container::Raw {
            if let Some(tc) = tar_codec_from_ext(name) {
                if tc == fmt.codec {
                    return Ok(Format::tar(fmt.codec));
                }
            }
        }
        return Ok(fmt);
    }
    // ISO 9660's `CD001` marker sits at offset 32769 (sector 16, +1), well past the 512-byte head, so
    // it can't live in `sniff_bytes`. Probe it directly, this catches an ISO regardless of extension
    // (a raw `.img` disc image) before falling back to the name.
    if is_iso9660(path) {
        return Ok(Format::iso());
    }
    sniff_ext(name).ok_or(ArchiveError::UnsupportedFormat)
}

/// True if the file carries the ISO 9660 `CD001` standard identifier at the primary volume descriptor
/// (byte offset 32769). Cheap: a single 5-byte read at a fixed offset.
fn is_iso9660(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    if f.seek(SeekFrom::Start(32769)).is_err() {
        return false;
    }
    let mut m = [0u8; 5];
    f.read_exact(&mut m).is_ok() && &m == b"CD001"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_magics() {
        assert_eq!(sniff_bytes(b"PK\x03\x04rest").unwrap(), Format::zip());
        assert_eq!(sniff_bytes(b"Rar!\x1a\x07\x00").unwrap(), Format::rar());
        assert_eq!(
            sniff_bytes(b"7z\xbc\xaf\x27\x1c").unwrap(),
            Format::sevenz()
        );
        assert_eq!(sniff_bytes(CRAM_MAGIC).unwrap().container, Container::Cram);
    }

    #[test]
    fn codec_magics() {
        assert_eq!(
            sniff_bytes(&[0x1f, 0x8b, 0x08]).unwrap(),
            Format::raw(Codec::Gzip)
        );
        assert_eq!(
            sniff_bytes(&[0x28, 0xb5, 0x2f, 0xfd, 0]).unwrap(),
            Format::raw(Codec::Zstd)
        );
        assert_eq!(sniff_bytes(b"BZh9").unwrap(), Format::raw(Codec::Bzip2));
        assert!(sniff_bytes(b"not-an-archive").is_none());
    }

    #[test]
    fn tar_ustar_at_257() {
        let mut head = vec![0u8; 300];
        head[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff_bytes(&head).unwrap(), Format::tar(Codec::None));
    }

    /// A folder must be named as such rather than reported as whatever the platform says about
    /// opening a directory as a file — on Windows that is "Access is denied", which reads as a
    /// permissions fault.
    #[test]
    fn a_directory_is_not_an_archive() {
        let dir = std::env::temp_dir().join("cram-sniff-dir-test");
        std::fs::create_dir_all(&dir).unwrap();
        let err = sniff_path(&dir).unwrap_err();
        assert!(
            matches!(err, ArchiveError::NotAnArchive(_)),
            "expected NotAnArchive, got {err:?}"
        );
        assert!(err.to_string().contains("is a folder, not an archive"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
