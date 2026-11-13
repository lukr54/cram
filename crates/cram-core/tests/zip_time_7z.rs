//! Interop for ZIP modification times: 7-Zip must show a Cram-made entry at the source file's local
//! time, for archives from both writer paths.
//!
//! Before the fix Cram wrote UTC into the DOS date-time and 7-Zip, which reads that field as local
//! time like every archiver, showed each entry shifted by the zone offset. Cram now writes local time
//! there and adds the `0x5455` extended timestamp (UTC seconds). This test checks both: once as
//! written, and once with `0x5455` renamed to an unknown ID so 7-Zip has only the DOS fields.
//!
//! The expected time is 7-Zip's own reading of the source file, through a ZIP 7-Zip makes itself, so
//! the test holds in any zone without computing an offset.
//!
//! It is **self-skipping** like `interop_realworld.rs`: without 7-Zip installed it does nothing.
//! `CRAM_ZIP_SEQUENTIAL` is process-wide, so the one test that sets it holds [`ENV_LOCK`].

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use cram_core::engine;
use cram_core::format::Format;
use cram_core::progress::NullSink;
use cram_core::writer::CreateOptions;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn seven_zip() -> Option<PathBuf> {
    [
        "C:/Program Files/7-Zip/7z.exe",
        "C:/Program Files (x86)/7-Zip/7z.exe",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

/// Run 7-Zip, returning stdout on a clean exit and panicking with its output otherwise.
fn run_7z(exe: &Path, args: &[&str], cwd: &Path) -> String {
    let out = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("7z did not start");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "7z {args:?} exited {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// The `Modified` value 7-Zip lists for the single entry in `archive`, to the second.
fn listed_mtime(exe: &Path, archive: &Path) -> String {
    let listing = run_7z(
        exe,
        &["l", "-slt", archive.to_str().unwrap()],
        Path::new("."),
    );
    // The archive's own properties come first; entries follow the dashed separator.
    let entries = listing
        .split_once("----------")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("no entry block in:\n{listing}"));
    let values: Vec<&str> = entries
        .lines()
        .filter_map(|l| l.trim_end().strip_prefix("Modified = "))
        .collect();
    assert_eq!(values.len(), 1, "expected one entry in:\n{listing}");
    let v = values[0];
    assert!(v.len() >= 19, "short Modified value {v:?}");
    v[..19].to_string()
}

fn le16(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn le32(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// Rename every `0x5455` field in the extra-field block at `start..start + len` to an unassigned ID,
/// so readers skip it. Returns how many were renamed.
fn rename_ut(zip: &mut [u8], start: usize, len: usize) -> usize {
    let (mut i, end, mut renamed) = (start, start + len, 0);
    while i + 4 <= end {
        let size = le16(zip, i + 2);
        if le16(zip, i) == 0x5455 {
            zip[i..i + 2].copy_from_slice(&0x7A7Au16.to_le_bytes());
            renamed += 1;
        }
        i += 4 + size;
    }
    renamed
}

/// Hide the extended timestamps of an archive Cram wrote (no ZIP64, no comment), local and central.
fn hide_ext_timestamps(zip: &mut [u8]) -> usize {
    let eocd = zip
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .expect("end of central directory");
    let count = le16(zip, eocd + 10);
    let mut p = le32(zip, eocd + 16);
    let mut renamed = 0;
    for _ in 0..count {
        assert_eq!(&zip[p..p + 4], b"PK\x01\x02", "central header at {p}");
        let (n, m, k) = (le16(zip, p + 28), le16(zip, p + 30), le16(zip, p + 32));
        renamed += rename_ut(zip, p + 46 + n, m);
        let q = le32(zip, p + 42);
        assert_eq!(&zip[q..q + 4], b"PK\x03\x04", "local header at {q}");
        let (ln, lm) = (le16(zip, q + 26), le16(zip, q + 28));
        renamed += rename_ut(zip, q + 30 + ln, lm);
        p += 46 + n + m + k;
    }
    renamed
}

#[test]
fn seven_zip_shows_the_local_mtime_for_both_writer_paths() {
    let Some(exe) = seven_zip() else {
        eprintln!("skip: 7z.exe not found, ZIP time interop not checked here");
        return;
    };
    let root = std::env::temp_dir().join(format!("cram-zip7ztime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let file = src.join("stamp.txt");
    std::fs::write(
        &file,
        b"a file whose time 7-Zip must show as local\n".repeat(20),
    )
    .unwrap();

    // Two seconds ago at most, on an even second with no fraction, so the DOS field (2-second
    // steps), 0x5455 (whole seconds) and NTFS (100 ns) all hold the same time. 7-Zip converts a UTC
    // time for display with the offset in force now, while the DOS field holds the offset in force
    // at the stamp, so the two daylight-saving states must match: with a stamp an hour back, the
    // test failed for the hour after each change. Writing the file's own time rather than the
    // current one is checked by the library tests, on fixed dates.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let stamp = i64::try_from((now - 2) & !1).unwrap();
    filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(stamp, 0)).unwrap();

    // What 7-Zip shows for the source file through an archive of its own making.
    let oracle = root.join("oracle.zip");
    run_7z(
        &exe,
        &[
            "a",
            "-tzip",
            "-bso0",
            "-bsp0",
            oracle.to_str().unwrap(),
            "stamp.txt",
        ],
        &src,
    );
    let want = listed_mtime(&exe, &oracle);

    let cram_zip = |name: &str, sequential: bool| -> PathBuf {
        let out = root.join(name);
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        if sequential {
            std::env::set_var("CRAM_ZIP_SEQUENTIAL", "1");
        }
        let made = engine::create::create(
            &out,
            Format::zip(),
            std::slice::from_ref(&file),
            CreateOptions::default(),
            &NullSink,
        );
        if sequential {
            std::env::remove_var("CRAM_ZIP_SEQUENTIAL");
        }
        made.unwrap_or_else(|e| panic!("create {name}: {e}"));
        out
    };

    for (label, archive) in [
        ("parallel", cram_zip("parallel.zip", false)),
        ("sequential", cram_zip("sequential.zip", true)),
    ] {
        run_7z(
            &exe,
            &["t", "-bso0", "-bsp0", archive.to_str().unwrap()],
            &root,
        );
        assert_eq!(
            listed_mtime(&exe, &archive),
            want,
            "{label}: 7-Zip shows a different time for the Cram entry than for the source file"
        );

        // The same archive with 0x5455 hidden: 7-Zip has only the DOS fields left.
        let mut bytes = std::fs::read(&archive).unwrap();
        assert_eq!(
            hide_ext_timestamps(&mut bytes),
            2,
            "{label}: expected 0x5455 in the local and the central header"
        );
        let dos_only = root.join(format!("{label}-dos-only.zip"));
        std::fs::write(&dos_only, &bytes).unwrap();
        run_7z(
            &exe,
            &["t", "-bso0", "-bsp0", dos_only.to_str().unwrap()],
            &root,
        );
        assert_eq!(
            listed_mtime(&exe, &dos_only),
            want,
            "{label}: with only the DOS fields, 7-Zip shows a time other than the source's local time"
        );
        eprintln!("{label}: 7-Zip shows {want} with and without 0x5455");
    }

    let _ = std::fs::remove_dir_all(&root);
}
