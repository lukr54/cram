//! Undoing a cancelled extraction, without touching anything that was already there.
//!
//! An extract writes straight into the destination, so "remove what this run wrote" is not the same
//! question as "remove what is in the folder". Extracting into a folder that already has files in it
//! is ordinary, and deleting the difference would destroy the user's data.
//!
//! So a path counts as this run's only if **it did not exist when we were about to create it**. An
//! entry that overwrote a file the user already had is deliberately left alone: their original is
//! gone the moment it is overwritten, and deleting the partial replacement on top would leave them
//! with neither. A half-written file they can see and delete is a better outcome than an empty space
//! where their file used to be.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Everything this extraction brought into existence, so a cancel can remove exactly that.
///
/// Both engine paths share one of these, hence the locks; they are taken once per created entry,
/// which is nothing next to the write that follows.
#[derive(Default)]
pub struct CreatedLog {
    files: Mutex<Vec<PathBuf>>,
    dirs: Mutex<Vec<PathBuf>>,
    /// Parents already made, so the second file in a directory does not pay to make it again. See
    /// [`CreatedLog::ensure_dir`].
    ensured: Mutex<HashSet<PathBuf>>,
    /// The archive's download quarantine, copied onto everything created here. See
    /// [`super::quarantine`]. Unset everywhere but macOS, and there unless the archive has one.
    mark: super::quarantine::Mark,
}

impl CreatedLog {
    /// A log that also stamps `mark` onto every file it opens and every directory it makes. This is
    /// where both engine paths create everything they write, which is why the mark lives here.
    pub fn marking(mark: super::quarantine::Mark) -> Self {
        CreatedLog {
            mark,
            ..Default::default()
        }
    }

    /// Make an entry's parent directory, at most once per directory per run.
    ///
    /// Both engine paths used to call `note_dir` + `fs::create_dir_all` for **every file**, and
    /// neither is cheap when the answer is "it is already there": `note_dir` stats the path, and
    /// `create_dir_all` issues a `mkdir` that fails `EEXIST` and then stats it to confirm it is a
    /// directory. Three syscalls per file to learn nothing.
    ///
    /// Measured extracting the kernel tree (100,992 members in 6,214 directories): **100,994 `mkdir`
    /// calls of which 94,779 failed**, against GNU tar's 6,214. Remembering which parents exist
    /// makes the count the number of directories rather than the number of files.
    pub fn ensure_dir(&self, p: &Path) -> std::io::Result<()> {
        if let Ok(seen) = self.ensured.lock() {
            if seen.contains(p) {
                return Ok(());
            }
        }
        self.note_dir(p);
        // Every level `create_dir_all` is about to make, deepest first, so each can be marked as
        // `unzip` marks it. Found by walking up to the first level that exists, which is the
        // destination at the latest, since both engines make it before anything else. Only paid
        // when there is a mark to apply.
        let mut new_levels = Vec::new();
        if self.mark.is_set() {
            let mut q = Some(p);
            while let Some(d) = q.filter(|d| !d.exists()) {
                new_levels.push(d.to_path_buf());
                q = d.parent();
            }
        }
        std::fs::create_dir_all(p)?;
        for d in &new_levels {
            self.mark.apply_path(d);
        }
        if let Ok(mut seen) = self.ensured.lock() {
            seen.insert(p.to_path_buf());
        }
        Ok(())
    }
    /// Record a file as ours, if and only if nothing is there yet. Call this *before* creating it.
    pub fn note_file(&self, p: &Path) {
        if !p.exists() {
            if let Ok(mut v) = self.files.lock() {
                v.push(p.to_path_buf());
            }
        }
    }

    /// Create an entry's file, recording it as ours only if nothing was there.
    ///
    /// [`note_file`](Self::note_file) followed by `File::create` asks the filesystem the same
    /// question twice: a `statx` to find out whether the path exists, then an `openat` that would
    /// have said so. In a normal extraction the `statx` fails every time — 100,997 of 101,060 on the
    /// kernel tree. `create_new` gets the answer from the `openat` we were going to issue anyway,
    /// and as a bonus there is no longer a gap between the check and the create for anything else to
    /// slip into.
    ///
    /// Either way the file gets the archive's mark, if any: an overwritten file now holds the
    /// archive's content just as much as a new one does.
    pub fn create_file(&self, p: &Path) -> std::io::Result<std::fs::File> {
        let f = match std::fs::File::create_new(p) {
            Ok(f) => {
                if let Ok(mut v) = self.files.lock() {
                    v.push(p.to_path_buf());
                }
                f
            }
            // It was already there, so it is the user's and not ours to unwind — overwrite it
            // without recording it, which is exactly what `note_file` decided by omission. Only if
            // it is a regular file: `File::create` here followed a link planted at the path and
            // truncated whatever it named. See `overwrite`.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                super::overwrite::open_existing(p)?
            }
            Err(e) => return Err(e),
        };
        self.mark.apply_open(&f);
        Ok(f)
    }

    /// Same, for a directory.
    pub fn note_dir(&self, p: &Path) {
        if !p.exists() {
            if let Ok(mut v) = self.dirs.lock() {
                v.push(p.to_path_buf());
            }
        }
    }

    /// Remove what this run created: files first, then the directories that are left empty.
    ///
    /// `remove_dir` refuses a directory that still has anything in it, which is exactly the
    /// behaviour wanted here — a directory that also holds something the user already had survives,
    /// without needing to work out which case it is. Deepest paths first so children are gone before
    /// their parents are tried. Every failure is ignored: this runs while unwinding an operation the
    /// user already stopped, and turning a cleanup failure into an error would replace a tidy-up
    /// problem with a reported one.
    pub fn unwind(&self) {
        if let Ok(files) = self.files.lock() {
            for f in files.iter() {
                let _ = std::fs::remove_file(f);
            }
        }
        let mut dirs: Vec<PathBuf> = self
            .dirs
            .lock()
            .map(|d| d.iter().cloned().collect())
            .unwrap_or_default();
        // Ancestors too: `create_dir_all` makes every missing level, and only the deepest was noted.
        if let Ok(files) = self.files.lock() {
            for f in files.iter() {
                let mut p = f.parent();
                while let Some(d) = p {
                    dirs.push(d.to_path_buf());
                    p = d.parent();
                }
            }
        }
        dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        dirs.dedup();
        for d in dirs {
            let _ = std::fs::remove_dir(&d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cram-unwind-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The whole point: a cancelled extraction takes back what it wrote and leaves everything else,
    /// including a file it overwrote. Deleting that one would leave the user with nothing at all
    /// where their file used to be, which is worse than the partial they can see.
    #[test]
    fn unwind_removes_only_what_this_run_created() {
        let dir = scratch("only-ours");
        let theirs = dir.join("theirs.txt");
        std::fs::write(&theirs, b"the user's own file").unwrap();

        let log = CreatedLog::default();

        // Ours: did not exist when noted.
        let ours = dir.join("sub/ours.bin");
        log.note_dir(&dir.join("sub"));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        log.note_file(&ours);
        std::fs::write(&ours, b"half an entry").unwrap();

        // Theirs: already there when noted, so it is never recorded, even though this run wrote it.
        log.note_file(&theirs);
        std::fs::write(&theirs, b"overwritten by the cancelled run").unwrap();

        log.unwind();

        assert!(!ours.exists(), "a file this run created is removed");
        assert!(
            !dir.join("sub").exists(),
            "a directory this run created, left empty, is removed"
        );
        assert!(
            theirs.exists(),
            "a file that was already there is never this run's to delete"
        );
        assert!(dir.exists(), "the destination itself is not touched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory that still holds something the user had must survive, even though this run also
    /// put a file in it. `remove_dir` gives that for free by refusing a non-empty directory.
    #[test]
    fn a_directory_still_holding_their_file_survives() {
        let dir = scratch("shared-dir");
        let shared = dir.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("theirs.txt"), b"pre-existing").unwrap();

        let log = CreatedLog::default();
        let ours = shared.join("ours.bin");
        log.note_file(&ours);
        std::fs::write(&ours, b"partial").unwrap();

        log.unwind();

        assert!(!ours.exists(), "our file goes");
        assert!(shared.exists(), "their directory stays");
        assert!(shared.join("theirs.txt").exists(), "their file stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every level `ensure_dir` makes gets the mark, not only the deepest, and a level that was
    /// already there does not. Entries whose parents are not listed in the archive take this path.
    #[cfg(target_os = "macos")]
    #[test]
    fn ensure_dir_marks_each_level_it_makes_and_no_other() {
        use std::process::Command;
        let q = "0083;6745f2a0;Safari;3F2504E0-4F89-11D3-9A0C-0305E82C3301";
        let mark_of = |p: &Path| {
            let o = Command::new("xattr")
                .args(["-p", "com.apple.quarantine"])
                .arg(p)
                .output()
                .unwrap();
            o.status
                .success()
                .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        };
        let dir = scratch("mark-levels");
        let src = dir.join("archive");
        std::fs::write(&src, b"x").unwrap();
        let ok = Command::new("xattr")
            .args(["-w", "com.apple.quarantine", q])
            .arg(&src)
            .status()
            .unwrap();
        assert!(ok.success());
        std::fs::create_dir_all(dir.join("out/theirs")).unwrap();

        let log = CreatedLog::marking(super::super::quarantine::Mark::of(&src));
        log.ensure_dir(&dir.join("out/theirs/a/b/c")).unwrap();
        let f = log.create_file(&dir.join("out/theirs/a/b/c/f")).unwrap();
        drop(f);

        assert_eq!(mark_of(&dir.join("out/theirs")), None, "already there");
        for p in ["a", "a/b", "a/b/c", "a/b/c/f"] {
            let p = dir.join("out/theirs").join(p);
            assert_eq!(mark_of(&p).as_deref(), Some(q), "{}", p.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
