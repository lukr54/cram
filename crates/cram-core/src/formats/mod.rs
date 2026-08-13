//! Format dispatch: turn a sniffed [`Format`] into a concrete reader/writer. Only ZIP read is wired
//! for the core; the tar/rar/7z/raw backends slot in here as they land.

use std::path::Path;
use std::sync::Arc;

use crate::error::{ArchiveError, Result};
use crate::format::{Codec, Container, Format};
use crate::reader::{ArchiveReader, RandomAccessReader};
use crate::secret::PasswordProvider;
use crate::writer::{ArchiveWriter, CreateOptions};

pub mod cram;
pub mod iso;
pub(crate) mod lzma2seg;
pub mod rar;
pub mod raw;
pub mod seqcache;
pub mod sevenz;
pub mod sevenz_write;
pub mod tar;
pub mod tar_write;
pub mod zip;
pub mod zip_write;

/// Open an archive for reading. `pw` supplies passwords lazily for encrypted entries/headers.
pub fn open(
    path: &Path,
    fmt: Format,
    pw: Arc<dyn PasswordProvider>,
) -> Result<Box<dyn ArchiveReader>> {
    match fmt.container {
        Container::Zip => Ok(Box::new(zip::ZipReader::open(path, pw)?)),
        Container::Rar => Ok(Box::new(rar::RarReader::open(path, pw)?)),
        Container::SevenZ => Ok(Box::new(sevenz::SevenZReader::open(path, pw)?)),
        Container::Tar => Ok(Box::new(tar::TarReader::open(path, fmt, pw)?)),
        Container::Raw => Ok(Box::new(raw::RawReader::open(path, fmt, pw)?)),
        Container::Cram => Ok(Box::new(cram::CramReader::open(path, pw)?)),
        Container::Iso => Ok(Box::new(iso::IsoReader::open(path, pw)?)),
    }
}

/// Open an archive as a [`RandomAccessReader`], the mount / on-access primitive.
///
/// Two tiers back this:
/// - **Natively seekable**, ZIP (central directory + per-entry local headers), `.cram` (a footer
///   index over content-addressed packs), and ISO 9660 (each file is a contiguous extent). These serve
///   ranges straight from disk with no whole-archive buffering.
/// - **Sequential, staged to RAM**; tar / 7z / rar / raw, so [`seqcache::SeqCacheReader`] decodes
///   them once into a bounded in-memory cache and serves ranges from there. Capped, so a too-large
///   archive is refused (extract it instead of mounting). tar, rar and raw have no seek boundary at
///   all. 7z does — [`sevenz::SevenZRandomAccess`], which extraction and `verify` fan out over — but
///   its smallest addressable unit is a solid block or an LZMA2 segment, so serving a mount's small
///   reads through it would decode one of those per read. **This dispatch is on `Container`, not on
///   `ArchiveReader::as_random_access`**: giving a backend the latter does not put it in the tier
///   above, and the two decisions are deliberately separate.
///
/// `pw` supplies passwords lazily, exactly as [`open`] does.
pub fn open_random_access(
    path: &Path,
    fmt: Format,
    pw: Arc<dyn PasswordProvider>,
) -> Result<Box<dyn RandomAccessReader>> {
    open_random_access_with(path, fmt, pw, cram::CacheProfile::Extract)
}

/// As [`open_random_access`], but states what the reader is for so it can size its caches.
///
/// Only `.cram` acts on it today, and only for the decompressed-pack cache: a mount is one reader
/// doing local reads that stays open for hours, where extraction's sizing (one pack per worker)
/// means hundreds of megabytes resident to serve a pattern that needs a handful of packs.
pub fn open_random_access_with(
    path: &Path,
    fmt: Format,
    pw: Arc<dyn PasswordProvider>,
    profile: cram::CacheProfile,
) -> Result<Box<dyn RandomAccessReader>> {
    match fmt.container {
        Container::Zip => Ok(Box::new(zip::ZipReader::open(path, pw)?)),
        Container::Cram => Ok(Box::new(cram::CramReader::open_with_cache(
            path, pw, profile,
        )?)),
        Container::Iso => Ok(Box::new(iso::IsoReader::open(path, pw)?)),
        // No native random-access interface → decode the whole stream into a bounded in-memory cache.
        Container::Tar | Container::SevenZ | Container::Rar | Container::Raw => {
            Ok(Box::new(seqcache::SeqCacheReader::decode(path, fmt, pw)?))
        }
    }
}

/// Open a writer to create an archive at `path`. `RAR` is rejected (read-only); `7z`/tar/`.cram`
/// writers land with their create phases. Encryption/level/codec come from [`CreateOptions`].
pub fn create(path: &Path, fmt: Format, opts: &CreateOptions) -> Result<Box<dyn ArchiveWriter>> {
    if !fmt.is_writable() {
        return Err(ArchiveError::ReadOnly);
    }
    match fmt.container {
        Container::Zip => Ok(Box::new(zip_write::ZipArchiveWriter::create(path, opts)?)),
        Container::Tar => Ok(Box::new(tar_write::TarArchiveWriter::create(
            path, fmt, opts,
        )?)),
        Container::SevenZ => Ok(Box::new(sevenz_write::SevenZArchiveWriter::create(
            path, opts,
        )?)),
        Container::Cram => Ok(Box::new(cram::CramArchiveWriter::create(path, opts)?)),
        _ => Err(ArchiveError::UnsupportedFormat),
    }
}

/// One archive kind `create` can write, named by the extension that selects it.
pub struct CreateTarget {
    /// Extension including the leading dot. Matching is longest-first, so `.tar.gz` wins over
    /// `.tar` regardless of the order here.
    pub ext: &'static str,
    pub container: Container,
    pub codec: Codec,
    /// Whether a picker should offer this name. The rest are accepted aliases (`.tgz` is written
    /// by anyone who types it, and is not worth a second tile).
    pub offer: bool,
}

/// **Every archive Cram can create, in one table.**
///
/// This existed twice before: `cram-cli` and the Studio GUI each carried their own extension match,
/// and they had drifted four formats apart, so the GUI silently offered less than the engine could
/// do and nothing failed to tell anyone. Anything that needs to know what can be created -- the
/// extension matcher below, the CLI's error text, the GUI's format picker -- reads this and only
/// this.
///
/// RAR and ISO are absent because they are not writable ([`Format::is_writable`]): creating RAR is
/// forbidden by the UnRAR licence, permanently.
pub const CREATE_TARGETS: &[CreateTarget] = &[
    t(".cram", Container::Cram, Codec::None, true),
    t(".zip", Container::Zip, Codec::None, true),
    t(".7z", Container::SevenZ, Codec::None, true),
    t(".tar", Container::Tar, Codec::None, true),
    t(".tar.gz", Container::Tar, Codec::Gzip, true),
    t(".tgz", Container::Tar, Codec::Gzip, false),
    t(".tar.xz", Container::Tar, Codec::Xz, true),
    t(".txz", Container::Tar, Codec::Xz, false),
    t(".tar.zst", Container::Tar, Codec::Zstd, true),
    t(".tzst", Container::Tar, Codec::Zstd, false),
    t(".tar.bz2", Container::Tar, Codec::Bzip2, true),
    t(".tbz2", Container::Tar, Codec::Bzip2, false),
    t(".tbz", Container::Tar, Codec::Bzip2, false),
    t(".tar.lz4", Container::Tar, Codec::Lz4, true),
    t(".tar.br", Container::Tar, Codec::Brotli, true),
];

const fn t(ext: &'static str, container: Container, codec: Codec, offer: bool) -> CreateTarget {
    CreateTarget {
        ext,
        container,
        codec,
        offer,
    }
}

/// The format a new archive at `path` should be, from its name alone.
///
/// Longest match wins, so `out.tar.gz` is a gzipped tar rather than a bare tar that happens to end
/// in `.gz`. Nothing about the file is read: it does not exist yet.
pub fn format_for_new(path: &Path) -> Result<Format> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    CREATE_TARGETS
        .iter()
        .filter(|t| name.ends_with(t.ext))
        .max_by_key(|t| t.ext.len())
        .map(|t| Format {
            container: t.container,
            codec: t.codec,
        })
        .ok_or_else(|| ArchiveError::Backend(format!("create supports {}", creatable_list())))
}

/// The offered extensions, comma separated, for error messages and help text.
pub fn creatable_list() -> String {
    CREATE_TARGETS
        .iter()
        .filter(|t| t.offer)
        .map(|t| t.ext)
        .collect::<Vec<_>>()
        .join(" ")
}
