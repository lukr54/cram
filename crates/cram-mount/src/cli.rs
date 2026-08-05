//! The `mount` command-line, called by the unified `cram` binary. `args[0]` is treated as the program
//! name (ignored) and the mount parameters are read from `args[1..]`, the same shape the former
//! standalone `cram-mount` binary parsed, so behavior is unchanged.
//!
//! `cram mount [--selftest] [-p <pw>] <archive> <mount-dir>`, mount an archive as a virtual folder via
//! ProjFS. The format is sniffed; every readable container mounts: the natively-seekable ones (`.cram`,
//! ZIP, ISO 9660) serve ranges straight from disk, and the sequential ones (tar/7z/rar/raw) are decoded
//! once into a bounded in-memory cache and served from there.
//!
//! Without `--selftest`: mounts and waits for Enter (browse it in Explorer, then press Enter to
//! unmount). With `--selftest`: mounts, walks + reads the whole virtual tree back through ProjFS,
//! prints what it found, and unmounts; a self-contained end-to-end check.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use cram_core::secret::{FixedPassword, NoPassword, PasswordProvider, Secret};

/// Run the mount CLI. `args[0]` is the program name (ignored); parameters are read from `args[1..]`.
pub fn main(args: &[String]) -> ExitCode {
    let selftest = args.iter().any(|a| a == "--selftest");
    let pw: Arc<dyn PasswordProvider> = match args.iter().position(|a| a == "-p") {
        Some(i) => Arc::new(FixedPassword(Secret::new(
            args.get(i + 1).cloned().unwrap_or_default(),
        ))),
        None => Arc::new(NoPassword),
    };
    let pos: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| *i > 0 && !a.starts_with("--") && a.as_str() != "-p")
        .filter(|(i, _)| args.get(i.wrapping_sub(1)).map(|s| s.as_str()) != Some("-p"))
        .map(|(_, a)| a)
        .collect();
    let (Some(archive), Some(root)) = (pos.first(), pos.get(1)) else {
        eprintln!("usage: cram mount [--selftest] [-p <pw>] <archive> <mount-dir>");
        return ExitCode::from(2);
    };
    let archive = PathBuf::from(archive);
    let root = PathBuf::from(root);

    let m = match crate::mount(&archive, &root, pw) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mount failed: {e}");
            return ExitCode::from(1);
        }
    };
    println!("mounted {} at {}", archive.display(), m.root().display());

    if selftest {
        let (files, dirs, bytes) = walk_verify(&root);
        println!("selftest: {dirs} dirs, {files} files, {bytes} bytes read back through ProjFS");
    } else if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!("browse it in Explorer; press Enter to unmount...");
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).ok();
    } else {
        // No console to press Enter at: a shortcut, a scheduled task, a service, `cram mount`
        // inside a script. Reading a line from a closed stdin returns EOF immediately, so the mount
        // came up and went away again in the same breath while printing "mounted" and "unmounted",
        // which reads like success and leaves nothing mounted.
        //
        // There is nothing to wait *for* in that case, so wait for the process to be stopped
        // instead. A stop leaves ProjFS placeholders behind in the mount directory, which is what
        // `mount` already detects and refuses to mount over, telling the user to delete the folder.
        println!(
            "no console attached, so nothing to press Enter on: the mount stays up until this"
        );
        println!(
            "process is stopped. Stopping it leaves the mount folder behind; delete it before"
        );
        println!("mounting there again.");
        loop {
            std::thread::park();
        }
    }

    drop(m);
    println!("unmounted");
    ExitCode::SUCCESS
}

/// Recursively enumerate + read every file under `dir` via `std::fs` (which drives the ProjFS
/// callbacks), printing a line per entry. Returns (files, dirs, total_bytes_read).
fn walk_verify(dir: &Path) -> (u64, u64, u64) {
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (files, dirs, bytes);
    };
    for e in rd.flatten() {
        let path = e.path();
        let ft = match e.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            dirs += 1;
            println!("  [d] {}", path.display());
            let (f, d, b) = walk_verify(&path);
            files += f;
            dirs += d;
            bytes += b;
        } else {
            let mut buf = Vec::new();
            let n = std::fs::File::open(&path)
                .and_then(|mut f| f.read_to_end(&mut buf))
                .unwrap_or(0);
            files += 1;
            bytes += n as u64;
            println!("  [f] {} ({n} bytes)", path.display());
        }
    }
    (files, dirs, bytes)
}
