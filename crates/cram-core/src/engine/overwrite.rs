//! Opening a destination that is already there, without writing through it.
//!
//! `File::create` on an existing path follows a symbolic link and truncates whatever it points at.
//! An extraction that met `out/target.txt -> /somewhere/else` therefore wrote the archive's bytes
//! into a file outside the destination (reported against 1.3.1). Overwriting a regular file that is
//! already there is ordinary and stays the behaviour; what changes is that the open now refuses
//! anything at the final component that is not a regular file: a link, a FIFO, a device.
//!
//! The check is made on the handle, not on the path. A `symlink_metadata` before the open would
//! leave a window for the link to be planted between the two; opening with "do not follow" and
//! then asking the open descriptor what it is leaves none. Nothing is truncated until that answer
//! is in.
//!
//! **Not covered, on purpose:** a directory link *above* the final component (`out/sub -> /elsewhere`
//! with an entry `sub/x`) is followed. Cram never creates a link from an archive (no backend yields
//! one, and `.cram` cannot encode one), so a link inside the destination is one the user or another
//! program put there, and following a directory the user arranged is what `tar` does too. Hard links
//! are also outside this: a second name for a regular file is a regular file, and refusing it would
//! mean refusing every overwrite on a filesystem where the user has made one.

use std::fs::File;
use std::io;
use std::path::Path;

/// Create `p` for writing: a new file if nothing is there, the existing one truncated if it is a
/// regular file, and an error naming the cause if it is anything else.
///
/// For callers that do not need to know which of the first two happened. [`CreatedLog::create_file`]
/// does, and calls [`open_existing`] itself.
///
/// [`CreatedLog::create_file`]: super::unwind::CreatedLog::create_file
pub fn create(p: &Path) -> io::Result<File> {
    match File::create_new(p) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => open_existing(p),
        Err(e) => Err(e),
    }
}

/// Open a path that already exists for overwriting, refusing anything that is not a regular file.
/// `create_new` has already been tried and said `AlreadyExists`; it does not follow links either
/// (`O_CREAT | O_EXCL` fails on one), so this is the only arm that needs the guard.
pub fn open_existing(p: &Path) -> io::Result<File> {
    let f = imp::open_regular(p).map_err(|e| refusal(p, e))?;
    f.set_len(0)?;
    Ok(f)
}

/// Word the error by what is actually at the path, so the report says "symbolic link" rather than
/// an errno that differs per platform (`ELOOP` on Linux and macOS, `EMLINK` on FreeBSD).
fn refusal(p: &Path, cause: io::Error) -> io::Error {
    let what = match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => "a symbolic link",
        Ok(m) if m.is_dir() => "a directory",
        Ok(m) if !m.is_file() => "not a regular file",
        _ => return cause,
    };
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "refusing to write through {}: {what} is already there ({cause})",
            p.display()
        ),
    )
}

fn not_regular() -> io::Error {
    io::Error::other("not a regular file")
}

#[cfg(unix)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// `O_NOFOLLOW` makes the open fail on a link at the final component. `O_NONBLOCK` makes it
    /// fail on a FIFO with no reader instead of blocking the extraction forever, which is what a
    /// plain `File::create` did; it is cleared again once the descriptor is known to be a file.
    pub fn open_regular(p: &Path) -> io::Result<File> {
        let f = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(p)?;
        if !f.metadata()?.is_file() {
            return Err(super::not_regular());
        }
        let fd = f.as_raw_fd();
        // SAFETY: `fd` is a valid descriptor owned by `f` for the duration of both calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(f)
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    /// Open the reparse point itself rather than what it names.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

    /// `is_symlink` is true for name-surrogate reparse points only (symbolic links and junctions).
    /// A OneDrive placeholder or a deduplicated file is a reparse point too but not a link, and it
    /// is opened a second time without the flag so its filter driver sees an ordinary write. The
    /// gap between the two opens only matters if the file is swapped for a link inside it, which
    /// needs the same rights over the destination as planting the link in the first place.
    pub fn open_regular(p: &Path) -> io::Result<File> {
        let f = OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)?;
        let meta = f.metadata()?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(super::not_regular());
        }
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return Ok(f);
        }
        drop(f);
        OpenOptions::new().write(true).open(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cram-overwrite-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn file_link(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    /// Needs Developer Mode or elevation on Windows. Without either the test has nothing to check,
    /// and says so rather than passing on a link that was never made.
    #[cfg(windows)]
    fn file_link(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("skipped: cannot create a symlink here ({e})");
                false
            }
        }
    }

    #[test]
    fn a_link_is_refused_and_its_target_left_alone() {
        let d = scratch("link");
        let marker = d.join("marker.txt");
        std::fs::write(&marker, b"ORIGINAL_CONTENT").unwrap();
        let link = d.join("target.txt");
        if !file_link(&marker, &link) {
            return;
        }
        let err = create(&link).expect_err("a link must be refused");
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert_eq!(std::fs::read(&marker).unwrap(), b"ORIGINAL_CONTENT");
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());

        // Dangling: `create_new` must not create the target through it either.
        let dangling = d.join("dangling.txt");
        assert!(file_link(&d.join("nowhere"), &dangling));
        assert!(create(&dangling).is_err());
        assert!(!d.join("nowhere").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_regular_file_is_overwritten_and_truncated() {
        let d = scratch("plain");
        let plain = d.join("plain.txt");
        std::fs::write(&plain, b"old and longer").unwrap();
        create(&plain).unwrap().write_all(b"new").unwrap();
        assert_eq!(std::fs::read(&plain).unwrap(), b"new");
        let fresh = d.join("fresh.txt");
        create(&fresh).unwrap().write_all(b"x").unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A plain `File::create` blocked here forever, waiting for a reader.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_rather_than_waited_on() {
        let d = scratch("fifo");
        let fifo = d.join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(create(&fifo).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    /// No way to open without following here, so check the path first and accept the window.
    pub fn open_regular(p: &Path) -> io::Result<File> {
        if !std::fs::symlink_metadata(p)?.is_file() {
            return Err(super::not_regular());
        }
        OpenOptions::new().write(true).open(p)
    }
}
