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
}

impl CreatedLog {
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
        std::fs::create_dir_all(p)?;
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
    pub fn create_file(&self, p: &Path) -> std::io::Result<std::fs::File> {
        match std::fs::File::create_new(p) {
            Ok(f) => {
                if let Ok(mut v) = self.files.lock() {
                    v.push(p.to_path_buf());
                }
                Ok(f)
            }
            // It was already there, so it is the user's and not ours to unwind — overwrite it
            // without recording it, which is exactly what `note_file` decided by omission.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => std::fs::File::create(p),
            Err(e) => Err(e),
        }
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
}
