//! The list of mounts to bring back after a reboot.
//!
//! A mount does not survive a restart. The folder and everything written into it does, but the
//! process serving the archive's own files does not, so until something re-mounts, a game's files
//! list at the right sizes and fail to open. This is the record of which ones to bring back.
//!
//! **Nothing is remembered unless asked for.** `--remember` is the whole opt-in. There is no
//! setting that turns auto-remount on for everything, and an empty list is the default, so a
//! machine that never asked for this restores nothing at boot.
//!
//! The file is plain text, one `key=value` per line with records separated by blank lines, because
//! it is something a person may well want to read or fix by hand after moving a drive:
//!
//! ```text
//! archive=D:\games\Some Game.cram
//! root=D:\games\Some Game
//! writable=true
//! ```
//!
//! Paths go in verbatim and nothing is escaped. A line break cannot occur in a Windows path, while
//! a tab or an `=` can, which is why records are split on blank lines and fields on the first `=`.

use std::io::Write;
use std::path::{Path, PathBuf};

use cram_core::error::{ArchiveError, Result};

/// One remembered mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub archive: PathBuf,
    pub root: PathBuf,
    pub writable: bool,
}

/// Where the list lives: the per-user config directory, beside the hardware profile, so someone who
/// knows where one of them is can find the other.
pub fn registry_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| Path::new(&a).join("cram").join("mounts.txt"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
            .map(|base| base.join("cram").join("mounts.txt"))
    }
}

/// Every remembered mount, or an empty list when there is no file yet.
pub fn load() -> Vec<Entry> {
    let Some(path) = registry_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(_) => Vec::new(),
    }
}

/// Split out from [`load`] so the format is testable without a config directory to write into.
///
/// A malformed record is skipped rather than failing the whole read: this file can be hand-edited,
/// and one bad line should cost that line, not every other mount the user expected back.
fn parse(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let (mut archive, mut root, mut writable) = (None::<String>, None::<String>, false);
    let finish = |archive: &mut Option<String>,
                  root: &mut Option<String>,
                  writable: &mut bool,
                  out: &mut Vec<Entry>| {
        if let (Some(a), Some(r)) = (archive.take(), root.take()) {
            out.push(Entry {
                archive: PathBuf::from(a),
                root: PathBuf::from(r),
                writable: *writable,
            });
        }
        *writable = false;
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            finish(&mut archive, &mut root, &mut writable, &mut out);
            continue;
        }
        if line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "archive" => archive = Some(value.trim().to_string()),
            "root" => root = Some(value.trim().to_string()),
            "writable" => writable = value.trim() == "true",
            _ => {}
        }
    }
    // A file that does not end in a blank line still has a last record.
    finish(&mut archive, &mut root, &mut writable, &mut out);
    out
}

fn save(entries: &[Entry]) -> Result<()> {
    let path = registry_path().ok_or_else(|| {
        ArchiveError::Backend("no per-user config directory to remember mounts in".into())
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write beside, then rename: an interrupted write must not leave a half-file that loses every
    // remembered mount.
    let tmp = path.with_extension("txt.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        writeln!(f, "# Mounts Cram brings back with `cram mount --restore`.")?;
        writeln!(
            f,
            "# Added by `--remember`, removed by `--forget`. Safe to edit by hand."
        )?;
        for e in entries {
            writeln!(f)?;
            writeln!(f, "archive={}", e.archive.display())?;
            writeln!(f, "root={}", e.root.display())?;
            writeln!(f, "writable={}", e.writable)?;
        }
        f.flush()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Add a mount, replacing any entry for the same root. The root is the identity: one folder can
/// only be serving one archive.
pub fn remember(archive: &Path, root: &Path, writable: bool) -> Result<()> {
    let archive = absolute(archive);
    let root = absolute(root);
    let mut entries = load();
    entries.retain(|e| e.root != root);
    entries.push(Entry {
        archive,
        root,
        writable,
    });
    save(&entries)
}

/// Drop the entry for `root`, reporting whether there was one. Never touches the folder itself: a
/// writable mount's contents are the user's to keep or delete.
pub fn forget(root: &Path) -> Result<bool> {
    let root = absolute(root);
    let mut entries = load();
    let before = entries.len();
    entries.retain(|e| e.root != root);
    let removed = entries.len() != before;
    if removed {
        save(&entries)?;
    }
    Ok(removed)
}

/// Absolute where possible, because this list is read at boot from a working directory that has
/// nothing to do with the one the mount was set up in.
fn absolute(p: &Path) -> PathBuf {
    std::fs::canonicalize(p)
        .map(|c| {
            // Drop the verbatim prefix `canonicalize` adds on Windows. It is correct, and it is
            // also what the user reads in this file, where it makes every path look wrong.
            let s = c.to_string_lossy().to_string();
            PathBuf::from(s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s))
        })
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|d| d.join(p))
                .unwrap_or_else(|_| p.to_path_buf())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Forward slashes throughout. The parser never looks at a separator, and a test buried in
    // escaped backslashes is a test nobody can read.

    #[test]
    fn parses_records_and_skips_junk() {
        let text = concat!(
            "# a comment\n",
            "\n",
            "archive=/a/one.cram\n",
            "root=/a/one\n",
            "writable=true\n",
            "\n",
            "archive=/b/two.cram\n",
            "root=/b/two\n",
            "writable=false\n",
            "\n",
            "nonsense-with-no-equals\n",
        );
        let got = parse(text);
        assert_eq!(
            got.len(),
            2,
            "two complete records; the junk line is ignored"
        );
        assert!(got[0].writable, "the first record is writable");
        assert!(
            !got[1].writable,
            "writable has to reset between records, or one writable mount silently makes every \
             later one writable too"
        );
        assert_eq!(got[1].archive, PathBuf::from("/b/two.cram"));
    }

    #[test]
    fn a_trailing_record_without_a_blank_line_still_counts() {
        // Hand-edited files rarely end in a blank line, and losing the last mount to a missing
        // newline would be an infuriating way to lose a mount.
        let got = parse("archive=/a.cram\nroot=/a\nwritable=true");
        assert_eq!(got.len(), 1);
        assert!(got[0].writable);
    }

    #[test]
    fn an_incomplete_record_is_dropped_rather_than_guessed() {
        assert!(parse("archive=/only-this.cram\n").is_empty());
        assert!(parse("root=/only-this\n").is_empty());
    }
}
