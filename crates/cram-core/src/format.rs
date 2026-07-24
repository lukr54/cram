//! Format model: a `Format` is a `Container` × `Codec`, so compound formats compose instead of
//! multiplying into a flat enum. `.zip` = {Zip, None}; `.tar.gz` = {Tar, Gzip}; a bare `foo.xz`
//! = {Raw, Xz} (the decoded stream *is* the single entry). Detection lives in [`crate::sniff`].

/// The container structure that holds entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// PKZIP, per-entry compressed, random-access (the parallel fast path).
    Zip,
    /// 7z, solid/blocked LZMA2 et al.
    SevenZ,
    /// RAR, read-only (creating RAR is legally forbidden).
    Rar,
    /// tar, uncompressed concatenation, usually wrapped in a whole-stream `Codec`.
    Tar,
    /// Cram-native chunk-store (cross-file dedup), see the dedup format design.
    Cram,
    /// ISO 9660 (+ Joliet) CD/DVD image, read-only, uncompressed extents, random-access/mountable.
    Iso,
    /// No container: the decoded `Codec` stream is itself a single entry (`foo.gz`, `foo.xz`).
    Raw,
}

/// A whole-stream compression codec (used bare for `Raw`, or wrapping `Tar`/`Cram`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    None,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
    Lz4,
    Brotli,
}

/// A concrete archive format = container + whole-stream codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub container: Container,
    pub codec: Codec,
}

impl Format {
    pub const fn new(container: Container, codec: Codec) -> Self {
        Self { container, codec }
    }
    pub const fn zip() -> Self {
        Self::new(Container::Zip, Codec::None)
    }
    pub const fn sevenz() -> Self {
        Self::new(Container::SevenZ, Codec::None)
    }
    pub const fn rar() -> Self {
        Self::new(Container::Rar, Codec::None)
    }
    pub const fn tar(codec: Codec) -> Self {
        Self::new(Container::Tar, codec)
    }
    pub const fn raw(codec: Codec) -> Self {
        Self::new(Container::Raw, codec)
    }
    pub const fn cram(codec: Codec) -> Self {
        Self::new(Container::Cram, codec)
    }
    pub const fn iso() -> Self {
        Self::new(Container::Iso, Codec::None)
    }

    /// ZIP, `.cram`, and ISO 9660 support cheap random access → the parallel-per-entry extraction
    /// fast path and on-access mount (ISO stores uncompressed contiguous extents). Everything else is
    /// a front-to-back stream.
    pub fn is_random_access(&self) -> bool {
        matches!(
            self.container,
            Container::Zip | Container::Cram | Container::Iso
        )
    }

    /// Whether Cram can *create* this format. RAR and ISO are read-only (we don't author them);
    /// Raw single-streams are codec-only.
    pub fn is_writable(&self) -> bool {
        !matches!(self.container, Container::Rar | Container::Iso)
    }

    /// A short human label, e.g. "zip", "tar.gz", "7z".
    pub fn label(&self) -> &'static str {
        use Codec::*;
        use Container::*;
        match (self.container, self.codec) {
            (Zip, _) => "zip",
            (SevenZ, _) => "7z",
            (Rar, _) => "rar",
            (Cram, _) => "cram",
            (Iso, _) => "iso",
            (Tar, None) => "tar",
            (Tar, Gzip) => "tar.gz",
            (Tar, Xz) => "tar.xz",
            (Tar, Zstd) => "tar.zst",
            (Tar, Bzip2) => "tar.bz2",
            (Tar, Lz4) => "tar.lz4",
            (Tar, Brotli) => "tar.br",
            (Raw, Gzip) => "gz",
            (Raw, Xz) => "xz",
            (Raw, Zstd) => "zst",
            (Raw, Bzip2) => "bz2",
            (Raw, Lz4) => "lz4",
            (Raw, Brotli) => "br",
            (Raw, None) => "bin",
        }
    }
}
