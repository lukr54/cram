//! Unified entry/metadata model shared by every backend. The one place the zip-slip guard
//! lives is [`EntryPath::from_raw`] — every backend must funnel entry names through it, so no
//! backend can accidentally write outside the output directory.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink(String),
    Other,
}

/// A normalized, path-traversal-safe entry name. Holds the original `raw` name (for display)
/// and a `safe` relative path guaranteed to stay under any base it's joined to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryPath {
    raw: String,
    safe: PathBuf,
}

/// Does this single path component resolve to a Win32 DOS device? Win32 name parsing ignores
/// trailing spaces/dots and matches the name *before* the first `.`, case-insensitively — so
/// `nul`, `NUL.txt`, and `COM1 ` all name devices. Used to mangle such names on extraction.
fn is_reserved_dos_name(comp: &str) -> bool {
    let trimmed = comp.trim_end_matches([' ', '.']);
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    // COM0-9 / LPT0-9, plus the superscript COM¹/COM²/COM³ forms newer Windows also reserves.
    let rest = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"));
    matches!(
        rest,
        Some(
            "0" | "1"
                | "2"
                | "3"
                | "4"
                | "5"
                | "6"
                | "7"
                | "8"
                | "9"
                | "\u{00B9}"
                | "\u{00B2}"
                | "\u{00B3}"
        )
    )
}

/// Reject absurdly deep entry paths. A hostile archive could otherwise carry a single name with
/// hundreds of thousands of `/`-components and drive pathological per-component work (deep recursion
/// or filesystem traversal) in a consumer. Real archives never approach this.
const MAX_PATH_DEPTH: usize = 4096;

impl EntryPath {
    /// Normalize an archive entry name and reject anything that could escape the output dir:
    /// `..` components, absolute paths, drive letters / ADS (`:`), and NUL. Returns `None` for
    /// unsafe names (the caller turns that into `ArchiveError::UnsafePath`). Also rejects paths
    /// deeper than `MAX_PATH_DEPTH` components (hostile-archive DoS guard).
    ///
    /// Windows reserved device names (`NUL`, `CON`, `COM1`, …) are *mangled* (prefixed with `_`),
    /// not rejected: a Unix-authored archive may legitimately contain a file named `NUL`, and
    /// `File::create("…\\NUL")` opens the null device — the bytes vanish while the extractor
    /// reports success. Mangling extracts the file under a safe name instead of losing it.
    pub fn from_raw(raw: &str) -> Option<Self> {
        let mut safe = PathBuf::new();
        let mut depth = 0usize;
        for comp in raw.replace('\\', "/").split('/') {
            let c = match comp {
                "" | "." => continue,
                ".." => return None,
                c if c.contains(':') || c.contains('\0') => return None,
                c => c,
            };
            depth += 1;
            if depth > MAX_PATH_DEPTH {
                return None; // pathologically deep path — reject rather than process it
            }
            if is_reserved_dos_name(c) {
                safe.push(format!("_{c}"));
            } else {
                safe.push(c);
            }
        }
        if safe.as_os_str().is_empty() {
            return None;
        }
        Some(Self {
            raw: raw.to_string(),
            safe,
        })
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }
    pub fn safe(&self) -> &Path {
        &self.safe
    }
    /// The absolute output path for this entry under `base` (guaranteed to stay under `base`).
    pub fn join_under(&self, base: &Path) -> PathBuf {
        base.join(&self.safe)
    }
}

/// One archive member (metadata only — no content).
#[derive(Clone, Debug)]
pub struct Entry {
    pub index: usize,
    pub path: EntryPath,
    pub kind: EntryKind,
    /// Uncompressed size in bytes.
    pub size: u64,
    pub compressed_size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub unix_mode: Option<u32>,
    pub crc32: Option<u32>,
    pub encrypted: bool,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }
    pub fn name(&self) -> &str {
        self.path.raw()
    }
}

/// Total uncompressed bytes and file count (directories excluded) — sizes the progress bar.
pub fn totals(entries: &[Entry]) -> (u64, u64) {
    let mut bytes: u64 = 0;
    let mut files = 0;
    for e in entries {
        if !e.is_dir() {
            // Declared sizes are untrusted (ZIP64 allows u64::MAX per entry) — saturate, don't
            // wrap/panic.
            bytes = bytes.saturating_add(e.size);
            files += 1;
        }
    }
    (bytes, files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute() {
        assert!(EntryPath::from_raw("../etc/passwd").is_none());
        assert!(EntryPath::from_raw("a/../../b").is_none());
        assert!(EntryPath::from_raw("C:/Windows/system32").is_none()); // drive-letter ':'
        assert!(EntryPath::from_raw("foo\0bar").is_none());
        assert!(EntryPath::from_raw("").is_none());
        assert!(EntryPath::from_raw("/").is_none());
    }

    #[test]
    fn accepts_and_normalizes_safe_names() {
        let p = EntryPath::from_raw("dir\\sub/file.txt").unwrap();
        assert_eq!(p.safe(), Path::new("dir/sub/file.txt"));
        // leading slash is stripped → stays relative/under base
        let p2 = EntryPath::from_raw("/etc/hosts").unwrap();
        assert_eq!(p2.safe(), Path::new("etc/hosts"));
        assert!(p2.join_under(Path::new("out")).starts_with("out"));
    }

    #[test]
    fn rejects_pathologically_deep_paths() {
        // Hostile-archive DoS guard: a name with more than MAX_PATH_DEPTH components is rejected
        // outright, so no consumer (e.g. the ProjFS mount's ancestor walk) can be driven into
        // pathological per-component work by a crafted deep path.
        let too_deep = vec!["a"; MAX_PATH_DEPTH + 1].join("/");
        assert!(EntryPath::from_raw(&too_deep).is_none());
        // A path exactly at the limit is still accepted.
        let at_limit = vec!["a"; MAX_PATH_DEPTH].join("/");
        assert!(EntryPath::from_raw(&at_limit).is_some());
    }

    #[test]
    fn mangles_reserved_device_names() {
        // Bare device name → mangled to a safe on-disk name (file kept, not silently lost).
        assert_eq!(
            EntryPath::from_raw("NUL").unwrap().safe(),
            Path::new("_NUL")
        );
        assert_eq!(
            EntryPath::from_raw("nul").unwrap().safe(),
            Path::new("_nul")
        );
        // Device name with an extension (Win32 matches the stem before the first '.').
        assert_eq!(
            EntryPath::from_raw("CON.txt").unwrap().safe(),
            Path::new("_CON.txt")
        );
        assert_eq!(
            EntryPath::from_raw("com1").unwrap().safe(),
            Path::new("_com1")
        );
        // Reserved even as a directory component.
        assert_eq!(
            EntryPath::from_raw("a/NUL/b.txt").unwrap().safe(),
            Path::new("a/_NUL/b.txt")
        );
        // Non-devices are untouched (substring / wrong-arity must not trigger).
        assert_eq!(
            EntryPath::from_raw("NULL.txt").unwrap().safe(),
            Path::new("NULL.txt")
        );
        assert_eq!(
            EntryPath::from_raw("COM10").unwrap().safe(),
            Path::new("COM10")
        );
        assert_eq!(
            EntryPath::from_raw("console.log").unwrap().safe(),
            Path::new("console.log")
        );
    }
}
