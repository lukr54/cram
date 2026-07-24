//! Cram archive **mount** via Windows Projected File System (ProjFS): present a `.cram` (or any
//! Cram-readable archive that offers random access) as a **virtual folder**, browse the tree and
//! open files on demand, with content materialized lazily via `RandomAccessReader::read_range`, no
//! up-front extraction.
//!
//! Read-only. The five ProjFS callbacks map to the archive: directory enumeration comes from the
//! entry list; placeholder metadata (size/dir-flag) from the entry; file data from `read_range`.
//! ProjFS invokes callbacks on its own threads, so the reader is shared `&` (it is `Send + Sync`)
//! and the active-enumeration map is behind a `Mutex`.
//!
//! Uses the MIT/Apache `windows` crate's ProjFS bindings (no GPL `windows-projfs`); the binding
//! links on the mingw toolchain. Non-Windows targets get a stub `mount` that errors.

// The directory model below is consumed by the Windows ProjFS provider and by the unit tests; on a
// non-Windows, non-test build it is intentionally absent, so its imports are gated the same way.
#[cfg(any(windows, test))]
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cram_core::error::Result;
#[cfg(any(windows, test))]
use cram_core::reader::RandomAccessReader;
use cram_core::secret::PasswordProvider;
use std::sync::Arc;

#[cfg(windows)]
mod projfs;
#[cfg(windows)]
mod projfs_api;

pub mod cli;

/// Whether this machine can mount at all, i.e. whether the optional Windows feature `Client-ProjFS`
/// is enabled. Callers that show a mount affordance should check this first and explain
/// [`UNAVAILABLE_HINT`] instead of offering an action that cannot work.
#[cfg(windows)]
pub fn available() -> bool {
    projfs_api::available()
}

#[cfg(not(windows))]
pub fn available() -> bool {
    false
}

/// User-facing explanation of how to turn ProjFS on, for the [`available`] == false case.
#[cfg(windows)]
pub const UNAVAILABLE_HINT: &str = projfs_api::UNAVAILABLE;

#[cfg(not(windows))]
pub const UNAVAILABLE_HINT: &str = "archive mount requires Windows (ProjFS)";

/// A live mount. Dropping it stops the virtualization and releases the archive. Keep it alive (e.g.
/// block the thread) for as long as the folder should stay browsable.
pub struct Mount {
    // Drop guard: holding it keeps virtualization running; dropping it unmounts. Never read directly.
    #[cfg(windows)]
    #[allow(dead_code)]
    inner: projfs::MountInner,
    root: PathBuf,
}

impl Mount {
    /// The virtualization root (the folder the archive appears under).
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Mount `archive` (any random-access format, `.cram` or ZIP) as a virtual folder at `root`. The
/// format is sniffed from the file; a sequential-only container (tar/7z/rar/raw) is rejected with
/// `ArchiveError::UnsupportedFormat`. `root` is created if absent and must be empty. Returns a
/// [`Mount`] guard; the folder stays live until it is dropped.
#[cfg(windows)]
pub fn mount(archive: &Path, root: &Path, pw: Arc<dyn PasswordProvider>) -> Result<Mount> {
    // Sniff → dispatch to whichever backend offers random access (ZIP or `.cram`). The mount only
    // ever touches the `RandomAccessReader` seam, so it is agnostic to which concrete reader backs it.
    let fmt = cram_core::sniff::sniff_path(archive)?;
    let reader = cram_core::formats::open_random_access(archive, fmt, pw)?;
    let inner = projfs::MountInner::start(reader, root)?;
    Ok(Mount {
        inner,
        root: root.to_path_buf(),
    })
}

#[cfg(not(windows))]
pub fn mount(_archive: &Path, _root: &Path, _pw: Arc<dyn PasswordProvider>) -> Result<Mount> {
    Err(cram_core::error::ArchiveError::Backend(
        "archive mount requires Windows (ProjFS)".into(),
    ))
}

/// Build the browsable directory model from a random-access reader's entry list: a per-directory
/// child list (for enumeration) and a path→entry lookup (for placeholder info + file data). Shared
/// by the real mount and unit tests. Paths use forward slashes, no leading/trailing slash.
#[cfg(any(windows, test))]
pub(crate) struct DirModel {
    /// dir path (e.g. "" for root, "src", "src/sub") → its immediate children.
    pub tree: HashMap<String, Vec<Child>>,
    /// entry path → (is_dir, size, index into the reader's entries).
    pub lookup: HashMap<String, EntryInfo>,
}

#[cfg(any(windows, test))]
#[derive(Clone)]
pub(crate) struct Child {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy)]
pub(crate) struct EntryInfo {
    pub is_dir: bool,
    pub size: u64,
    pub index: usize,
}

/// Case-fold a path (segment or full) to the canonical key used for **every** map lookup. Windows
/// is case-insensitive, so `SRC` and `src` must resolve to the same directory node; we key `tree`
/// and `lookup` by the folded form while `Child.name` keeps the original case for display. Folding
/// consistently is what makes case-variant ancestor dirs (`src/a` + `SRC/b`) merge into one node
/// holding both files instead of orphaning one, the mount serves ProjFS's enumerated names, which
/// we fold the same way on the way back in (`path_of`), so both sides always agree.
#[cfg(any(windows, test))]
pub(crate) fn fold(s: &str) -> String {
    s.to_lowercase()
}

#[cfg(any(windows, test))]
impl DirModel {
    pub(crate) fn build(reader: &dyn RandomAccessReader) -> Self {
        let mut tree: HashMap<String, Vec<Child>> = HashMap::new();
        let mut lookup: HashMap<String, EntryInfo> = HashMap::new();
        tree.entry(String::new()).or_default(); // root always exists

        for (index, e) in reader.entries().iter().enumerate() {
            // Project the SANITIZED path, not the raw archive name: `safe()` carries the reserved-
            // device mangling (`NUL` → `_NUL`) that extraction applies. Serving the raw name would
            // let a mounted entry called `NUL`/`CON` bind the Win32 device, opens hit the null/
            // console device before ever reaching ProjFS, so the entry's real content is unreachable
            // and a copy silently produces an empty file.
            let path = e.path.safe().to_string_lossy().replace('\\', "/");
            let path = path.trim_matches('/').to_string();
            if path.is_empty() {
                continue;
            }
            let is_dir = e.is_dir();
            let size = e.size;
            // FIRST-wins, to stay consistent with the tree: `add_child` below dedups a folded name
            // keeping the first occurrence, so enumeration shows the first entry's name+size. If the
            // lookup were last-wins (plain `insert`), a case-variant collision (`File.txt` +
            // `file.txt`) or a duplicate name would enumerate the first entry but serve the LAST
            // entry's size and bytes, a name/content mismatch. `or_insert` makes placeholder info
            // and file data resolve to the SAME entry the directory enumerated; the shadowed
            // duplicate is simply not independently addressable through the (case-insensitive) mount.
            lookup.entry(fold(&path)).or_insert(EntryInfo {
                is_dir,
                size,
                index,
            });

            // Register this entry as a child of its parent, synthesizing any missing ancestor dirs
            // (so a mount works even if intermediate directory entries were not stored explicitly).
            let (parent, leaf) = match path.rsplit_once('/') {
                Some((p, l)) => (p.to_string(), l.to_string()),
                None => (String::new(), path.clone()),
            };
            add_child(&mut tree, &parent, &leaf, is_dir, size);
            ensure_ancestors(&mut tree, &mut lookup, &parent);
        }

        // Reconcile file/dir path collisions deterministically: any path that has children is a
        // directory, full stop. A malformed archive can hold both a file `foo` and entries under
        // `foo/…`; without this, the outcome would depend on entry order (file-with-orphaned-children
        // vs directory-that-lookup-calls-a-file), and enumeration (`Child.is_dir`) could disagree with
        // placeholder info (`lookup`). Here the directory always wins, its children stay reachable,
        // the colliding file is shadowed, and both maps are forced to agree, independent of order.
        let dir_keys: Vec<String> = tree.keys().filter(|k| !k.is_empty()).cloned().collect();
        for d in &dir_keys {
            lookup.insert(
                d.clone(),
                EntryInfo {
                    is_dir: true,
                    size: 0,
                    index: usize::MAX,
                },
            );
            // Fix this dir's entry in its parent's child list (keys are folded; `leaf` is too).
            let (parent, leaf) = match d.rsplit_once('/') {
                Some((p, l)) => (p, l),
                None => ("", d.as_str()),
            };
            if let Some(children) = tree.get_mut(parent) {
                for c in children.iter_mut().filter(|c| fold(&c.name) == leaf) {
                    c.is_dir = true;
                    c.size = 0;
                }
            }
        }

        // Sort each directory's children (case-insensitive) for stable enumeration.
        for children in tree.values_mut() {
            children.sort_by_key(|c| fold(&c.name));
            children.dedup_by(|a, b| fold(&a.name) == fold(&b.name));
        }
        DirModel { tree, lookup }
    }
}

#[cfg(any(windows, test))]
fn add_child(
    tree: &mut HashMap<String, Vec<Child>>,
    parent: &str,
    leaf: &str,
    is_dir: bool,
    size: u64,
) {
    // Key by the folded parent so case-variant parents (`src` / `SRC`) land in the SAME node,
    // merging their children rather than splitting into two keys the mount can't both reach.
    let entry = tree.entry(fold(parent)).or_default();
    if !entry.iter().any(|c| fold(&c.name) == fold(leaf)) {
        entry.push(Child {
            name: leaf.to_string(),
            is_dir,
            size,
        });
    }
}

/// Ensure every ancestor directory of `dir` exists as a tree node and as a child of its own parent.
/// **Iterative**, not recursive: it walks from `dir` up to the root one segment at a time, so a
/// pathologically deep entry path in a hostile archive can't overflow the stack (recursion here
/// would grow one frame per path component). Works on borrowed slices of `dir`, so no per-level
/// allocation either. (`EntryPath::from_raw` also caps path depth, but this stays safe regardless.)
#[cfg(any(windows, test))]
fn ensure_ancestors(
    tree: &mut HashMap<String, Vec<Child>>,
    lookup: &mut HashMap<String, EntryInfo>,
    dir: &str,
) {
    let mut cur = dir;
    while !cur.is_empty() {
        tree.entry(fold(cur)).or_default();
        lookup.entry(fold(cur)).or_insert(EntryInfo {
            is_dir: true,
            size: 0,
            index: usize::MAX, // synthesized dir; no backing entry
        });
        let (parent, leaf) = match cur.rsplit_once('/') {
            Some((p, l)) => (p, l),
            None => ("", cur),
        };
        add_child(tree, parent, leaf, true, 0);
        cur = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cram_core::engine;
    use cram_core::format::{Codec, Format};
    use cram_core::formats::cram::CramReader;
    use cram_core::model::{Entry, EntryKind, EntryPath};
    use cram_core::progress::NullSink;
    use cram_core::secret::NoPassword;
    use cram_core::writer::CreateOptions;
    use std::fs;

    /// A minimal `RandomAccessReader` over a fixed entry list, lets `DirModel::build` be tested
    /// against synthetic archives that a real filesystem can't produce (e.g. two directories whose
    /// names differ only in case). Only `entries()` is exercised by the model.
    struct MockReader {
        entries: Vec<Entry>,
    }
    impl RandomAccessReader for MockReader {
        fn entries(&self) -> &[Entry] {
            &self.entries
        }
        fn copy_entry(&self, _index: usize, _out: &mut dyn std::io::Write) -> Result<u64> {
            Ok(0)
        }
        fn read_range(&self, _index: usize, _off: u64, _len: u64) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }
    fn file_entry(index: usize, name: &str, size: u64) -> Entry {
        Entry {
            index,
            path: EntryPath::from_raw(name).unwrap(),
            kind: EntryKind::File,
            size,
            compressed_size: None,
            modified: None,
            unix_mode: None,
            crc32: None,
            encrypted: false,
        }
    }

    #[test]
    fn case_variant_ancestor_dirs_merge_without_data_loss() {
        // Two files whose only ancestor differs solely in case ("src" vs "SRC"). Windows is
        // case-insensitive, so they must merge into ONE folder holding BOTH files; neither may
        // vanish. Guards against: keying the tree by the unfolded name, which holds "src" and "SRC"
        // as two separate keys while the parent enumerates only the first, leaving the other file
        // orphaned.
        let reader = MockReader {
            entries: vec![file_entry(0, "src/a.txt", 1), file_entry(1, "SRC/b.txt", 2)],
        };
        let model = DirModel::build(&reader);

        // Root shows exactly one directory (the merged "src").
        let root_dirs: Vec<_> = model.tree[""].iter().filter(|c| c.is_dir).collect();
        assert_eq!(root_dirs.len(), 1, "case-variant dirs must collapse to one");

        // That single folder holds BOTH files.
        let src = &model.tree["src"];
        assert!(src.iter().any(|c| c.name == "a.txt"), "a.txt reachable");
        assert!(src.iter().any(|c| c.name == "b.txt"), "b.txt reachable");

        // Both files resolve for placeholder/read (lookup keyed by the folded path).
        assert!(model.lookup.contains_key("src/a.txt"));
        assert!(model.lookup.contains_key("src/b.txt"));
    }

    #[test]
    fn case_variant_files_serve_consistent_name_size_and_content() {
        // Two files whose full paths differ only in case ("File.txt" vs "file.txt"). Windows is
        // case-insensitive, so the mount exposes ONE file. Enumeration keeps the first entry
        // (first-wins dedup in the tree), so the lookup; which drives placeholder size and file
        // data, must resolve to the SAME first entry. Guards against: a last-wins lookup (a plain
        // `insert`), under which the directory shows "File.txt" @ 10 bytes while opening it serves
        // "file.txt"'s 20 bytes and reports size 20, a name/content/size mismatch. Both size and
        // the served index must be the first entry's.
        let reader = MockReader {
            entries: vec![file_entry(0, "File.txt", 10), file_entry(1, "file.txt", 20)],
        };
        let model = DirModel::build(&reader);

        // Root enumerates exactly one file, and it is the first entry's name + size.
        let root_files: Vec<_> = model.tree[""].iter().filter(|c| !c.is_dir).collect();
        assert_eq!(root_files.len(), 1, "case-variant files collapse to one");
        assert_eq!(root_files[0].name, "File.txt");
        assert_eq!(root_files[0].size, 10);

        // Placeholder + file data resolve to the SAME (first) entry: size 10, index 0.
        let info = model.lookup["file.txt"]; // folded key
        assert_eq!(
            info.size, 10,
            "served size matches the enumerated (first) entry"
        );
        assert_eq!(
            info.index, 0,
            "served content is the first entry, not the last"
        );
    }

    #[test]
    fn file_and_dir_at_same_path_resolve_to_directory_either_order() {
        // A malformed archive holding both a file `foo` and a child `foo/bar`. The path has
        // children, so it must be a directory (children reachable, colliding file shadowed); and
        // this must hold regardless of which entry comes first, with enumeration and lookup agreeing.
        for entries in [
            vec![file_entry(0, "foo", 5), file_entry(1, "foo/bar", 7)],
            vec![file_entry(0, "foo/bar", 7), file_entry(1, "foo", 5)],
        ] {
            let model = DirModel::build(&MockReader { entries });

            // Root shows "foo" as a directory.
            let foo = model.tree[""]
                .iter()
                .find(|c| fold(&c.name) == "foo")
                .expect("foo present at root");
            assert!(
                foo.is_dir,
                "a path with children must enumerate as a directory"
            );

            // lookup agrees it's a directory (is_dir + synthesized index), and "bar" is reachable.
            let info = model.lookup["foo"];
            assert!(info.is_dir, "placeholder info must agree it is a directory");
            assert_eq!(info.index, usize::MAX);
            assert!(
                model.tree["foo"].iter().any(|c| c.name == "bar"),
                "child reachable"
            );
        }
    }

    #[test]
    fn reserved_device_names_are_projected_mangled() {
        // An archive entry literally named `NUL` (legal in Unix-authored archives) must be served
        // under its mangled `_NUL` name, projecting the raw name binds the Win32 null device and
        // the content becomes unreachable through the mount.
        let reader = MockReader {
            entries: vec![file_entry(0, "NUL", 4), file_entry(1, "docs/CON.txt", 7)],
        };
        let model = DirModel::build(&reader);
        assert!(
            model.tree[""].iter().any(|c| c.name == "_NUL"),
            "NUL must enumerate as _NUL"
        );
        assert!(model.lookup.contains_key("_nul"), "folded mangled key");
        assert!(
            !model.lookup.contains_key("nul"),
            "raw device name must not be served"
        );
        assert!(model.tree["docs"].iter().any(|c| c.name == "_CON.txt"));
    }

    #[test]
    fn dir_model_builds_tree_and_lookup() {
        // Create a small .cram with a nested tree, then check the mount's directory model.
        let dir = std::env::temp_dir().join(format!("cram-mount-ut-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/sub")).unwrap();
        fs::write(dir.join("src/readme.txt"), b"hello readme").unwrap();
        fs::write(dir.join("src/sub/note.txt"), b"nested note").unwrap();
        let archive = dir.join("m.cram");
        engine::create::create(
            &archive,
            Format::cram(Codec::None),
            &[dir.join("src")],
            CreateOptions::default(),
            &NullSink,
        )
        .unwrap();

        let reader = CramReader::open(&archive, Arc::new(NoPassword)).unwrap();
        let model = DirModel::build(&reader);

        // root has "src"; "src" has "readme.txt" + "sub"; "src/sub" has "note.txt".
        assert!(model.tree[""].iter().any(|c| c.name == "src" && c.is_dir));
        let src = &model.tree["src"];
        assert!(src.iter().any(|c| c.name == "readme.txt" && !c.is_dir));
        assert!(src.iter().any(|c| c.name == "sub" && c.is_dir));
        assert!(model.tree["src/sub"].iter().any(|c| c.name == "note.txt"));
        // lookup resolves a file to its size + a real index.
        let f = model.lookup["src/readme.txt"];
        assert_eq!(f.size, 12);
        assert!(!f.is_dir);
        assert_ne!(f.index, usize::MAX);

        let _ = fs::remove_dir_all(&dir);
    }
}
