//! Adaptive parallelism: auto-detect hardware, calibrate codec throughput, and derive the
//! best settings per job, "detect coarse, measure fine, cache the result."
//!
//! Layers:
//!   1. [`HwProfile::detect`], static profile (cores, RAM, per-drive media/bus/physical-id).
//!   2. [`classify`], per-job bottleneck side from the archive's own header metadata.
//!   3. [`Governor`], runtime self-tuning from decode→writer queue occupancy.
//!   4. [`calibrate`], one-time micro-bench → this machine's real per-core codec rates;
//!      [`measure_write_wall`] measures the number no API exposes (gated: it writes to disk).
//!
//! [`derive_plan`] combines them into a [`Plan`] (workers, writers, pipeline shape, buffers, …).

use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::ptr;

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use lzma_rust2::{XzOptions, XzReader, XzWriter};

// Hardware detection, platform layer. Windows uses raw kernel32 FFI; Unix reads /proc + /sys +
// statvfs. Every probe degrades to a safe default on failure, so a wrong volume / missing
// permission / absent sysfs never panics, detection is only advisory input to the parallel planner.

#[cfg(windows)]
#[repr(C)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page: u64,
    avail_page: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_ext_virtual: u64,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct DiskExtent {
    disk_number: u32,
    starting_offset: i64,
    extent_length: i64,
}

#[cfg(windows)]
#[repr(C)]
struct VolumeDiskExtents {
    number_of_disk_extents: u32,
    extents: [DiskExtent; 16],
}

#[cfg(windows)]
#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional_parameters: [u8; 1],
}

#[cfg(windows)]
#[repr(C)]
struct DeviceSeekPenaltyDescriptor {
    version: u32,
    size: u32,
    incurs_seek_penalty: u8,
}

#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x1;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x2;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
#[cfg(windows)]
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002d_1400;
#[cfg(windows)]
const STORAGE_DEVICE_PROPERTY: u32 = 0;
#[cfg(windows)]
const STORAGE_SEEK_PENALTY_PROPERTY: u32 = 7;
#[cfg(windows)]
const PROPERTY_STANDARD_QUERY: u32 = 0;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        dir: *const u16,
        free_to_caller: *mut u64,
        total: *mut u64,
        free_total: *mut u64,
    ) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sec: *mut std::ffi::c_void,
        disposition: u32,
        flags: u32,
        template: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn DeviceIoControl(
        h: *mut std::ffi::c_void,
        code: u32,
        in_buf: *const std::ffi::c_void,
        in_size: u32,
        out_buf: *mut std::ffi::c_void,
        out_size: u32,
        returned: *mut u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
    fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
    fn GetVolumePathNameW(name: *const u16, out: *mut u16, len: u32) -> i32;
    fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32;
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
#[cfg(windows)]
fn is_bad(h: *mut std::ffi::c_void) -> bool {
    h.is_null() || h as isize == -1
}

// -------------------------------------------------------------------------------------------
// Unix (Linux-first) detection: RAM from /proc/meminfo, drive media/bus from /sys/block, free
// space via statvfs. Absent sysfs (containers, non-Linux unix) yields safe "unknown" defaults.
// -------------------------------------------------------------------------------------------
#[cfg(unix)]
mod unix_platform {
    use super::Bus;
    use std::path::Path;

    /// (total, available) physical RAM in bytes; (0, 0) when it can't be measured, which every caller
    /// already treats as "unknown" rather than "none".
    #[cfg(target_os = "linux")]
    pub(super) fn memory() -> (u64, u64) {
        match std::fs::read_to_string("/proc/meminfo") {
            Ok(t) => super::parse_meminfo(&t),
            Err(_) => (0, 0),
        }
    }

    /// macOS has no `/proc`; the equivalents are sysctls.
    ///
    /// `available` is free pages only, which **under**-reports on macOS because the kernel keeps most
    /// of RAM as cache that it would happily evict. That is the safe direction: the figure only caps
    /// the worker pool, so guessing low costs a little parallelism while guessing high risks
    /// over-committing memory. It is a real measurement rather than an invented headroom number.
    #[cfg(target_os = "macos")]
    pub(super) fn memory() -> (u64, u64) {
        let total = sysctl_u64(c"hw.memsize").unwrap_or(0);
        let page = sysctl_u64(c"hw.pagesize").unwrap_or(4096);
        let free = sysctl_u32(c"vm.page_free_count").unwrap_or(0) as u64;
        (total, free.saturating_mul(page))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn memory() -> (u64, u64) {
        (0, 0)
    }

    #[cfg(target_os = "macos")]
    fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
        let mut v: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: `v`/`len` are a correctly sized destination for a scalar sysctl; a non-zero return
        // leaves them untouched and yields None.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut v as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(v)
    }

    #[cfg(target_os = "macos")]
    fn sysctl_u32(name: &std::ffi::CStr) -> Option<u32> {
        let mut v: u32 = 0;
        let mut len = std::mem::size_of::<u32>();
        // SAFETY: as above, for a 32-bit scalar.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut v as *mut u32 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(v)
    }

    /// The whole-disk device backing `path`, packed major:minor into a u32. Two paths on the same
    /// physical disk return the same value (all `topology`/`detect` rely on); empty on any failure.
    #[cfg(target_os = "linux")]
    pub fn physical_drives_for_path(path: &str) -> Vec<u32> {
        match backing_disk(Path::new(path)) {
            Some((id, _)) => vec![id],
            None => Vec::new(),
        }
    }

    /// macOS: the filesystem's device id identifies the volume, which is what the scheduler actually
    /// groups by. Resolving further to a *physical* disk would need IOKit; the practical cost is only
    /// that two partitions of one disk are treated as two, which at worst reads them concurrently.
    ///
    /// This also primes the media cache, because the mount point needed to ask `diskutil` about the
    /// drive is in hand right here.
    #[cfg(target_os = "macos")]
    pub fn physical_drives_for_path(path: &str) -> Vec<u32> {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::metadata(path) else {
            return Vec::new();
        };
        let id = meta.dev() as u32;
        prime_media_cache(id, Path::new(path));
        vec![id]
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn physical_drives_for_path(_path: &str) -> Vec<u32> {
        Vec::new()
    }

    /// `Some(true)` = spinning disk (seek penalty); `Some(false)` = SSD; `None` = unknown. Read from
    /// /sys/block/<disk>/queue/rotational.
    #[cfg(target_os = "linux")]
    pub(super) fn drive_seek_penalty(id: u32) -> Option<bool> {
        let name = disk_name_for_id(id)?;
        let rot = std::fs::read_to_string(format!("/sys/block/{name}/queue/rotational")).ok()?;
        match rot.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        }
    }

    /// Bus class inferred from the kernel device name, enough to seed the write-ceiling prior.
    #[cfg(target_os = "linux")]
    pub(super) fn drive_bus_type(id: u32) -> Bus {
        match disk_name_for_id(id) {
            Some(n) if n.starts_with("nvme") => Bus::Nvme,
            Some(n) if n.starts_with("sd") => Bus::Sata, // SATA/SAS/USB all surface as sdX
            Some(_) => Bus::Other,
            None => Bus::Unknown,
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn drive_seek_penalty(id: u32) -> Option<bool> {
        media_of(id).and_then(|m| m.0)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn drive_bus_type(id: u32) -> Bus {
        media_of(id).map(|m| m.1).unwrap_or(Bus::Unknown)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn drive_seek_penalty(_id: u32) -> Option<bool> {
        None
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn drive_bus_type(_id: u32) -> Bus {
        Bus::Unknown
    }

    // ---- macOS media detection -------------------------------------------------------------------
    //
    // Whether a drive is spinning decides between one sequential reader and several parallel ones, and
    // getting it wrong on an external USB hard disk means seek thrash, exactly the case a big photo
    // collection lives on. macOS answers it through IOKit, which `diskutil` already wraps, so this
    // shells out **once per volume** and caches the result rather than binding IOKit. Any failure
    // leaves the entry unknown and the caller falls back to today's defaults.

    #[cfg(target_os = "macos")]
    type Media = (Option<bool>, Bus);

    #[cfg(target_os = "macos")]
    fn media_cache() -> &'static std::sync::Mutex<std::collections::HashMap<u32, Media>> {
        static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, Media>>> =
            std::sync::OnceLock::new();
        CACHE.get_or_init(Default::default)
    }

    #[cfg(target_os = "macos")]
    fn media_of(id: u32) -> Option<Media> {
        media_cache().lock().ok()?.get(&id).copied()
    }

    #[cfg(target_os = "macos")]
    fn prime_media_cache(id: u32, path: &Path) {
        if media_cache()
            .lock()
            .map(|c| c.contains_key(&id))
            .unwrap_or(true)
        {
            return; // already known (or the lock is poisoned, then just skip detection)
        }
        let media = mount_point(path)
            .and_then(|mp| query_diskutil(&mp))
            .unwrap_or((None, Bus::Unknown));
        if let Ok(mut c) = media_cache().lock() {
            c.insert(id, media);
        }
    }

    /// The mount point containing `path`, via `statfs`; `diskutil` accepts a mount point but not an
    /// arbitrary file inside it.
    #[cfg(target_os = "macos")]
    fn mount_point(path: &Path) -> Option<std::path::PathBuf> {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a valid zeroed statfs; read only after a success (0) return.
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let raw = st.f_mntonname;
        let bytes: Vec<u8> = raw
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as u8)
            .collect();
        Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(
            bytes,
        )))
    }

    /// Ask `diskutil` about a mount point and pull the two facts the planner needs out of its plist.
    /// Scanned as text rather than parsed as XML: the two keys are unambiguous, and a whole plist
    /// parser would be a dependency for six lines of work.
    #[cfg(target_os = "macos")]
    fn query_diskutil(mount: &Path) -> Option<Media> {
        let out = std::process::Command::new("diskutil")
            .arg("info")
            .arg("-plist")
            .arg(mount)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // `SolidState` is absent on some devices (notably disk images), absent stays unknown.
        let ssd = super::plist_bool(&text, "SolidState");
        let bus = match super::plist_string(&text, "BusProtocol").as_deref() {
            Some("PCI-Express") | Some("PCI") | Some("Apple Fabric") => Bus::Nvme,
            Some("SATA") | Some("SAS") => Bus::Sata,
            Some("USB") => Bus::Usb,
            Some(_) => Bus::Other,
            None => Bus::Unknown,
        };
        // A spinning disk reports SolidState=false; keep that as the seek-penalty answer.
        Some((ssd.map(|is_ssd| !is_ssd), bus))
    }

    /// Free space (MiB) available to a non-root user on the filesystem holding `dir`, via statvfs.
    pub fn free_space_mib(dir: &Path) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a valid zeroed statvfs; its fields are read only after a success (0) return.
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let bytes = (st.f_bavail as u64).saturating_mul(st.f_frsize as u64);
        Some(bytes / (1024 * 1024))
    }

    // ---- path -> backing whole-disk resolution via sysfs (Linux) ----

    /// Resolve `path` to its backing whole disk: stat the path for its device number, find the sysfs
    /// block node, and climb from a partition to its parent disk. Returns (packed id, kernel name).
    #[cfg(target_os = "linux")]
    fn backing_disk(path: &Path) -> Option<(u32, String)> {
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(path).ok()?.dev();
        let (maj, min) = (major(dev), minor(dev));
        let mut cur = std::fs::canonicalize(format!("/sys/dev/block/{maj}:{min}")).ok()?;
        if cur.join("partition").exists() {
            cur = cur.parent()?.to_path_buf();
        }
        let name = cur.file_name()?.to_str()?.to_string();
        let id = std::fs::read_to_string(cur.join("dev"))
            .ok()
            .and_then(|s| pack_dev(s.trim()))
            .unwrap_or_else(|| pack(maj, min));
        Some((id, name))
    }

    /// Recover a disk's kernel name from a packed id by scanning /sys/block.
    #[cfg(target_os = "linux")]
    fn disk_name_for_id(id: u32) -> Option<String> {
        for entry in std::fs::read_dir("/sys/block").ok()?.flatten() {
            if let Ok(dev) = std::fs::read_to_string(entry.path().join("dev")) {
                if pack_dev(dev.trim()) == Some(id) {
                    return entry.file_name().to_str().map(|s| s.to_string());
                }
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    fn pack(maj: u64, min: u64) -> u32 {
        (((maj & 0xfff) as u32) << 20) | ((min as u32) & 0x000f_ffff)
    }
    #[cfg(target_os = "linux")]
    fn pack_dev(s: &str) -> Option<u32> {
        let (maj, min) = s.split_once(':')?;
        Some(pack(maj.parse().ok()?, min.parse().ok()?))
    }
    // glibc/musl dev_t major/minor encoding.
    #[cfg(target_os = "linux")]
    fn major(dev: u64) -> u64 {
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfffu64)
    }
    #[cfg(target_os = "linux")]
    fn minor(dev: u64) -> u64 {
        (dev & 0xff) | ((dev >> 12) & !0xffu64)
    }
}

#[cfg(unix)]
use unix_platform::{drive_bus_type, drive_seek_penalty, memory};
#[cfg(unix)]
pub use unix_platform::{free_space_mib, physical_drives_for_path};

/// Scan an Apple plist for `<key>NAME</key><true/>` → `Some(true)`, `<false/>` → `Some(false)`,
/// absent → `None`.
///
/// Compiled everywhere, not just on macOS, so that this parsing is unit-tested on whatever machine
/// runs the tests, including ones that can never execute the macOS path. It is the error-prone part
/// of the drive probe, and what it returns decides sequential-versus-parallel reads.
#[cfg(any(target_os = "macos", test))]
fn plist_bool(text: &str, key: &str) -> Option<bool> {
    let rest = text.split_once(&format!("<key>{key}</key>"))?.1;
    let head = rest.trim_start();
    if head.starts_with("<true/>") {
        Some(true)
    } else if head.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

/// `<key>NAME</key><string>VALUE</string>` → `Some(VALUE)`. See [`plist_bool`] for why this is not
/// macOS-gated.
#[cfg(any(target_os = "macos", test))]
fn plist_string(text: &str, key: &str) -> Option<String> {
    let rest = text.split_once(&format!("<key>{key}</key>"))?.1;
    let open = rest.find("<string>")? + "<string>".len();
    let close = rest[open..].find("</string>")? + open;
    Some(rest[open..close].to_string())
}

/// Extract total/available physical RAM (bytes) from a `/proc/meminfo` body. Compiled under `test`
/// as well as on Linux so the parsing, which is the error-prone part, is unit-tested even on a host
/// that has no `/proc`. The `MemAvailable` field is the kernel's own estimate of allocatable RAM.
///
/// Gated on `linux` rather than `unix`: macOS is a unix but reads its memory figures from sysctls,
/// so a `unix` gate would compile this into the macOS build with no caller and trip `dead_code`.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(text: &str) -> (u64, u64) {
    fn kib(v: &str) -> u64 {
        v.split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .map(|k| k * 1024)
            .unwrap_or(0)
    }
    let (mut total, mut avail) = (0u64, 0u64);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = kib(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = kib(v);
        }
    }
    (total, avail)
}

// Static hardware profile

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bus {
    Nvme,
    Sata,
    Usb,
    Other,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub number: u32,
    pub ssd: Option<bool>, // None = couldn't determine
    pub bus: Bus,
}

impl DriveInfo {
    /// Best-guess sustained-write ceiling before a real measurement (see [`measure_write_wall`]).
    /// NOTE: DRAM-less / QLC SSDs run FAR below these priors post-SLC, that gap is exactly why
    /// the write probe exists.
    pub fn default_wall_mibs(&self) -> f64 {
        match self.ssd {
            Some(false) => 120.0, // HDD
            _ => match self.bus {
                Bus::Nvme => 500.0,
                Bus::Sata => 350.0,
                _ => 250.0,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct HwProfile {
    pub logical: usize,
    pub physical: usize,
    pub smt: bool,
    pub ram_total: u64,
    pub ram_avail: u64,
    /// Drive backing the current working directory (the usual dst).
    pub work_drive: Option<DriveInfo>,
}

impl HwProfile {
    /// Profile the machine, describing the drive backing `work` (the destination we're about to
    /// write to). Profiling the CWD instead, as plain `detect()` does; describes the wrong disk
    /// whenever the destination lives on another volume: extracting from C: onto a D: HDD would be
    /// planned as if D: were C:.
    pub fn detect_for(work: &Path) -> Self {
        let mut p = Self::detect();
        if let Some(d) = work
            .to_str()
            .map(physical_drives_for_path)
            .and_then(|v| v.into_iter().next())
            .map(drive_info)
        {
            p.work_drive = Some(d);
        }
        p
    }

    pub fn detect() -> Self {
        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let physical = num_cpus::get_physical().max(1);
        let (ram_total, ram_avail) = memory();
        let work_drive = std::env::current_dir()
            .ok()
            .and_then(|d| d.to_str().map(|s| s.to_string()))
            .map(|p| physical_drives_for_path(&p))
            .and_then(|v| v.into_iter().next())
            .map(drive_info);
        HwProfile {
            logical,
            physical,
            smt: logical > physical,
            ram_total,
            ram_avail,
            work_drive,
        }
    }
}

#[cfg(windows)]
fn memory() -> (u64, u64) {
    let mut ms: MemoryStatusEx = unsafe { std::mem::zeroed() };
    ms.length = std::mem::size_of::<MemoryStatusEx>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut ms) };
    if ok == 0 {
        (0, 0)
    } else {
        (ms.total_phys, ms.avail_phys)
    }
}

/// PhysicalDrive numbers backing a filesystem path (handles spanned/striped volumes).
#[cfg(windows)]
pub fn physical_drives_for_path(path: &str) -> Vec<u32> {
    // path -> mount point (e.g. "C:\")
    let wp = wide(path);
    let mut buf = [0u16; 260];
    if unsafe { GetVolumePathNameW(wp.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } == 0 {
        return Vec::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
    let mount = String::from_utf16_lossy(&buf[..end]);
    let vol = mount.trim_end_matches(['\\', '/']);
    if vol.is_empty() {
        return Vec::new();
    }
    let dev = format!(r"\\.\{}", vol);
    let wd = wide(&dev);
    let h = unsafe {
        CreateFileW(
            wd.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if is_bad(h) {
        return Vec::new();
    }
    let mut vde: VolumeDiskExtents = unsafe { std::mem::zeroed() };
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            ptr::null(),
            0,
            &mut vde as *mut _ as *mut c_void,
            std::mem::size_of::<VolumeDiskExtents>() as u32,
            &mut ret,
            ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return Vec::new();
    }
    let n = (vde.number_of_disk_extents as usize).min(16);
    (0..n).map(|i| vde.extents[i].disk_number).collect()
}

fn drive_info(number: u32) -> DriveInfo {
    DriveInfo {
        number,
        ssd: drive_seek_penalty(number).map(|p| !p),
        bus: drive_bus_type(number),
    }
}

/// `Some(true)` = incurs seek penalty (HDD); `Some(false)` = SSD; `None` = unknown.
#[cfg(windows)]
fn drive_seek_penalty(n: u32) -> Option<bool> {
    let dev = format!(r"\\.\PhysicalDrive{}", n);
    let wd = wide(&dev);
    let h = unsafe {
        CreateFileW(
            wd.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if is_bad(h) {
        return None;
    }
    let q = StoragePropertyQuery {
        property_id: STORAGE_SEEK_PENALTY_PROPERTY,
        query_type: PROPERTY_STANDARD_QUERY,
        additional_parameters: [0],
    };
    let mut desc: DeviceSeekPenaltyDescriptor = unsafe { std::mem::zeroed() };
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &q as *const _ as *const c_void,
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            &mut desc as *mut _ as *mut c_void,
            std::mem::size_of::<DeviceSeekPenaltyDescriptor>() as u32,
            &mut ret,
            ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(h) };
    if ok == 0 || ret == 0 {
        None
    } else {
        Some(desc.incurs_seek_penalty != 0)
    }
}

#[cfg(windows)]
fn drive_bus_type(n: u32) -> Bus {
    let dev = format!(r"\\.\PhysicalDrive{}", n);
    let wd = wide(&dev);
    let h = unsafe {
        CreateFileW(
            wd.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if is_bad(h) {
        return Bus::Unknown;
    }
    let q = StoragePropertyQuery {
        property_id: STORAGE_DEVICE_PROPERTY,
        query_type: PROPERTY_STANDARD_QUERY,
        additional_parameters: [0],
    };
    let mut out = [0u8; 512];
    let mut ret = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &q as *const _ as *const c_void,
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut ret,
            ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(h) };
    // STORAGE_DEVICE_DESCRIPTOR.BusType is a u32 at offset 28.
    if ok == 0 || ret < 32 {
        return Bus::Unknown;
    }
    let bus = u32::from_le_bytes([out[28], out[29], out[30], out[31]]);
    match bus {
        0x11 => Bus::Nvme, // BusTypeNvme
        0x0B => Bus::Sata, // BusTypeSata
        0x07 => Bus::Usb,  // BusTypeUsb
        0 => Bus::Unknown,
        _ => Bus::Other,
    }
}

/// Same physical drive for source and destination? (The decisive topology check.)
pub fn topology(src: &Path, dst: &Path) -> Topology {
    let s = src
        .to_str()
        .map(physical_drives_for_path)
        .unwrap_or_default();
    let d = dst
        .to_str()
        .map(physical_drives_for_path)
        .unwrap_or_default();
    match (s.first(), d.first()) {
        (Some(a), Some(b)) if a == b => Topology::SameDrive,
        (Some(_), Some(_)) => Topology::TwoDrive,
        _ => Topology::Unknown,
    }
}

// Calibration, measure this machine's real per-core codec throughput (light, in-memory)

#[derive(Clone, Copy, Debug, Default)]
pub struct Rates {
    pub deflate_enc: f64, // MiB/s per core
    pub deflate_dec: f64,
    pub lzma_dec: f64,
}

impl Rates {
    /// Decode MiB/s for a codec: measured where we have it, sane defaults otherwise.
    pub fn decode_rate(&self, c: Codec) -> f64 {
        match c {
            Codec::Store => 5000.0,
            Codec::Deflate => nonzero(self.deflate_dec, 488.0),
            Codec::Lzma => nonzero(self.lzma_dec, 75.0),
            Codec::Zstd => 1500.0,
            Codec::Lz4 => 2500.0,
            Codec::Bzip2 => 40.0,
            Codec::Brotli => 300.0,
        }
    }
}
fn nonzero(v: f64, d: f64) -> f64 {
    if v > 0.0 {
        v
    } else {
        d
    }
}

fn gen_text(bytes: usize) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "the",
        "quick",
        "brown",
        "archive",
        "compression",
        "parallel",
        "throughput",
        "stream",
        "deflate",
        "worker",
        "sector",
        "bandwidth",
        "offset",
        "extract",
        "decode",
        "pipeline",
        "and",
        "of",
        "to",
        "in",
        "a",
        "is",
        "on",
    ];
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut buf = Vec::with_capacity(bytes + 16);
    while buf.len() < bytes {
        let w = WORDS[(next() as usize) % WORDS.len()];
        buf.extend_from_slice(w.as_bytes());
        buf.push(if next() % 12 == 0 { b'\n' } else { b' ' });
    }
    buf.truncate(bytes);
    buf
}

/// Median of `f` over `n` runs after one discarded warm-up. A single timed sample of a codec on a
/// busy desktop is dominated by scheduling noise, turbo state and cold caches, and whatever comes
/// out of here is persisted permanently as this machine's "measured" rate, so it has to be a
/// number that survives a noisy desktop, not one lucky or unlucky run.
fn median_of<F: FnMut() -> f64>(n: usize, mut f: F) -> f64 {
    let _warmup = f();
    let mut v: Vec<f64> = (0..n.max(1)).map(|_| f()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

/// Single-threaded per-core rates for DEFLATE (enc+dec) and LZMA (dec). No disk.
pub fn calibrate(sample_mib: usize) -> Rates {
    let data = gen_text(sample_mib * 1024 * 1024);
    let mib = data.len() as f64 / (1024.0 * 1024.0);

    // DEFLATE encode (median of repeated runs)
    let mut deflated = Vec::new();
    let deflate_enc = median_of(3, || {
        let t = Instant::now();
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        enc.write_all(&data).unwrap();
        deflated = enc.finish().unwrap();
        mib / t.elapsed().as_secs_f64().max(1e-9)
    });

    // DEFLATE decode (median of repeated runs)
    let deflate_dec = median_of(3, || {
        let t = Instant::now();
        io::copy(
            &mut DeflateDecoder::new(deflated.as_slice()),
            &mut io::sink(),
        )
        .unwrap();
        mib / t.elapsed().as_secs_f64().max(1e-9)
    });

    // LZMA/XZ decode (compress a smaller slice at a fast preset just to get a stream)
    let slice = &data[..data.len().min(48 * 1024 * 1024)];
    let smib = slice.len() as f64 / (1024.0 * 1024.0);
    let mut w = XzWriter::new(Vec::new(), XzOptions::with_preset(3)).unwrap();
    w.write_all(slice).unwrap();
    let xz = w.finish().unwrap();
    let lzma_dec = median_of(3, || {
        let t = Instant::now();
        io::copy(&mut XzReader::new(xz.as_slice(), false), &mut io::sink()).unwrap();
        smib / t.elapsed().as_secs_f64().max(1e-9)
    });

    Rates {
        deflate_enc,
        deflate_dec,
        lzma_dec,
    }
}

// Bottleneck classification + settings derivation

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Codec {
    Store,
    Deflate,
    Lzma,
    Zstd,
    Lz4,
    Bzip2,
    Brotli,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Extract,
    Create,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bottleneck {
    WriteBound,
    CpuBound,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// Each worker reads+decodes+writes its own entry, the default. Parallel per-entry writers keep
    /// the SSD saturated where a single write stream underutilizes it, so they beat a one-writer
    /// pipeline on the drives we measured.
    PerEntry,
    /// N decoders → bounded queue → 1 dedicated sequential writer. Reserved / not emitted by
    /// default: the QLC "sequential is better" theory did not hold on the tested NVMe SSD.
    Pipeline,
    /// 1-2 workers, serialized (HDD, where parallel seeks thrash).
    Serial,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Topology {
    SameDrive,
    TwoDrive,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub bottleneck: Bottleneck,
    pub shape: Shape,
    pub workers: usize,
    pub writers: usize,
    pub read_buf: usize,
    pub write_buf: usize,
    pub queue_bytes: usize,
    pub preallocate: bool,
    /// Codec worker threads for the create path (all cores, but RAM-capped for LZMA/xz).
    pub codec_threads: usize,
    pub note: &'static str,
}

/// Which side of the wall are we on? Probes with `hw.physical` decoders (the natural start).
pub fn classify(
    op: Op,
    codec: Codec,
    blocks: usize,
    hw: &HwProfile,
    rates: &Rates,
    wall: f64,
) -> Bottleneck {
    if op == Op::Create {
        // Output ≪ input for anything compressible → CPU-bound; Store degenerates to a copy.
        return if codec == Codec::Store {
            Bottleneck::WriteBound
        } else {
            Bottleneck::CpuBound
        };
    }
    let parallel_units = blocks.max(1).min(hw.physical);
    let projected = parallel_units as f64 * rates.decode_rate(codec);
    if projected >= wall {
        Bottleneck::WriteBound
    } else {
        Bottleneck::CpuBound
    }
}

/// How many packs the `.cram` writer may have in flight at once, given the pack size it is writing.
///
/// This is the create path's memory knob, and the **only** one that may depend on the machine. Pack
/// size is a property of the archive and must not: an unencrypted `.cram` is guaranteed
/// byte-for-byte identical from the same inputs (see `tests/reproducible.rs`), which is what lets it
/// be content-addressed, checked against a published hash, and signed. Deriving pack size from RAM
/// would make the same folder compress differently on a laptop and a workstation, and would leave a
/// small machine with a permanently worse archive that copying to a big machine could never undo.
/// Batch has no such problem: it is invisible in the output, and was measured that way on the kernel
/// tree at 32 MiB packs, where batches of 16, 8 and 4 all produced byte-identical archives while
/// peak RSS fell 4952 -> 3251 -> 1734 MB.
///
/// The multiplier is empirical and deliberately conservative. A slot holds the raw pack waiting to
/// be sealed, the copy the background compressor is working through, the compressed output, and the
/// codec's own state; the three points above imply somewhere between 6.6x and 11.8x the pack size
/// per slot, and they do not fit a clean line (freed memory the allocator has not returned inflates
/// peak RSS at the top end). Taking the worst observed rather than fitting a curve is the honest
/// reading, so 12x, and half of available RAM as the budget.
pub fn create_batch(pack_bytes: usize, hw: &HwProfile) -> usize {
    const SLOT_MULTIPLE: usize = 12;
    // One slot per thread, and no more: a batch compresses under `into_par_iter`, so packs beyond
    // the thread count only queue, while packs short of it leave threads with nothing to do at all.
    //
    // This was a flat 16 until it was measured against the threads it was starving. On the kernel
    // tree at 32 MiB packs on 24 threads, `--best` produced a byte-identical archive in 51.8 s at
    // 16 and 42.2 s at 24 -- the eight idle threads were a fifth of the run. Peak RSS goes 5.1 ->
    // 8.5 GB with them, which is what the budget below is for; it predicted 9.2 GB, so it holds.
    //
    // Neither is the floor. A batch ends when its slowest pack does, and packs are equal in raw
    // size but not in compress time (6.9 s to 18.9 s across one batch here), so ~19% of the lanes'
    // time is still spent waiting at the barrier. Removing the barrier is a separate change.
    let ceiling = hw.logical.max(1);
    // `CRAM_BATCH` forces the value. This exists so the property the whole split rests on -- that
    // batch cannot change the archive -- is testable on one machine, since batch is otherwise a
    // function of installed RAM and there is no other way to vary it in a test.
    if let Some(n) = std::env::var_os("CRAM_BATCH")
        .and_then(|v| v.to_str()?.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n.min(64);
    }
    if hw.ram_avail == 0 {
        // Couldn't read memory. One slot per thread is only safe because the budget below vetoes
        // it on a small machine, and here there is no budget, so fall back to the old flat cap.
        return ceiling.min(16);
    }
    let budget = hw.ram_avail / 2;
    let per_slot = (pack_bytes * SLOT_MULTIPLE).max(1) as u64;
    ((budget / per_slot) as usize).clamp(1, ceiling)
}

/// Lanes for the `.cram` writer's entry pipeline: how many files are chunked at once, how many wait
/// behind them, and how far ahead of the commit stage one file may run.
#[derive(Debug, Clone, Copy)]
pub struct PrepareLanes {
    /// Threads running FastCDC, BLAKE3 and the Lepton pass.
    pub workers: usize,
    /// Files queued behind the workers, so a thread that finishes a small file never waits for the
    /// engine's loop to name the next one.
    pub depth: usize,
    /// Chunks one worker may produce before the commit stage has taken any of them. This is what
    /// lets a worker on a large file keep going instead of handing over a chunk at a time, and with
    /// it the only real memory in the pipeline.
    pub buffer: usize,
}

/// Size the entry pipeline for this machine.
///
/// Like [`create_batch`], every number here is invisible in the output: chunk boundaries are a
/// function of the file's bytes, and everything order-dependent happens on one thread in entry
/// order, so lanes cannot change the archive. `tests/chunk_lanes.rs` is what holds that down, and
/// the three env overrides exist so it can vary them on one machine.
///
/// **`packs_are_cheap` is the whole reason this is not simply one lane per thread.** The chunk
/// workers do not have the machine to themselves: the pack compressors are already on it, one per
/// thread, and they are the expensive half. Measured on the 1.9 GB kernel tree, 94 778 files, 24
/// threads, best of two:
///
/// | lanes | `--store` | default level |
/// |------:|----------:|--------------:|
/// | in-line (before this existed) | 4.08 s | 7.29 s |
/// | 2  |      -- | 7.32 s |
/// | 4  |  2.52 s | 7.54 s |
/// | 8  |  2.32 s | 7.76 s |
/// | 12 |  2.27 s | 7.75 s |
/// | 24 |  2.40 s | 7.85 s |
///
/// Two opposite curves. Under `--store` the pack pool has almost nothing to do, the cores are free,
/// and the chunkers are bound by the per-file `open`/`read` rather than by CPU, so going wide wins
/// 1.8x by twelve lanes. At the default level the pack pool needs 131 core-seconds of LZMA and owns
/// every core already; extra chunk lanes cannot add throughput and only preempt the compressors,
/// costing 6% by twelve. So the wide setting is spent only where the packs are known in advance to
/// be cheap, and everything else gets just enough lanes to keep the commit stage fed.
///
/// This leaves something on the table: a corpus that *attempts* compression and mostly fails --
/// media inside a mixed tree, where pack occupancy measured 30% -- has idle cores too and still gets
/// the narrow setting. Dividing the machine between the two pools at runtime, from the pack queue's
/// own occupancy, is the answer to that and is not this change.
///
/// The buffer budget is a quarter of available RAM, against the half [`create_batch`] already claims
/// for packs. Worst case is every in-flight file sitting on a full buffer of maximum-size chunks,
/// which is what this divides by; the typical case is a fraction of that, since chunks average
/// `CHUNK_AVG` and most files are smaller than one buffer.
pub fn prepare_lanes(max_chunk: usize, packs_are_cheap: bool, hw: &HwProfile) -> PrepareLanes {
    let env = |k: &str| {
        std::env::var_os(k)
            .and_then(|v| v.to_str()?.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
    };
    let logical = hw.logical.max(1);
    let lanes = if packs_are_cheap {
        (logical / 2).clamp(2, 16)
    } else {
        // One lane already chunks around 760 MB/s, which is well past what a compressing create
        // consumes, so this is "enough to keep the commit stage fed" and deliberately no more.
        (logical / 8).clamp(2, 6)
    };
    let workers = env("CRAM_CHUNK_WORKERS").unwrap_or(lanes).min(256);
    let depth = env("CRAM_CHUNK_DEPTH").unwrap_or(workers).min(1024);
    if let Some(buffer) = env("CRAM_CHUNK_BUFFER") {
        return PrepareLanes {
            workers,
            depth,
            buffer: buffer.min(4096),
        };
    }
    // Couldn't read memory: a modest fixed buffer rather than a computed one, since the computation
    // is the only thing keeping the product below in bounds.
    if hw.ram_avail == 0 {
        return PrepareLanes {
            workers,
            depth,
            buffer: 8,
        };
    }
    let budget = hw.ram_avail / 4;
    let in_flight = (workers + depth).max(1) as u64;
    let per_chunk = max_chunk.max(1) as u64;
    let buffer = (budget / (in_flight * per_chunk)) as usize;
    PrepareLanes {
        workers,
        depth,
        buffer: buffer.clamp(1, 64),
    }
}

/// Approx. per-thread compressor memory (MiB), the LZMA/xz RAM trap. zstd is far cheaper.
fn codec_mem_per_thread_mib(codec: Codec) -> f64 {
    match codec {
        Codec::Lzma => 2400.0, // xz/LZMA2 level ~9: ~2.4 GiB/thread → -T0 -9 blows up RAM
        Codec::Zstd => 256.0,
        _ => 128.0,
    }
}

const MIB: usize = 1024 * 1024;

/// The core of the engine: turn (op, codec, layout, hardware, topology) into concrete settings.
pub fn derive_plan(
    op: Op,
    codec: Codec,
    blocks: usize,
    hw: &HwProfile,
    topo: Topology,
    rates: &Rates,
    wall: f64,
) -> Plan {
    let is_hdd = matches!(hw.work_drive.as_ref().and_then(|d| d.ssd), Some(false));
    if is_hdd && op == Op::Extract {
        return Plan {
            bottleneck: Bottleneck::WriteBound,
            shape: Shape::Serial,
            workers: 2,
            writers: 1,
            read_buf: 8 * MIB,
            write_buf: 8 * MIB,
            queue_bytes: 16 * MIB,
            preallocate: true,
            codec_threads: 1,
            note: "HDD: serialize, parallel seeks thrash a spinning disk",
        };
    }

    match op {
        Op::Create => {
            let ram_cap = if hw.ram_avail > 0 {
                ((hw.ram_avail as f64 * 0.6) / (codec_mem_per_thread_mib(codec) * MIB as f64 / 1.0))
                    .max(1.0) as usize
            } else {
                hw.logical
            };
            // zstd-MT scales past physical; xz is RAM-capped; fast paths saturate ~physical.
            let codec_threads = hw.logical.min(ram_cap).max(1);
            Plan {
                bottleneck: Bottleneck::CpuBound,
                shape: Shape::PerEntry,
                workers: hw.logical,
                writers: 1,
                read_buf: 8 * MIB,
                write_buf: 8 * MIB,
                queue_bytes: hw.logical * 8 * MIB,
                preallocate: false,
                codec_threads,
                note: if codec == Codec::Lzma {
                    "create: all cores; LZMA/xz threads capped by RAM (avoids the -T0 -9 blowup)"
                } else {
                    "create: all cores, codec-MT, store-the-incompressible"
                },
            }
        }
        Op::Extract => {
            let bottleneck = classify(op, codec, blocks, hw, rates, wall);
            match (bottleneck, topo) {
                (Bottleneck::WriteBound, Topology::TwoDrive) => Plan {
                    bottleneck,
                    shape: Shape::PerEntry,
                    // dst drive isn't serving reads; ~physical-core writers measured as the SSD
                    // write peak (more than that was slower).
                    workers: hw.physical,
                    writers: hw.physical,
                    read_buf: 8 * MIB,
                    write_buf: 8 * MIB,
                    queue_bytes: hw.physical * 8 * MIB,
                    preallocate: true,
                    codec_threads: 1,
                    note: "2nd drive: reads free → parallel per-entry writers (~physical cores)",
                },
                (Bottleneck::WriteBound, _) => {
                    // Single drive: parallel per-entry writers. A lone sequential writer measured
                    // slower than parallel on the tested NVMe SSD (one write stream underutilizes the
                    // drive), so the pipeline is NOT built. ~physical*3/4 keeps some headroom for the
                    // competing reads on the shared drive; governor tunes.
                    let workers = ((hw.physical * 3) / 4).clamp(4, 8);
                    Plan {
                        bottleneck,
                        shape: Shape::PerEntry,
                        workers,
                        writers: workers,
                        read_buf: 8 * MIB,
                        write_buf: 8 * MIB,
                        queue_bytes: workers * 8 * MIB,
                        preallocate: true,
                        codec_threads: 1,
                        note: "write-bound: parallel per-entry writers (beats a 1-writer pipeline)",
                    }
                }
                (Bottleneck::CpuBound, _) => {
                    // Decode is the wall → ride cores (bounded by independent blocks).
                    let workers = blocks
                        .max(1)
                        .min(hw.logical)
                        .max(hw.physical.min(blocks.max(1)));
                    Plan {
                        bottleneck,
                        shape: Shape::PerEntry,
                        workers: workers.max(1),
                        writers: workers.max(1),
                        read_buf: 8 * MIB,
                        write_buf: 8 * MIB,
                        queue_bytes: workers.max(1) * 8 * MIB,
                        preallocate: true,
                        codec_threads: 1,
                        note: "cpu-bound decode: parallelize across independent blocks",
                    }
                }
            }
        }
    }
}

// Runtime governor, the free feedback loop that self-corrects worker count from queue fill.

/// EWMA of decode→writer queue occupancy with hysteresis. Full queue ⇒ writer is the wall ⇒
/// shed a worker; starved ⇒ decode is the wall ⇒ add one. Corrects any static mis-estimate
/// and tracks the SLC-cache cliff mid-job, no hardware database required.
pub struct Governor {
    fill: f64,
    alpha: f64,
    hi: f64,
    lo: f64,
    wmin: usize,
    wmax: usize,
    streak_hi: u32,
    streak_lo: u32,
    hold: u32,
}

impl Governor {
    pub fn new(wmin: usize, wmax: usize) -> Self {
        Governor {
            fill: 0.5,
            alpha: 0.3,
            hi: 0.8,
            lo: 0.2,
            wmin,
            wmax,
            streak_hi: 0,
            streak_lo: 0,
            hold: 3,
        }
    }

    /// Feed one sample (current queue length / capacity, current worker count); returns the
    /// adjusted worker count.
    pub fn update(&mut self, queue_len: usize, queue_cap: usize, workers: usize) -> usize {
        let sample = if queue_cap == 0 {
            0.5
        } else {
            queue_len as f64 / queue_cap as f64
        };
        self.fill = self.alpha * sample + (1.0 - self.alpha) * self.fill;
        let mut w = workers;
        if self.fill > self.hi {
            self.streak_hi += 1;
            self.streak_lo = 0;
            if self.streak_hi >= self.hold && w > self.wmin {
                w -= 1;
                self.streak_hi = 0;
            }
        } else if self.fill < self.lo {
            self.streak_lo += 1;
            self.streak_hi = 0;
            if self.streak_lo >= self.hold && w < self.wmax {
                w += 1;
                self.streak_lo = 0;
            }
        } else {
            self.streak_hi = 0;
            self.streak_lo = 0;
        }
        w
    }

    pub fn fill(&self) -> f64 {
        self.fill
    }
}

// Write-wall probe, measures the number no API exposes. GATED: it writes to disk.

#[derive(Clone, Copy, Debug)]
pub struct WriteWall {
    pub burst_mibs: f64,
    pub sustained_mibs: f64,
    pub cliff_mib: Option<f64>, // bytes written when throughput first stepped down (SLC size)
}

/// Sequentially write `total_mib` MiB of 8 MiB blocks to a temp file under `dir`, sampling
/// throughput to find the SLC-cache cliff and the sustained floor. Deletes the temp file after.
/// Heavy: call only with the user's go-ahead.
pub fn measure_write_wall(dir: &Path, cap_mib: usize) -> io::Result<WriteWall> {
    // Sampling window, adapted to the probe length. A fixed 512 MiB window would close NO window at
    // all on any probe shorter than that and report zero throughput, a short automatic probe would
    // measure nothing while the caller silently fell back to a guess. A quarter of the cap gives a
    // short probe several samples, while a long probe keeps the full 512 MiB granularity.
    let win_mib: usize = (cap_mib / 4).clamp(32, 512);
    let path = dir.join(".cram_writeprobe.tmp");
    let mut f = std::fs::File::create(&path)?;
    let block = vec![0xA5u8; 8 * MIB];
    let mut windows: Vec<f64> = Vec::new();
    let mut peak = 0.0f64;
    let mut cliff_mib: Option<f64> = None;
    let mut cliff_idx: Option<usize> = None;
    let mut written_mib = 0usize;
    let mut win_bytes = 0usize;
    let mut win_start = Instant::now();
    // Run the write loop through an inner closure so EVERY exit, including the error path; falls
    // through to the temp-file cleanup below. ENOSPC is this probe's *expected* failure mode (it
    // writes toward the cap), and erroring straight out would leave a multi-GiB
    // `.cram_writeprobe.tmp` permanently occupying the space of an already-full drive: the worst
    // possible outcome for a calibration.
    let run = (|| -> io::Result<()> {
        while written_mib < cap_mib {
            f.write_all(&block)?;
            win_bytes += block.len();
            written_mib += 8;
            if win_bytes >= win_mib * MIB {
                let _ = f.sync_data(); // force to media, not the OS cache
                let secs = win_start.elapsed().as_secs_f64().max(1e-9);
                let mibs = (win_bytes as f64 / MIB as f64) / secs;
                windows.push(mibs);
                if mibs > peak {
                    peak = mibs;
                }
                // First sustained >30% drop from peak = the SLC cliff.
                if cliff_mib.is_none() && windows.len() >= 3 && mibs < peak * 0.7 {
                    cliff_mib = Some(written_mib as f64);
                    cliff_idx = Some(windows.len() - 1);
                }
                if let Some(ci) = cliff_idx {
                    if (windows.len() - ci) * win_mib >= 16 * 1024 {
                        break; // ~16 GiB past the cliff nails the QLC sustained floor
                    }
                }
                win_bytes = 0;
                win_start = Instant::now();
            }
        }
        Ok(())
    })();
    let _ = f.sync_data();
    drop(f);
    let _ = std::fs::remove_file(&path);
    run?;
    // No full window recorded (cap_mib < one 512 MiB window): fail cleanly rather than compute a
    // statistic over nothing. Every summary below indexes back from `windows.len()`, which
    // underflows on an empty vec and panics.
    if windows.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write probe too small to measure (needs at least 512 MiB)",
        ));
    }
    let mean = |s: &[f64]| {
        if s.is_empty() {
            0.0
        } else {
            s.iter().sum::<f64>() / s.len() as f64
        }
    };
    let sustained = match cliff_idx {
        // Past a detected SLC cliff, the post-cliff mean IS the sustained floor.
        Some(ci) if windows.len() > ci + 1 => mean(&windows[ci..]),
        // No cliff seen. A "mean of the last third" degenerates to a single window on a short
        // probe, so its answer swings run-to-run: the same NVMe measures 1005 MiB/s on one run and
        // 356 on the next as write-back cache comes and goes. A median over all windows discards
        // both the cache-absorbed opener and a one-off stall.
        _ => {
            let mut w = windows.clone();
            w.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            w[w.len() / 2]
        }
    };
    Ok(WriteWall {
        burst_mibs: peak,
        sustained_mibs: sustained,
        cliff_mib,
    })
}

// Persistence, cache the profile so calibration is a one-time cost.

/// Free space available to this user on the volume containing `dir`, in MiB. Used to refuse a write
/// probe on a drive that can't spare the room, a calibration must never be what fills someone's disk.
#[cfg(windows)]
pub fn free_space_mib(dir: &Path) -> Option<u64> {
    let w = wide(dir.to_str()?);
    let mut free_to_caller: u64 = 0;
    let (mut total, mut free_total) = (0u64, 0u64);
    let ok = unsafe {
        GetDiskFreeSpaceExW(w.as_ptr(), &mut free_to_caller, &mut total, &mut free_total)
    };
    (ok != 0).then_some(free_to_caller / (1024 * 1024))
}

#[cfg(windows)]
pub fn profile_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|a| Path::new(&a).join("cram").join("profile.toml"))
}

/// Per-user profile location on Unix: `$XDG_CONFIG_HOME/cram/profile.toml`, falling back to
/// `~/.config/cram/profile.toml`, the standard spot for a CLI tool's cached state.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn profile_path() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
        .map(|base| base.join("cram").join("profile.toml"))
}

/// macOS keeps per-application state under `~/Library/Application Support`, not in an XDG directory.
/// `XDG_CONFIG_HOME` is still honoured first for anyone who sets it.
#[cfg(target_os = "macos")]
pub fn profile_path() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| Path::new(&h).join("Library").join("Application Support"))
        })
        .map(|base| base.join("cram").join("profile.toml"))
}

/// Bump when the profile's meaning changes, so an old file is re-measured instead of misread.
pub const PROFILE_SCHEMA: u32 = 2;

/// Identity of the machine a profile was measured on: core counts plus the work drive's media and
/// bus. Without this stamp a profile copied between machines, or a *roaming* profile following the
/// user onto different hardware, which is where this one lives; is indistinguishable from one
/// measured locally, so another machine's numbers would be trusted as this machine's.
pub fn machine_id() -> String {
    let hw = HwProfile::detect();
    let d = hw
        .work_drive
        .as_ref()
        .map(|d| {
            let media = match d.ssd {
                Some(true) => "ssd",
                Some(false) => "hdd",
                None => "unk",
            };
            format!("{media}-{:?}", d.bus)
        })
        .unwrap_or_else(|| "nodrive".to_string());
    format!("{}c{}p-{}", hw.logical, hw.physical, d)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persist calibrated rates. `wall` must be a **measured** sustained ceiling from
/// [`measure_write_wall`], pass `None` if it was never measured. Writing a bus-table guess here
/// would launder an estimate into a number everything downstream treats as measured.
pub fn save_profile(rates: &Rates, wall: Option<f64>) -> io::Result<()> {
    if let Some(p) = profile_path() {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut body = format!(
            "# Cram calibrated hardware profile\nschema = {}\nsaved_unix = {}\nmachine = \"{}\"\ndeflate_enc_mibs = {:.1}\ndeflate_dec_mibs = {:.1}\nlzma_dec_mibs = {:.1}\n",
            PROFILE_SCHEMA,
            unix_now(),
            machine_id(),
            rates.deflate_enc,
            rates.deflate_dec,
            rates.lzma_dec
        );
        // Only a real measurement is recorded at all; absence means "not measured", never 0.
        if let Some(w) = wall.filter(|w| *w > 0.0) {
            body.push_str(&format!(
                "write_wall_mibs = {w:.1}\nwrite_wall_measured = 1\n"
            ));
        }
        std::fs::write(p, body)?;
    }
    Ok(())
}

/// Parse a profile file body into (rates, wall). Blank and `#` comment lines are skipped,
/// crucially WITHOUT early-returning, so one comment line can't discard the whole profile.
fn parse_profile(text: &str) -> (Rates, f64) {
    let mut r = Rates::default();
    let mut wall = 0.0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let mut it = line.splitn(2, '=');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim();
        if let Ok(val) = v.parse::<f64>() {
            match k {
                "deflate_enc_mibs" => r.deflate_enc = val,
                "deflate_dec_mibs" => r.deflate_dec = val,
                "lzma_dec_mibs" => r.lzma_dec = val,
                "write_wall_mibs" => wall = val,
                _ => {}
            }
        }
    }
    (r, wall)
}

/// Load cached rates (+ measured wall, 0.0 when never measured) so we can skip re-calibration.
/// Returns `None`, forcing a fresh calibration; when the profile was written by an older schema
/// or measured on different hardware.
pub fn load_profile() -> Option<(Rates, f64)> {
    let p = profile_path()?;
    let text = std::fs::read_to_string(p).ok()?;
    if !profile_applies(&text) {
        return None;
    }
    Some(parse_profile(&text))
}

/// Is this profile for this schema and this machine? A pre-schema file (v1) has neither key and is
/// rejected, which is correct: it may carry a laundered guess as if it were measured.
fn profile_applies(text: &str) -> bool {
    let mut schema = None;
    let mut machine = None;
    for line in text.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "schema" => schema = v.trim().parse::<u32>().ok(),
            "machine" => machine = Some(v.trim().trim_matches('"').to_string()),
            _ => {}
        }
    }
    schema == Some(PROFILE_SCHEMA) && machine.as_deref() == Some(machine_id().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The macOS drive probe decides one sequential reader vs several parallel ones, and getting it
    /// wrong on an external USB hard disk means seek thrash. The probe itself can only run on macOS,
    /// but its parsing is pure string work and is checked here on every platform.
    #[test]
    fn diskutil_plist_yields_media_and_bus() {
        // Shape of a real `diskutil info -plist` reply, trimmed to the two keys that are read.
        let spinning_usb = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>BusProtocol</key><string>USB</string>
  <key>DeviceIdentifier</key><string>disk4s2</string>
  <key>SolidState</key><false/>
  <key>VolumeName</key><string>Family Archive</string>
</dict></plist>"#;
        assert_eq!(plist_bool(spinning_usb, "SolidState"), Some(false));
        assert_eq!(
            plist_string(spinning_usb, "BusProtocol").as_deref(),
            Some("USB")
        );

        let internal_ssd = r#"<plist><dict>
  <key>BusProtocol</key><string>Apple Fabric</string>
  <key>SolidState</key><true/>
</dict></plist>"#;
        assert_eq!(plist_bool(internal_ssd, "SolidState"), Some(true));
        assert_eq!(
            plist_string(internal_ssd, "BusProtocol").as_deref(),
            Some("Apple Fabric")
        );

        // Absent keys must read as unknown, never as a confident wrong answer; a disk image reports
        // no SolidState at all, and guessing "SSD" there would pick parallel reads on unknown media.
        let no_media_key =
            r#"<plist><dict><key>VolumeName</key><string>Backup</string></dict></plist>"#;
        assert_eq!(plist_bool(no_media_key, "SolidState"), None);
        assert_eq!(plist_string(no_media_key, "BusProtocol"), None);
    }

    #[test]
    fn meminfo_parses_total_and_available() {
        // A realistic /proc/meminfo head: values in kB, whitespace-padded, other fields interleaved.
        let sample = "MemTotal:       16384000 kB\nMemFree:          512000 kB\nMemAvailable:    8192000 kB\nBuffers:           40000 kB\n";
        let (total, avail) = parse_meminfo(sample);
        assert_eq!(total, 16_384_000 * 1024);
        assert_eq!(avail, 8_192_000 * 1024);
        // Missing fields degrade to 0, never a parse panic.
        assert_eq!(parse_meminfo("Nonsense: xyz\n"), (0, 0));
    }

    #[test]
    fn profile_parse_survives_comment_and_blank_lines() {
        let text = "# Cram calibrated hardware profile\n\ndeflate_enc_mibs = 17.0\ndeflate_dec_mibs = 574.0\nlzma_dec_mibs = 103.0\nwrite_wall_mibs = 191.0\n";
        let (r, wall) = parse_profile(text);
        assert_eq!(wall, 191.0);
        assert_eq!(r.deflate_dec, 574.0);
        assert_eq!(r.lzma_dec, 103.0);
    }

    #[test]
    fn governor_sheds_when_queue_full() {
        let mut g = Governor::new(4, 8);
        let mut w = 6;
        for _ in 0..10 {
            w = g.update(100, 100, w); // full
        }
        assert!(w < 6, "should shed workers when the writer is the wall");
    }

    #[test]
    fn governor_grows_when_starved() {
        let mut g = Governor::new(4, 12);
        let mut w = 6;
        for _ in 0..10 {
            w = g.update(0, 100, w); // starved
        }
        assert!(w > 6, "should add workers when decode is the wall");
    }

    #[test]
    fn low_ratio_zip_is_write_bound() {
        let hw = HwProfile {
            logical: 16,
            physical: 8,
            smt: true,
            ram_total: 16 << 30,
            ram_avail: 8 << 30,
            work_drive: None,
        };
        let rates = Rates {
            deflate_enc: 15.0,
            deflate_dec: 488.0,
            lzma_dec: 75.0,
        };
        // Big many-entry ZIP, single QLC drive at a 194 wall → write-bound → Pipeline.
        let p = derive_plan(
            Op::Extract,
            Codec::Deflate,
            2000,
            &hw,
            Topology::SameDrive,
            &rates,
            194.0,
        );
        assert_eq!(p.bottleneck, Bottleneck::WriteBound);
        // Verdict: parallel per-entry, NOT a single-writer pipeline.
        assert_eq!(p.shape, Shape::PerEntry);
        assert!(p.workers >= 4 && p.workers <= 8);
    }

    #[test]
    fn solid_lzma_is_cpu_bound() {
        let hw = HwProfile {
            logical: 16,
            physical: 8,
            smt: true,
            ram_total: 16 << 30,
            ram_avail: 8 << 30,
            work_drive: None,
        };
        let rates = Rates {
            deflate_enc: 15.0,
            deflate_dec: 488.0,
            lzma_dec: 75.0,
        };
        // One solid LZMA block (75 MiB/s < 194 wall) → CPU-bound.
        let p = derive_plan(
            Op::Extract,
            Codec::Lzma,
            1,
            &hw,
            Topology::SameDrive,
            &rates,
            194.0,
        );
        assert_eq!(p.bottleneck, Bottleneck::CpuBound);
    }
}
