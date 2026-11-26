//! Passing macOS's download quarantine on from an archive to what is extracted from it.
//!
//! A browser marks what it downloads with the `com.apple.quarantine` extended attribute, and
//! Gatekeeper checks an app or script carrying it the first time it is opened. An extractor that
//! drops the mark hands the user unmarked executables from an archive that was marked, so Gatekeeper
//! never sees them.
//!
//! Measured on macOS 26.5.1 with a marked zip and tar: `unzip`, `ditto -x -k` and `bsdtar` each
//! copied the archive's value byte for byte onto every file and every directory they extracted,
//! the inside of an `.app` bundle included. Cram 1.3.2 marked none of them. This does what they do.
//!
//! Everything here is a no-op on other platforms, and on macOS when the archive carries no mark.

use std::path::Path;

/// The archive's quarantine value, if it has one, to copy onto everything extracted from it.
#[derive(Default, Clone)]
pub struct Mark(Option<Vec<u8>>);

impl Mark {
    /// Read the mark of the archive at `path`. Absent, unreadable or on another platform: none.
    pub fn of(path: &Path) -> Self {
        Mark(imp::read(path))
    }

    /// Whether there is anything to apply.
    pub fn is_set(&self) -> bool {
        self.0.is_some()
    }

    /// Apply to a file we hold open. Best-effort, like the mtime: a volume that refuses extended
    /// attributes is not a reason to fail an extraction.
    pub fn apply_open(&self, file: &std::fs::File) {
        if let Some(v) = &self.0 {
            imp::write_open(file, v);
        }
    }

    /// Apply to a path, without following a symbolic link. Used for directories.
    pub fn apply_path(&self, path: &Path) {
        if let Some(v) = &self.0 {
            imp::write_path(path, v);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    const NAME: &[u8] = b"com.apple.quarantine\0";

    fn cpath(p: &Path) -> Option<CString> {
        CString::new(p.as_os_str().as_bytes()).ok()
    }

    pub fn read(path: &Path) -> Option<Vec<u8>> {
        let p = cpath(path)?;
        // The value is a short `flags;time;agent;uuid` string; 4 KiB is far beyond any seen, and a
        // value that does not fit (ERANGE) is treated as absent rather than truncated.
        let mut buf = vec![0u8; 4096];
        // SAFETY: both strings are NUL-terminated and `buf` is valid for `buf.len()` bytes.
        let n = unsafe {
            libc::getxattr(
                p.as_ptr(),
                NAME.as_ptr().cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
                0,
            )
        };
        if n < 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(buf)
    }

    pub fn write_open(file: &std::fs::File, v: &[u8]) {
        // SAFETY: the descriptor is open for the life of `file`; `v` is valid for its length.
        unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                NAME.as_ptr().cast(),
                v.as_ptr().cast(),
                v.len(),
                0,
                0,
            );
        }
    }

    pub fn write_path(path: &Path, v: &[u8]) {
        let Some(p) = cpath(path) else { return };
        // SAFETY: as above.
        unsafe {
            libc::setxattr(
                p.as_ptr(),
                NAME.as_ptr().cast(),
                v.as_ptr().cast(),
                v.len(),
                0,
                libc::XATTR_NOFOLLOW,
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::Path;
    pub fn read(_: &Path) -> Option<Vec<u8>> {
        None
    }
    pub fn write_open(_: &std::fs::File, _: &[u8]) {}
    pub fn write_path(_: &Path, _: &[u8]) {}
}
