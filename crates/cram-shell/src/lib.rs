//! Cram's Explorer context menu — a COM `IContextMenu` handler.
//!
//! Explorer loads this DLL in-process, calls `Initialize` with whatever is selected,
//! `QueryContextMenu` to add items, and `InvokeCommand` when one is chosen, at which point we
//! spawn `cram.exe` (or Studio) and get out of the way.
//!
//! **Why COM and not registry verbs.** Plain `HKCR\<type>\shell\<verb>` entries are far simpler and
//! were tried first, in this project's predecessor: they did not render. A COM handler does, and it
//! is how WinRAR and 7-Zip appear too. The cost is that this code runs inside Explorer, which sets
//! the rules below.
//!
//! **Rules for code that lives in someone else's process.**
//! - Never block. `InvokeCommand` spawns and returns; it does not wait for an extraction.
//! - Never panic across the FFI boundary. Every entry point returns an `HRESULT`, and a panic
//!   unwinding into Explorer's C++ frames is undefined behaviour, so the panic-prone parts are
//!   kept out rather than caught.
//! - Do the least work possible in `QueryContextMenu`. It runs on every right-click, so it looks at
//!   file *extensions* and never opens a file to sniff it.
//!
//! The menu is one submenu whose contents depend on the selection: extract verbs when everything
//! selected is an archive, create verbs otherwise. The two Studio entries appear only when
//! `cram-studio.exe` is actually sitting next to this DLL, so a CLI-only install never offers to
//! open something that is not installed.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::cell::RefCell;
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::HKEY;
use windows::Win32::UI::Shell::Common::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// This handler's class id, `{934088FE-F647-4D05-9A52-FDF56127F43C}`.
///
/// Stable forever once shipped: it is what the registry points at, so changing it orphans every
/// existing registration. Deliberately **not** the predecessor's id — two different DLLs claiming
/// one class id is a coin toss over which one Explorer loads.
const CLSID_CRAM_SHELL: GUID = GUID::from_u128(0x934088fe_f647_4d05_9a52_fdf56127f43c);

/// Outstanding objects plus lock count; `DllCanUnloadNow` reports zero as "safe to unload".
static DLL_REFS: AtomicUsize = AtomicUsize::new(0);
/// This DLL's own module handle, captured in `DllMain`, used to find the binaries beside it.
static MODULE: AtomicIsize = AtomicIsize::new(0);

/// Extensions that put the *extract* verbs on the menu.
///
/// Kept in step with what the engine actually reads. A file whose extension is not here is treated
/// as ordinary content to be archived, which is the safe direction to be wrong in: offering
/// "Extract here" on something unreadable produces an error dialog, whereas offering "Add to
/// archive" on an archive merely nests it, which is a legitimate thing to want.
const ARCHIVE_EXTS: &[&str] = &[
    "cram", "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "tzst",
    "lz4", "br", "cab", "iso", "jar", "whl", "apk", "docx", "xlsx", "pptx", "epub",
];

/// Menu item ids, offsets from `idCmdFirst`. The order here is the order on screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verb {
    ExtractHere,
    ExtractTo,
    Test,
    OpenStudio,
    AddDialog,
    AddCram,
    AddZip,
}

impl Verb {
    /// The canonical (language-independent) verb name Explorer may ask for via `GetCommandString`.
    /// Windows uses it for accessibility and for scripted invocation, so it must not be localised.
    fn canonical(self) -> &'static str {
        match self {
            Verb::ExtractHere => "CramExtractHere",
            Verb::ExtractTo => "CramExtractTo",
            Verb::Test => "CramTest",
            Verb::OpenStudio => "CramOpenStudio",
            Verb::AddDialog => "CramAddDialog",
            Verb::AddCram => "CramAddCram",
            Verb::AddZip => "CramAddZip",
        }
    }

    /// One-line help, shown in Explorer's status bar.
    fn help(self) -> &'static str {
        match self {
            Verb::ExtractHere => "Extract the contents into this folder",
            Verb::ExtractTo => "Extract the contents into a new folder",
            Verb::Test => "Check the archive decodes and its checksums match",
            Verb::OpenStudio => "Open the archive in Cram Studio",
            Verb::AddDialog => "Create an archive in Cram Studio",
            Verb::AddCram => "Create a .cram archive",
            Verb::AddZip => "Create a .zip archive",
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A binary shipped beside this DLL. Cram's installer and its release archive both keep
/// `cram.exe`, `cram-extract.exe` and (for Studio users) `cram-studio.exe` in one directory, so
/// "next to me" is the whole search path — no registry lookup, nothing to go stale.
fn sibling(name: &str) -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let n = unsafe {
        GetModuleFileNameW(
            HMODULE(MODULE.load(Ordering::Relaxed) as *mut c_void),
            &mut buf,
        )
    };
    if n == 0 {
        return None;
    }
    let dll = PathBuf::from(OsString::from_wide(&buf[..n as usize]));
    let p = dll.parent()?.join(name);
    p.is_file().then_some(p)
}

fn cram_exe() -> Option<PathBuf> {
    sibling("cram.exe")
}

fn studio_exe() -> Option<PathBuf> {
    sibling("cram-studio.exe")
}

fn is_archive(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| ARCHIVE_EXTS.iter().any(|a| e.eq_ignore_ascii_case(a)))
        .unwrap_or(false)
}

/// Quote one argument for a Windows command line.
///
/// The paths come from Explorer, so they contain spaces routinely and can contain `"` — a file
/// name may legally hold one on some volumes. Without escaping, such a name would end the quoted
/// span and the rest of the path would be parsed as further arguments.
fn quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Start `exe` with `args`, detached, and return immediately.
///
/// `ShellExecuteW` rather than `CreateProcessW` because it is fire-and-forget and inherits none of
/// Explorer's handles. The return value is deliberately ignored: a failure to launch is not
/// something to raise a dialog about from inside Explorer, and the spawned process reports its own
/// errors.
fn spawn(exe: &Path, args: &str) {
    let exe_w = wide(&exe.to_string_lossy());
    let args_w = wide(args);
    let op = wide("open");
    // Run from the target's own folder, so a relative path a verb builds resolves where the user
    // is looking rather than wherever Explorer happened to be.
    let dir = exe.parent().map(|d| wide(&d.to_string_lossy()));
    unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(op.as_ptr()),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            dir.as_ref()
                .map(|d| PCWSTR(d.as_ptr()))
                .unwrap_or(PCWSTR::null()),
            SW_SHOWNORMAL,
        );
    }
}

/// The name to give an archive built from `files`: the item's own stem for a single selection, the
/// containing folder's name for several — the same choice every other archiver makes, because
/// "3 files.zip" has no better name available.
fn archive_stem(files: &[PathBuf]) -> String {
    let fallback = || "archive".to_string();
    match files {
        [] => fallback(),
        [one] => one
            .file_stem()
            .or_else(|| one.file_name())
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .unwrap_or_else(fallback),
        many => many[0]
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .unwrap_or_else(fallback),
    }
}

/// Where a created archive goes: beside the selection.
fn output_dir(files: &[PathBuf]) -> Option<PathBuf> {
    files.first()?.parent().map(Path::to_path_buf)
}

/// What belongs on the submenu for this selection — the whole decision, separated from the Win32
/// calls so it can be tested. `QueryContextMenu` only turns this list into menu items.
///
/// "All archives" rather than "any archive": a mixed selection becomes a create, because
/// extracting part of it and ignoring the rest would be a guess about what was meant.
fn menu_items(files: &[PathBuf], studio: bool) -> Vec<(Verb, String)> {
    if files.is_empty() {
        return Vec::new();
    }
    let stem = archive_stem(files);
    let mut items = Vec::new();
    if files.iter().all(|f| is_archive(f)) {
        items.push((Verb::ExtractHere, "Extract here".to_string()));
        items.push((Verb::ExtractTo, format!("Extract to {stem}\\")));
        items.push((Verb::Test, "Test archive".to_string()));
        if studio {
            items.push((Verb::OpenStudio, "Open in Cram Studio".to_string()));
        }
    } else {
        if studio {
            items.push((Verb::AddDialog, "Add to archive…".to_string()));
        }
        items.push((Verb::AddCram, format!("Add to {stem}.cram")));
        items.push((Verb::AddZip, format!("Add to {stem}.zip")));
    }
    items
}

#[implement(IShellExtInit, IContextMenu)]
struct CramMenu {
    files: RefCell<Vec<PathBuf>>,
    /// Which verbs were put on the menu, in order. `InvokeCommand` receives an index into this,
    /// so it must be rebuilt on every `QueryContextMenu` and never assumed.
    verbs: RefCell<Vec<Verb>>,
}

impl CramMenu {
    fn new() -> Self {
        DLL_REFS.fetch_add(1, Ordering::Relaxed);
        Self {
            files: RefCell::new(Vec::new()),
            verbs: RefCell::new(Vec::new()),
        }
    }
}

impl Drop for CramMenu {
    fn drop(&mut self) {
        DLL_REFS.fetch_sub(1, Ordering::Relaxed);
    }
}

impl IShellExtInit_Impl for CramMenu_Impl {
    fn Initialize(
        &self,
        _folder: *const ITEMIDLIST,
        pdtobj: Option<&IDataObject>,
        _progid: HKEY,
    ) -> Result<()> {
        let data = pdtobj.ok_or_else(|| Error::from(E_INVALIDARG))?;
        unsafe {
            let items: IShellItemArray = SHCreateShellItemArrayFromDataObject(data)?;
            let count = items.GetCount()?;
            let mut files = Vec::new();
            for i in 0..count {
                let item = items.GetItemAt(i)?;
                // A selection can include things with no path at all (a library, a device, a
                // search result). Skipping them is what keeps the menu off "This PC".
                if let Ok(pw) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                    if let Ok(s) = pw.to_string() {
                        files.push(PathBuf::from(s));
                    }
                    CoTaskMemFree(Some(pw.0 as *const c_void));
                }
            }
            *self.files.borrow_mut() = files;
        }
        Ok(())
    }
}

impl IContextMenu_Impl for CramMenu_Impl {
    fn QueryContextMenu(
        &self,
        hmenu: HMENU,
        indexmenu: u32,
        idcmdfirst: u32,
        _idcmdlast: u32,
        uflags: u32,
    ) -> Result<()> {
        self.verbs.borrow_mut().clear();

        // CMF_DEFAULTONLY: Explorer is asking only for the double-click action, not for a menu.
        const CMF_DEFAULTONLY: u32 = 0x0000_0001;
        let files = self.files.borrow();
        if uflags & CMF_DEFAULTONLY != 0 || files.is_empty() {
            return Ok(());
        }

        let Some(_cram) = cram_exe() else {
            // Registered but the binaries are gone (a moved or half-removed install). Adding a
            // menu whose every item does nothing is worse than adding none.
            return Ok(());
        };
        let items = menu_items(&files, studio_exe().is_some());
        if items.is_empty() {
            return Ok(());
        }

        // Build the submenu first, then hang it off one item on Explorer's menu: a single "Cram"
        // line, whatever the selection.
        let submenu = unsafe { CreatePopupMenu() }?;
        let mut added = 0u32;
        for (i, (verb, label)) in items.iter().enumerate() {
            let w = wide(label);
            // A separator above the Studio entry: it opens a window, the others just run.
            if *verb == Verb::OpenStudio {
                unsafe {
                    let _ = InsertMenuW(
                        submenu,
                        added,
                        MF_BYPOSITION | MF_SEPARATOR,
                        0,
                        PCWSTR::null(),
                    );
                }
                added += 1;
            }
            unsafe {
                let _ = InsertMenuW(
                    submenu,
                    added,
                    MF_BYPOSITION | MF_STRING,
                    idcmdfirst as usize + i,
                    PCWSTR(w.as_ptr()),
                );
            }
            added += 1;
            self.verbs.borrow_mut().push(*verb);
        }

        // `MF_POPUP` takes the submenu handle where an item id would go, which is how a submenu is
        // attached without `MENUITEMINFOW` — that struct carries HBITMAP fields and would pull the
        // whole GDI surface into this DLL for one menu line.
        let parent = wide("Cram");
        unsafe {
            InsertMenuW(
                hmenu,
                indexmenu,
                MF_BYPOSITION | MF_POPUP,
                submenu.0 as usize,
                PCWSTR(parent.as_ptr()),
            )?;
        }

        // The number of command ids used, returned as a success HRESULT — the documented protocol
        // for this method, and the reason it comes back through `Err`: `Ok(())` can only ever be
        // S_OK, i.e. "0 items added", which would let Explorer hand our ids to the next handler.
        // The parent item consumes none, since MF_POPUP put a menu handle in that field.
        Err(Error::from_hresult(HRESULT(items.len() as i32)))
    }

    fn InvokeCommand(&self, pici: *const CMINVOKECOMMANDINFO) -> Result<()> {
        let ici = unsafe { &*pici };
        // The verb arrives either as a small integer in the low word of a pointer, or as a string
        // (scripted invocation). Only the integer form is offered here; a string verb we do not
        // recognise must be refused rather than guessed at.
        let id = ici.lpVerb.0 as usize;
        if id > 0xFFFF {
            return Err(Error::from(E_INVALIDARG));
        }
        let verbs = self.verbs.borrow();
        let Some(&verb) = verbs.get(id) else {
            return Err(Error::from(E_INVALIDARG));
        };
        let files = self.files.borrow();
        if files.is_empty() {
            return Err(Error::from(E_UNEXPECTED));
        }

        match verb {
            Verb::OpenStudio | Verb::AddDialog => {
                let Some(studio) = studio_exe() else {
                    return Err(Error::from(E_FAIL));
                };
                let flag = if verb == Verb::OpenStudio {
                    "--open"
                } else {
                    "--add"
                };
                let args = std::iter::once(flag.to_string())
                    .chain(files.iter().map(|f| quote(f)))
                    .collect::<Vec<_>>()
                    .join(" ");
                spawn(&studio, &args);
            }
            Verb::ExtractHere | Verb::ExtractTo | Verb::Test => {
                let Some(cram) = cram_exe() else {
                    return Err(Error::from(E_FAIL));
                };
                // One process per archive: `cram x` takes a single archive, and separate processes
                // mean one unreadable file cannot take the rest of the selection down with it.
                for f in files.iter() {
                    let args = match verb {
                        Verb::Test => format!("t {}", quote(f)),
                        Verb::ExtractHere => {
                            let Some(dir) = f.parent() else { continue };
                            format!("x {} -o {}", quote(f), quote(dir))
                        }
                        _ => {
                            let Some(dir) = f.parent() else { continue };
                            let stem = f
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("extracted");
                            format!("x {} -o {}", quote(f), quote(&dir.join(stem)))
                        }
                    };
                    spawn(&cram, &args);
                }
            }
            Verb::AddCram | Verb::AddZip => {
                let Some(cram) = cram_exe() else {
                    return Err(Error::from(E_FAIL));
                };
                let Some(dir) = output_dir(&files) else {
                    return Err(Error::from(E_FAIL));
                };
                let ext = if verb == Verb::AddCram { "cram" } else { "zip" };
                let out = dir.join(format!("{}.{ext}", archive_stem(&files)));
                // One process for the whole selection here: a single archive is the point.
                let args = std::iter::once(format!("a {}", quote(&out)))
                    .chain(files.iter().map(|f| quote(f)))
                    .collect::<Vec<_>>()
                    .join(" ");
                spawn(&cram, &args);
            }
        }
        Ok(())
    }

    fn GetCommandString(
        &self,
        idcmd: usize,
        uflags: u32,
        _reserved: *const u32,
        pszname: PSTR,
        cchmax: u32,
    ) -> Result<()> {
        const GCS_VERBA: u32 = 0x0000_0000;
        const GCS_HELPTEXTA: u32 = 0x0000_0001;
        const GCS_VERBW: u32 = 0x0000_0004;
        const GCS_HELPTEXTW: u32 = 0x0000_0005;

        let verbs = self.verbs.borrow();
        let Some(&verb) = verbs.get(idcmd) else {
            return Err(Error::from(E_INVALIDARG));
        };
        let text = match uflags {
            GCS_VERBA | GCS_VERBW => verb.canonical(),
            GCS_HELPTEXTA | GCS_HELPTEXTW => verb.help(),
            _ => return Err(Error::from(E_NOTIMPL)),
        };
        if pszname.is_null() || cchmax == 0 {
            return Err(Error::from(E_INVALIDARG));
        }

        // The same out-parameter is a `char*` or a `wchar_t*` depending on the flag, and the caller
        // sized it in *characters* of that width. Writing the wrong width here is the classic way
        // to corrupt Explorer's stack.
        let wide_form = uflags == GCS_VERBW || uflags == GCS_HELPTEXTW;
        unsafe {
            if wide_form {
                let src = wide(text);
                if src.len() > cchmax as usize {
                    return Err(Error::from(E_OUTOFMEMORY));
                }
                std::ptr::copy_nonoverlapping(src.as_ptr(), pszname.0 as *mut u16, src.len());
            } else {
                let mut src = text.as_bytes().to_vec();
                src.push(0);
                if src.len() > cchmax as usize {
                    return Err(Error::from(E_OUTOFMEMORY));
                }
                std::ptr::copy_nonoverlapping(src.as_ptr(), pszname.0, src.len());
            }
        }
        Ok(())
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Option<&IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        if punkouter.is_some() {
            return Err(Error::from(CLASS_E_NOAGGREGATION));
        }
        let instance: IShellExtInit = CramMenu::new().into();
        unsafe { instance.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        if flock.as_bool() {
            DLL_REFS.fetch_add(1, Ordering::Relaxed);
        } else {
            DLL_REFS.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[no_mangle]
extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        if *rclsid != CLSID_CRAM_SHELL {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = Factory.into();
        factory.query(riid, ppv)
    }
}

#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    if DLL_REFS.load(Ordering::Relaxed) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

#[no_mangle]
extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(hinst.0 as isize, Ordering::Relaxed);
    }
    BOOL(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn an_archive_selection_offers_to_extract_and_anything_else_to_create() {
        let extract: Vec<Verb> = menu_items(&[p(r"C:\d\photos.zip")], false)
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        assert_eq!(
            extract,
            vec![Verb::ExtractHere, Verb::ExtractTo, Verb::Test],
            "an archive gets the extract verbs"
        );

        let create: Vec<Verb> = menu_items(&[p(r"C:\d\notes.txt")], false)
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        assert_eq!(create, vec![Verb::AddCram, Verb::AddZip]);

        // Mixed: extracting some of the selection and ignoring the rest would be a guess.
        let mixed: Vec<Verb> = menu_items(&[p(r"C:\d\a.zip"), p(r"C:\d\b.txt")], false)
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        assert_eq!(mixed, vec![Verb::AddCram, Verb::AddZip]);
    }

    #[test]
    fn studio_only_verbs_are_absent_without_studio() {
        for files in [vec![p(r"C:\d\a.zip")], vec![p(r"C:\d\a.txt")]] {
            let without = menu_items(&files, false);
            assert!(
                !without
                    .iter()
                    .any(|(v, _)| matches!(v, Verb::OpenStudio | Verb::AddDialog)),
                "a CLI-only install must not offer to open something that is not there"
            );
            let with = menu_items(&files, true);
            assert_eq!(
                with.len(),
                without.len() + 1,
                "Studio adds exactly one item"
            );
        }
    }

    #[test]
    fn the_proposed_name_is_the_item_or_its_folder() {
        // One item: its own stem.
        assert_eq!(archive_stem(&[p(r"C:\d\photos.zip")]), "photos");
        assert_eq!(archive_stem(&[p(r"C:\d\notes.txt")]), "notes");
        // Several: the containing folder, since there is no better name available.
        assert_eq!(
            archive_stem(&[p(r"C:\d\holiday\a.txt"), p(r"C:\d\holiday\b.txt")]),
            "holiday"
        );
        assert_eq!(archive_stem(&[]), "archive");
        // A dotted name keeps everything before the LAST dot, matching what the shell shows.
        assert_eq!(archive_stem(&[p(r"C:\d\backup.2026.tar")]), "backup.2026");
    }

    #[test]
    fn labels_name_the_thing_that_will_be_produced() {
        let items = menu_items(&[p(r"C:\d\photos.zip")], false);
        assert_eq!(items[1].1, "Extract to photos\\");
        let items = menu_items(&[p(r"C:\d\notes.txt")], false);
        assert_eq!(items[0].1, "Add to notes.cram");
        assert_eq!(items[1].1, "Add to notes.zip");
    }

    #[test]
    fn extension_matching_is_case_insensitive_and_not_a_substring_match() {
        assert!(is_archive(&p("a.ZIP")));
        assert!(is_archive(&p("a.Cram")));
        assert!(is_archive(&p("a.tar.gz")), "the last extension decides");
        assert!(!is_archive(&p("a.txt")));
        assert!(!is_archive(&p("a.zipper")), "not a prefix match");
        assert!(!is_archive(&p("zip")), "no extension at all");
        assert!(!is_archive(&p("a.")));
    }

    /// The paths come straight from Explorer. A quote inside a file name would otherwise close the
    /// quoted span and turn the rest of the path into further arguments.
    #[test]
    fn paths_survive_being_put_on_a_command_line() {
        assert_eq!(quote(&p(r"C:\a b\c.zip")), "\"C:\\a b\\c.zip\"");
        assert_eq!(quote(&p(r#"C:\we"rd.zip"#)), "\"C:\\we\\\"rd.zip\"");
        assert!(quote(&p(r"C:\d\file.zip")).starts_with('"'));
        assert!(quote(&p(r"C:\d\file.zip")).ends_with('"'));
    }

    #[test]
    fn a_new_archive_lands_beside_what_was_selected() {
        assert_eq!(
            output_dir(&[p(r"C:\d\holiday\a.txt")]),
            Some(p(r"C:\d\holiday"))
        );
        assert_eq!(output_dir(&[]), None);
    }

    /// Explorer asks for these by id; a wrong or empty answer shows as garbage in the status bar.
    #[test]
    fn every_offered_verb_has_a_canonical_name_and_help_text() {
        for (verb, _) in menu_items(&[p(r"C:\d\a.zip")], true)
            .into_iter()
            .chain(menu_items(&[p(r"C:\d\a.txt")], true))
        {
            assert!(!verb.canonical().is_empty());
            assert!(!verb.help().is_empty());
            assert!(
                verb.canonical().is_ascii(),
                "a canonical verb must not be localised or non-ASCII"
            );
        }
    }
}
