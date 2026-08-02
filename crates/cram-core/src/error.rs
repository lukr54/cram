//! Typed error + result model, replaces the old `Result<_, String>` / `Vec<(String,String)>` /
//! magic `"cancelled"` string. `Cancelled` is a first-class variant so the extract loop never
//! string-compares to detect cancellation.

pub type Result<T> = std::result::Result<T, ArchiveError>;

#[derive(thiserror::Error, Debug)]
pub enum ArchiveError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported or unrecognized archive format")]
    UnsupportedFormat,

    #[error("corrupt archive: {0}")]
    Corrupt(String),

    /// Zip-slip / path-traversal guard tripped (an entry tried to escape the output dir).
    #[error("unsafe entry path: {0}")]
    UnsafePath(String),

    #[error("password required")]
    PasswordRequired,

    #[error("wrong password")]
    WrongPassword,

    #[error("encryption not supported for this format/operation")]
    UnsupportedEncryption,

    /// e.g. attempting to create/modify a RAR archive.
    #[error("format is read-only")]
    ReadOnly,

    #[error("operation cancelled")]
    Cancelled,

    /// The content can't be extracted while it's still downloading (front-to-back streaming isn't
    /// possible for it, e.g. a zip whose entry sizes live in trailing data descriptors, an encrypted
    /// zip, or 7z/rar). NOT a failure: the caller should await the full download and extract normally.
    #[error("not stream-extractable; extract after the download completes")]
    StreamUnsupported,

    #[error("open the first volume of the multi-part archive first: {0}")]
    NeedFirstVolume(String),

    /// Escape hatch for backend-specific errors (unrar, sevenz, …).
    #[error("{0}")]
    Backend(String),
}

/// "Keep going, collect failures" extraction result; every backend funnels into this.
#[derive(Debug, Default)]
pub struct Report {
    /// Files written.
    pub extracted: u64,
    /// Files skipped because the destination already matched (skip-already-correct).
    pub skipped: u64,
    /// Uncompressed bytes written.
    pub bytes: u64,
    /// Per-entry failures: (entry name, message). Non-fatal; extraction continues.
    pub failed: Vec<(String, String)>,
    /// Entries refused by the path guard and never written (traversal names, absolute paths, a
    /// drive letter or ADS, a pathologically deep name). Dropping them is the correct action, but
    /// doing it silently makes a tampered archive indistinguishable from a nearly-empty one, so the
    /// count is carried out to the caller to report. `cram-extract` already prints its own.
    pub dropped_unsafe: u64,
    /// Set if the job was cancelled partway.
    pub cancelled: bool,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty() && !self.cancelled
    }
    pub fn push_failure(&mut self, name: impl Into<String>, err: impl std::fmt::Display) {
        self.failed.push((name.into(), err.to_string()));
    }
}
