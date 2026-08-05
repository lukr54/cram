//! Lazy, run-time binding to `ProjectedFSLib.dll`.
//!
//! ProjFS is delivered by the **optional** Windows feature `Client-ProjFS`, which is OFF by default:
//! on a stock install the DLL is staged in WinSxS but never projected into `System32`. A *load-time*
//! import of it therefore aborts the entire process at startup with `STATUS_DLL_NOT_FOUND`
//! (0xC0000135), before `main` runs; on every machine that has not enabled the feature.
//! `cram-mount` is linked into `cram.exe` unconditionally, so binding these functions the direct way
//! would stop the whole CLI from starting on any machine without the feature, over a capability
//! that only the `mount` verb needs.
//!
//! So the DLL is loaded on first use instead, and its absence is an ordinary error from `mount`
//! rather than a process that cannot launch. Type definitions still come from the `windows` crate,
//! types carry no linkage; only the function bindings are ours. Each signature below mirrors the
//! SDK's raw C ABI (the `windows` crate's ergonomic wrappers hide out-params and HRESULT checks,
//! which we re-add by hand).

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{s, w, Error as WinError, Result as WinResult, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::BOOLEAN;
use windows::Win32::Storage::ProjectedFileSystem::{
    PRJ_CALLBACKS, PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO,
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, PRJ_PLACEHOLDER_INFO,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// What the user is told when the feature is off. ProjFS cannot be enabled from inside a normal
/// process, it needs an elevated feature install, so the message carries the exact command rather
/// than a bare "not supported".
pub const UNAVAILABLE: &str = "This needs the Windows Projected File System feature, which is off by \
default. Enable it in an admin PowerShell with:\r\n\r\n    Enable-WindowsOptionalFeature -Online \
-FeatureName Client-ProjFS\r\n\r\nA restart may be required. Everything else in Cram works without it.";

/// `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)`, what the shims return if the DLL is missing. In
/// practice unreachable: `available()` is checked before a mount starts, and the callbacks only run
/// while a mount is live.
const E_NOT_SUPPORTED: HRESULT = HRESULT(0x8007_0032u32 as i32);

type FnMarkDirectoryAsPlaceholder =
    unsafe extern "system" fn(PCWSTR, PCWSTR, *const c_void, *const GUID) -> HRESULT;
type FnStartVirtualizing = unsafe extern "system" fn(
    PCWSTR,
    *const PRJ_CALLBACKS,
    *const c_void,
    *const c_void,
    *mut PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
) -> HRESULT;
type FnStopVirtualizing = unsafe extern "system" fn(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT);
type FnFileNameMatch = unsafe extern "system" fn(PCWSTR, PCWSTR) -> BOOLEAN;
type FnFillDirEntryBuffer = unsafe extern "system" fn(
    PCWSTR,
    *const PRJ_FILE_BASIC_INFO,
    PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT;
type FnWritePlaceholderInfo = unsafe extern "system" fn(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    PCWSTR,
    *const PRJ_PLACEHOLDER_INFO,
    u32,
) -> HRESULT;
type FnAllocateAlignedBuffer =
    unsafe extern "system" fn(PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT, usize) -> *mut c_void;
type FnWriteFileData = unsafe extern "system" fn(
    PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    *const GUID,
    *const c_void,
    u64,
    u32,
) -> HRESULT;
type FnFreeAlignedBuffer = unsafe extern "system" fn(*const c_void);
type FnGetOnDiskFileState = unsafe extern "system" fn(PCWSTR, *mut u32) -> HRESULT;

/// The resolved entry points. Only function pointers are kept (they are `Send + Sync`); the module
/// handle is not stored, the DLL stays loaded for the life of the process, which is
/// what we want, and `HMODULE` is not `Sync`.
struct Api {
    mark_directory_as_placeholder: FnMarkDirectoryAsPlaceholder,
    start_virtualizing: FnStartVirtualizing,
    stop_virtualizing: FnStopVirtualizing,
    file_name_match: FnFileNameMatch,
    fill_dir_entry_buffer: FnFillDirEntryBuffer,
    write_placeholder_info: FnWritePlaceholderInfo,
    allocate_aligned_buffer: FnAllocateAlignedBuffer,
    write_file_data: FnWriteFileData,
    free_aligned_buffer: FnFreeAlignedBuffer,
    /// Optional, unlike every field above. It is only needed to tell a file the user changed from
    /// one Cram projected, so a Windows build that somehow lacks the export should lose that
    /// distinction rather than lose mounting altogether.
    get_on_disk_file_state: Option<FnGetOnDiskFileState>,
}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn api() -> Option<&'static Api> {
    API.get_or_init(load).as_ref()
}

/// Resolve every entry point, or none at all: a partial table would turn a missing export into a
/// crash at an arbitrary later moment instead of a clean "feature unavailable" here.
fn load() -> Option<Api> {
    unsafe {
        let module = LoadLibraryW(w!("ProjectedFSLib.dll")).ok()?;
        // Both sides of each transmute are named: the source is what `GetProcAddress` hands back
        // (`FARPROC` after `?`), the destination the specific signature declared above. Spelling
        // them out is what keeps a wrong signature a compile-time question rather than a corrupted
        // stack at the first call.
        macro_rules! sym {
            ($name:literal, $ty:ty) => {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(GetProcAddress(
                    module,
                    s!($name),
                )?)
            };
        }
        Some(Api {
            mark_directory_as_placeholder: sym!(
                "PrjMarkDirectoryAsPlaceholder",
                FnMarkDirectoryAsPlaceholder
            ),
            start_virtualizing: sym!("PrjStartVirtualizing", FnStartVirtualizing),
            stop_virtualizing: sym!("PrjStopVirtualizing", FnStopVirtualizing),
            file_name_match: sym!("PrjFileNameMatch", FnFileNameMatch),
            fill_dir_entry_buffer: sym!("PrjFillDirEntryBuffer", FnFillDirEntryBuffer),
            write_placeholder_info: sym!("PrjWritePlaceholderInfo", FnWritePlaceholderInfo),
            allocate_aligned_buffer: sym!("PrjAllocateAlignedBuffer", FnAllocateAlignedBuffer),
            write_file_data: sym!("PrjWriteFileData", FnWriteFileData),
            free_aligned_buffer: sym!("PrjFreeAlignedBuffer", FnFreeAlignedBuffer),
            get_on_disk_file_state: GetProcAddress(module, s!("PrjGetOnDiskFileState")).map(|p| {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, FnGetOnDiskFileState>(p)
            }),
        })
    }
}

/// Whether ProjFS can be used at all on this machine. Callers should check this before starting a
/// mount so the user gets [`UNAVAILABLE`] rather than a raw HRESULT.
pub fn available() -> bool {
    api().is_some()
}

fn unsupported<T>() -> WinResult<T> {
    Err(WinError::from(E_NOT_SUPPORTED))
}

// ---- the wrappers, shaped like the `windows` crate's own so call sites read unchanged ----

/// # Safety
/// `root` must be a valid null-terminated wide string; `instance_id` a valid GUID pointer.
pub unsafe fn mark_directory_as_placeholder(
    root: PCWSTR,
    instance_id: *const GUID,
) -> WinResult<()> {
    let Some(api) = api() else {
        return unsupported();
    };
    (api.mark_directory_as_placeholder)(root, PCWSTR::null(), std::ptr::null(), instance_id).ok()
}

/// # Safety
/// `root` must be a valid null-terminated wide string, `callbacks` a valid table that outlives the
/// call, and `instance_context` the pointer ProjFS will hand back to every callback.
pub unsafe fn start_virtualizing(
    root: PCWSTR,
    callbacks: *const PRJ_CALLBACKS,
    instance_context: *const c_void,
) -> WinResult<PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT> {
    let Some(api) = api() else {
        return unsupported();
    };
    let mut ctx = PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT::default();
    (api.start_virtualizing)(
        root,
        callbacks,
        instance_context,
        std::ptr::null(),
        &mut ctx,
    )
    .ok()
    .map(|()| ctx)
}

/// # Safety
/// `ctx` must come from a successful [`start_virtualizing`] and not have been stopped already.
pub unsafe fn stop_virtualizing(ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT) {
    if let Some(api) = api() {
        (api.stop_virtualizing)(ctx);
    }
}

/// # Safety
/// Both arguments must be valid null-terminated wide strings.
pub unsafe fn file_name_match(name: PCWSTR, pattern: PCWSTR) -> BOOLEAN {
    match api() {
        Some(api) => (api.file_name_match)(name, pattern),
        None => BOOLEAN(0),
    }
}

/// # Safety
/// Callable only from inside a directory-enumeration callback, with that callback's buffer handle.
pub unsafe fn fill_dir_entry_buffer(
    name: PCWSTR,
    info: *const PRJ_FILE_BASIC_INFO,
    buf: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> WinResult<()> {
    let Some(api) = api() else {
        return unsupported();
    };
    (api.fill_dir_entry_buffer)(name, info, buf).ok()
}

/// # Safety
/// Callable only from inside a placeholder-info callback, with that callback's context and path.
pub unsafe fn write_placeholder_info(
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    path: PCWSTR,
    info: *const PRJ_PLACEHOLDER_INFO,
    size: u32,
) -> WinResult<()> {
    let Some(api) = api() else {
        return unsupported();
    };
    (api.write_placeholder_info)(ctx, path, info, size).ok()
}

/// Returns null on failure (including when ProjFS is unavailable), matching the SDK's contract.
///
/// # Safety
/// `ctx` must be a live virtualization context.
pub unsafe fn allocate_aligned_buffer(
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    size: usize,
) -> *mut c_void {
    match api() {
        Some(api) => (api.allocate_aligned_buffer)(ctx, size),
        None => std::ptr::null_mut(),
    }
}

/// # Safety
/// `buffer` must be at least `length` bytes and allocated by [`allocate_aligned_buffer`].
pub unsafe fn write_file_data(
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    stream_id: *const GUID,
    buffer: *const c_void,
    offset: u64,
    length: u32,
) -> WinResult<()> {
    let Some(api) = api() else {
        return unsupported();
    };
    (api.write_file_data)(ctx, stream_id, buffer, offset, length).ok()
}

/// # Safety
/// `buffer` must have come from [`allocate_aligned_buffer`] and not be freed twice.
pub unsafe fn free_aligned_buffer(buffer: *const c_void) {
    if let Some(api) = api() {
        (api.free_aligned_buffer)(buffer);
    }
}

/// ProjFS's on-disk state bits for one entry (`PRJ_FILE_STATE`).
///
/// The two that matter are `DIRTY_PLACEHOLDER` and `FULL`: both mean the file's contents no longer
/// come from us. A placeholder the user only *read* stays `HYDRATED_PLACEHOLDER` and is still ours
/// to reproduce from the archive, which is the distinction that decides whether a mount folder can
/// be deleted on unmount.
pub const FILE_STATE_DIRTY_PLACEHOLDER: u32 = 0x4;
pub const FILE_STATE_FULL: u32 = 0x8;
pub const FILE_STATE_TOMBSTONE: u32 = 0x10;

/// The ProjFS state of one path, or `None` when it cannot be determined -- an unreadable path, or a
/// Windows without the export. Callers must treat `None` as "assume it is the user's", since the
/// only action that depends on this is deletion.
pub fn on_disk_file_state(path: &std::path::Path) -> Option<u32> {
    let api = api()?;
    let f = api.get_on_disk_file_state?;
    let mut wide: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    wide.push(0);
    let mut state = 0u32;
    unsafe {
        f(PCWSTR(wide.as_ptr()), &mut state).ok().ok()?;
    }
    Some(state)
}

/// Whether anything under `root` is the user's rather than ours: a file they modified or created, or
/// an archive entry they deleted. Used to decide whether unmounting may delete the mount folder.
///
/// Errs towards "yes". Every caller uses this to guard a deletion, so an entry that cannot be
/// classified has to count as theirs.
pub fn holds_user_changes(root: &std::path::Path) -> bool {
    let Ok(rd) = std::fs::read_dir(root) else {
        return true;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        match on_disk_file_state(&path) {
            None => return true,
            Some(s)
                if s & (FILE_STATE_DIRTY_PLACEHOLDER | FILE_STATE_FULL | FILE_STATE_TOMBSTONE)
                    != 0 =>
            {
                return true
            }
            Some(_) => {}
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) && holds_user_changes(&path) {
            return true;
        }
    }
    false
}
