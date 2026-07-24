//! Format dispatch: turn a sniffed [`Format`] into a concrete reader/writer. Only ZIP read is wired
//! for the spine; the tar/rar/7z/raw backends slot in here as they land.

use std::path::Path;
use std::sync::Arc;

use crate::error::{ArchiveError, Result};
use crate::format::{Container, Format};
use crate::reader::{ArchiveReader, RandomAccessReader};
use crate::secret::PasswordProvider;
use crate::writer::{ArchiveWriter, CreateOptions};

pub mod cram;
pub mod iso;
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
/// - **Sequential, staged to RAM**; tar / 7z / rar / raw are front-to-back streams with no seek seam,
///   so [`seqcache::SeqCacheReader`] decodes them once into a bounded in-memory cache and serves ranges
///   from there. Capped, so a too-large archive is refused (extract it instead of mounting).
///
/// `pw` supplies passwords lazily, exactly as [`open`] does.
pub fn open_random_access(
    path: &Path,
    fmt: Format,
    pw: Arc<dyn PasswordProvider>,
) -> Result<Box<dyn RandomAccessReader>> {
    match fmt.container {
        Container::Zip => Ok(Box::new(zip::ZipReader::open(path, pw)?)),
        Container::Cram => Ok(Box::new(cram::CramReader::open(path, pw)?)),
        Container::Iso => Ok(Box::new(iso::IsoReader::open(path, pw)?)),
        // No native random-access seam → decode the whole stream into a bounded in-memory cache.
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
