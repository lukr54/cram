//! The Windows ProjFS provider: starts virtualization at a root folder and answers the five
//! read-only callbacks from a boxed [`RandomAccessReader`] (a `.cram` or ZIP backend) via a
//! [`DirModel`].

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Mutex;

use windows::core::{GUID, HRESULT, PCWSTR};
// Types only — the functions are bound at run time by `projfs_api`, because a load-time import of
// ProjectedFSLib.dll stops the whole binary from starting wherever the optional feature is off.
use windows::Win32::Storage::ProjectedFileSystem::{
    PRJ_CALLBACKS, PRJ_CALLBACK_DATA, PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN,
    PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO, PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    PRJ_PLACEHOLDER_INFO,
};

use cram_core::error::{ArchiveError, Result};
use cram_core::reader::RandomAccessReader;

use crate::projfs_api;
use crate::{Child, DirModel};

// HRESULT codes (windows_core::HRESULT is a newtype over i32).
const S_OK: HRESULT = HRESULT(0);
const E_INVALIDARG: HRESULT = HRESULT(0x8007_0057u32 as i32);
const E_FAIL: HRESULT = HRESULT(0x8000_4005u32 as i32);
const E_OUTOFMEMORY: HRESULT = HRESULT(0x8007_000Eu32 as i32);
const ERROR_FILE_NOT_FOUND_HR: HRESULT = HRESULT(0x8007_0002u32 as i32);
/// `HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED)` — returned for a read that fails because a password is
/// missing/wrong (e.g. an encrypted ZIP entry the mount can list but not decrypt), so the OS surfaces
/// a meaningful "access denied" instead of a generic failure.
const ERROR_ACCESS_DENIED_HR: HRESULT = HRESULT(0x8007_0005u32 as i32);

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// GUIDs are the enumeration-session keys; a plain tuple is Hash+Eq without a u128 conversion.
type EnumKey = (u32, u16, u16, [u8; 8]);
fn key(g: GUID) -> EnumKey {
    (g.data1, g.data2, g.data3, g.data4)
}

struct EnumSession {
    children: Vec<Child>,
    cursor: usize,
}

/// Shared, thread-safe state handed to every ProjFS callback via the instance context. `reader` is
/// boxed behind the `RandomAccessReader` trait (which is `Send + Sync`), so ProjFS's own callback
/// threads share it by `&` regardless of the concrete backend (`.cram` or ZIP).
struct MountState {
    reader: Box<dyn RandomAccessReader>,
    model: DirModel,
    enums: Mutex<HashMap<EnumKey, EnumSession>>,
}

/// A running virtualization. `Drop` stops it, frees the owned state, and scrubs the root.
pub(crate) struct MountInner {
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    state: *mut MountState,
    root: std::path::PathBuf,
}

impl MountInner {
    pub(crate) fn start(reader: Box<dyn RandomAccessReader>, root: &Path) -> Result<Self> {
        // Checked up front so a machine without the optional feature gets the instructions to turn
        // it on, rather than an HRESULT from whichever call happened to run first.
        if !projfs_api::available() {
            return Err(ArchiveError::Backend(projfs_api::UNAVAILABLE.to_string()));
        }
        let model = DirModel::build(&*reader);

        // Do every fallible setup step BEFORE handing the state to a raw pointer. Between
        // `Box::into_raw` and a successful `PrjStartVirtualizing` nothing owns the box, so an early
        // `?` return here would leak the whole MountState (reader + model). Ordering it this way
        // means the only raw-pointer window is the virtualization call, whose failure arm reclaims.
        std::fs::create_dir_all(root)?;
        let root_w = wide(&root.to_string_lossy());
        let instance_id = random_guid()?;

        let state = Box::into_raw(Box::new(MountState {
            reader,
            model,
            enums: Mutex::new(HashMap::new()),
        }));

        let result = (|| unsafe {
            projfs_api::mark_directory_as_placeholder(PCWSTR(root_w.as_ptr()), &instance_id)
                .map_err(|e| ArchiveError::Backend(format!("ProjFS mark placeholder: {e}")))?;

            let callbacks = PRJ_CALLBACKS {
                StartDirectoryEnumerationCallback: Some(start_enum),
                EndDirectoryEnumerationCallback: Some(end_enum),
                GetDirectoryEnumerationCallback: Some(get_enum),
                GetPlaceholderInfoCallback: Some(get_placeholder),
                GetFileDataCallback: Some(get_file_data),
                QueryFileNameCallback: None,
                NotificationCallback: None,
                CancelCommandCallback: None,
            };
            projfs_api::start_virtualizing(
                PCWSTR(root_w.as_ptr()),
                &callbacks,
                state as *const c_void,
            )
            .map_err(|e| ArchiveError::Backend(format!("ProjFS start: {e}")))
        })();

        match result {
            Ok(ctx) => Ok(MountInner {
                ctx,
                state,
                root: root.to_path_buf(),
            }),
            Err(e) => {
                // Reclaim the leaked state box on the failure path.
                unsafe { drop(Box::from_raw(state)) };
                Err(e)
            }
        }
    }
}

impl Drop for MountInner {
    fn drop(&mut self) {
        unsafe {
            projfs_api::stop_virtualizing(self.ctx);
            drop(Box::from_raw(self.state));
        }
        // ProjFS has NO unmark API: after stopping, the reparse-tagged root and every placeholder
        // hydrated while browsing persist on disk. Left in place they (a) show the user a stale,
        // partially-hydrated copy that looks like real files after "unmount", and (b) make the next
        // `PrjMarkDirectoryAsPlaceholder` on the same root fail (ERROR_REPARSE_POINT_ENCOUNTERED).
        // Best-effort scrub; `remove_dir_all` can choke on dead placeholders, so fall back to
        // `cmd /c rmdir` which tolerates them.
        force_remove_dir(&self.root);
    }
}

/// Robustly delete a (possibly ProjFS-marked) directory tree, best-effort. `std::fs::remove_dir_all`
/// does not treat `IO_REPARSE_TAG_PROJFS` as a name surrogate, so it can fail on a stopped root;
/// `cmd /c rmdir /s /q` deletes through a path that tolerates dead placeholders.
fn force_remove_dir(dir: &Path) {
    if !dir.exists() {
        return;
    }
    let _ = std::fs::remove_dir_all(dir);
    if !dir.exists() {
        return;
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // `cmd` splits on `/`, so a path like `C:/Users/...` — which is exactly what you get when the
    // mount root comes from a shell or a Rust `Path` built from one — makes it read `/Users` as a
    // switch and refuse ("Invalid switch"). The scrub would then silently do nothing, leaving the
    // reparse-tagged root on disk, and the NEXT mount of that same folder would fail for good with
    // ERROR_REPARSE_POINT_ENCOUNTERED. Hand it native separators.
    let native = dir.to_string_lossy().replace('/', "\\");
    let _ = std::process::Command::new("cmd")
        .args(["/c", "rmdir", "/s", "/q"])
        .arg(&native)
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

// ---- helpers ----

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn random_guid() -> Result<GUID> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| ArchiveError::Backend(format!("rng: {e}")))?;
    Ok(GUID::from_u128(u128::from_le_bytes(b)))
}

fn basic_info(is_dir: bool, size: u64) -> PRJ_FILE_BASIC_INFO {
    let mut bi: PRJ_FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    bi.IsDirectory = is_dir.into();
    bi.FileSize = size as i64;
    bi.FileAttributes = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    bi
}

/// Borrow the shared mount state from a callback's instance context.
///
/// # Safety
/// `cb` must be a live ProjFS callback-data pointer whose `InstanceContext` is the `MountState` we
/// registered in `PrjStartVirtualizing` (ProjFS guarantees both for the lifetime of the mount).
unsafe fn state<'a>(cb: *const PRJ_CALLBACK_DATA) -> &'a MountState {
    &*((*cb).InstanceContext as *const MountState)
}

/// The canonical (case-folded, forward-slash, slash-trimmed) archive key a callback refers to
/// ("" = root). Folded to match how `DirModel` keys `tree`/`lookup`, so an enumerated name comes
/// back to the same node regardless of the archive's original casing. Display casing is untouched:
/// placeholder writes use `(*cb).FilePathName` directly, not this.
unsafe fn path_of(cb: *const PRJ_CALLBACK_DATA) -> String {
    let p = (*cb).FilePathName;
    if p.is_null() {
        return String::new();
    }
    let raw = p.to_string().unwrap_or_default().replace('\\', "/");
    crate::fold(raw.trim_matches('/'))
}

// ---- the five callbacks ----

unsafe extern "system" fn start_enum(
    cb: *const PRJ_CALLBACK_DATA,
    enum_id: *const GUID,
) -> HRESULT {
    let st = state(cb);
    let children = st.model.tree.get(&path_of(cb)).cloned().unwrap_or_default();
    st.enums.lock().unwrap().insert(
        key(*enum_id),
        EnumSession {
            children,
            cursor: 0,
        },
    );
    S_OK
}

unsafe extern "system" fn end_enum(cb: *const PRJ_CALLBACK_DATA, enum_id: *const GUID) -> HRESULT {
    state(cb).enums.lock().unwrap().remove(&key(*enum_id));
    S_OK
}

unsafe extern "system" fn get_enum(
    cb: *const PRJ_CALLBACK_DATA,
    enum_id: *const GUID,
    search: PCWSTR,
    buf: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    let st = state(cb);
    let mut enums = st.enums.lock().unwrap();
    let Some(sess) = enums.get_mut(&key(*enum_id)) else {
        return E_INVALIDARG;
    };
    if (*cb).Flags.0 & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN.0 != 0 {
        sess.cursor = 0;
    }
    let filtered = !search.is_null();
    let mut filled_any = false;
    while sess.cursor < sess.children.len() {
        let child = sess.children[sess.cursor].clone();
        let name_w = wide(&child.name);
        let name = PCWSTR(name_w.as_ptr());
        if filtered && !projfs_api::file_name_match(name, search).as_bool() {
            sess.cursor += 1;
            continue;
        }
        let bi = basic_info(child.is_dir, child.size);
        match projfs_api::fill_dir_entry_buffer(name, &bi, buf) {
            Ok(()) => {
                sess.cursor += 1;
                filled_any = true;
            }
            // Buffer full mid-batch: stop with S_OK; ProjFS re-invokes and we resume at the
            // preserved cursor. But if even the FIRST entry of this invocation didn't fit
            // (caller's buffer too small for one record — e.g. a long name with a
            // single-entry query), S_OK would signal end-of-enumeration and every child from
            // the cursor onward would silently vanish from the listing. The contract is to
            // return the failure so the OS retries with a larger buffer.
            Err(e) => {
                if filled_any {
                    break;
                }
                return e.code();
            }
        }
    }
    S_OK
}

unsafe extern "system" fn get_placeholder(cb: *const PRJ_CALLBACK_DATA) -> HRESULT {
    let st = state(cb);
    let Some(info) = st.model.lookup.get(&path_of(cb)).copied() else {
        return ERROR_FILE_NOT_FOUND_HR;
    };
    let mut ph: PRJ_PLACEHOLDER_INFO = std::mem::zeroed();
    ph.FileBasicInfo = basic_info(info.is_dir, info.size);
    match projfs_api::write_placeholder_info(
        (*cb).NamespaceVirtualizationContext,
        (*cb).FilePathName,
        &ph,
        std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
    ) {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

unsafe extern "system" fn get_file_data(
    cb: *const PRJ_CALLBACK_DATA,
    byte_offset: u64,
    length: u32,
) -> HRESULT {
    let st = state(cb);
    let Some(info) = st.model.lookup.get(&path_of(cb)).copied() else {
        return ERROR_FILE_NOT_FOUND_HR;
    };
    if info.is_dir || info.index == usize::MAX {
        return E_INVALIDARG;
    }
    let ctx = (*cb).NamespaceVirtualizationContext;
    let data = match st.reader.read_range(info.index, byte_offset, length as u64) {
        Ok(d) => d,
        // A missing/wrong password (e.g. an encrypted ZIP entry the mount listed but can't decrypt
        // without the password supplied at mount time) maps to ACCESS_DENIED so the OS reports
        // something meaningful; any other backend error is a generic failure.
        Err(ArchiveError::PasswordRequired) | Err(ArchiveError::WrongPassword) => {
            return ERROR_ACCESS_DENIED_HR
        }
        Err(_) => return E_FAIL,
    };
    // Guard against a short decode: the placeholder advertised `info.size` (from the archive's own
    // metadata, e.g. a ZIP central-directory size), so a read within that size must return the bytes
    // it promised. If the entry actually decodes short (a malformed/lying archive), returning S_OK
    // with fewer bytes would silently truncate the virtual file; surface it as a read failure instead.
    let expected = info.size.saturating_sub(byte_offset).min(length as u64);
    if (data.len() as u64) < expected {
        return E_FAIL;
    }
    if data.is_empty() {
        return S_OK;
    }
    let buf = projfs_api::allocate_aligned_buffer(ctx, data.len());
    if buf.is_null() {
        return E_OUTOFMEMORY;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len());
    let r = projfs_api::write_file_data(
        ctx,
        &(*cb).DataStreamId,
        buf,
        byte_offset,
        data.len() as u32,
    );
    projfs_api::free_aligned_buffer(buf);
    match r {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}
