//! The `mount` command-line, called by the unified `cram` binary. `args[0]` is treated as the program
//! name (ignored) and the mount parameters are read from `args[1..]`, the same shape the former
//! standalone `cram-mount` binary parsed, so behavior is unchanged.
//!
//! ```text
//! cram mount [--writable] [--remember] [--selftest] [-p <pw>] <archive> <mount-dir>
//! cram mount --restore | --list | --forget <mount-dir>
//! ```
//!
//! Mount an archive as a virtual folder via ProjFS. The format is sniffed; every readable container
//! mounts: the natively-seekable ones (`.cram`, ZIP, ISO 9660) serve ranges straight from disk, and
//! the sequential ones (tar/7z/rar/raw) are decoded once into a bounded in-memory cache and served
//! from there.
//!
//! `--writable` keeps whatever is written into the mount folder as a layer over the archive, which is
//! never modified. `--remember` records the mount in [`crate::registry`], and `--restore` brings the
//! recorded ones back after a reboot; `--list` prints them and `--forget <mount-dir>` drops one. An
//! encrypted archive is not remembered, because its password cannot be stored. The three list verbs
//! mount nothing, so they answer before an archive and a directory are required.
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
    let writable = args.iter().any(|a| a == "--writable");
    let remember = args.iter().any(|a| a == "--remember");

    // These three do not mount anything, so they answer before the parsing below, which requires
    // an archive and a directory.
    if args.iter().any(|a| a == "--list") {
        return list_remembered();
    }
    if args.iter().any(|a| a == "--forget") {
        return forget_remembered(args);
    }
    if args.iter().any(|a| a == "--restore") {
        return restore_remembered();
    }
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
        eprintln!(
            "usage: cram mount [--writable] [--remember] [--selftest] [-p <pw>] <archive> <mount-dir>"
        );
        eprintln!("       cram mount --restore | --list | --forget <mount-dir>");
        return ExitCode::from(2);
    };
    let archive = PathBuf::from(archive);
    let root = PathBuf::from(root);

    let m = match crate::mount_with(&archive, &root, pw, writable) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mount failed: {e}");
            return ExitCode::from(1);
        }
    };
    println!("mounted {} at {}", archive.display(), m.root().display());
    if remember {
        // A password cannot be stored, so an encrypted archive can be mounted but never brought
        // back unattended. Say so when the user asks, rather than at the next reboot.
        if args.iter().any(|a| a == "-p") {
            eprintln!("not remembered: an encrypted archive needs its password at every mount.");
        } else {
            match crate::registry::remember(&archive, &root, writable) {
                Ok(()) => {
                    println!("remembered; `cram mount --restore` brings it back after a reboot.")
                }
                Err(e) => eprintln!("could not remember this mount: {e}"),
            }
        }
    }
    if writable {
        println!(
            "writable: anything written here is kept in {} and layered over the archive,",
            m.root().display()
        );
        println!("which is never modified. Delete that folder to go back to a pristine archive.");
    }

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
        if writable {
            println!(
                "process is stopped. The mount folder is kept, which is the point of --writable."
            );
        } else {
            println!("process is stopped. Stopping it leaves the mount folder behind; delete it");
            println!("before mounting there again.");
        }
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

/// `cram mount --list`: what would come back, and from where.
fn list_remembered() -> ExitCode {
    let entries = crate::registry::load();
    if entries.is_empty() {
        println!("No mounts are remembered.");
        println!("Add one with `cram mount --writable --remember <archive> <dir>`.");
        return ExitCode::SUCCESS;
    }
    for e in &entries {
        let missing = if e.archive.is_file() {
            ""
        } else {
            "   [archive missing]"
        };
        println!(
            "{}  <-  {}{}{}",
            e.root.display(),
            e.archive.display(),
            if e.writable { "  (writable)" } else { "" },
            missing
        );
    }
    println!("\n`cram mount --restore` brings these back.");
    ExitCode::SUCCESS
}

/// `cram mount --forget <dir>`: stop bringing that one back. Never touches the folder itself, so a
/// writable mount's contents are the user's to keep or delete.
fn forget_remembered(args: &[String]) -> ExitCode {
    let Some(dir) = args
        .iter()
        .enumerate()
        .filter(|(i, a)| *i > 0 && !a.starts_with("--"))
        .map(|(_, a)| a)
        .next()
    else {
        eprintln!("usage: cram mount --forget <mount-dir>");
        return ExitCode::from(2);
    };
    match crate::registry::forget(Path::new(dir)) {
        Ok(true) => {
            println!("forgotten: {dir}");
            println!("The folder and anything in it are untouched.");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            eprintln!("{dir} was not in the remembered list.");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("could not update the remembered list: {e}");
            ExitCode::from(1)
        }
    }
}

/// `cram mount --restore`: bring back every remembered mount in this one process, then hold them.
///
/// One process for all of them, rather than one each, so that quitting whatever launched it cannot
/// take a mount away mid-session and so the mounts have somewhere to share a memory budget later.
///
/// A single bad entry never costs the others: a missing archive, a folder that has become something
/// else, or an archive that now needs a password is reported and skipped.
fn restore_remembered() -> ExitCode {
    let entries = crate::registry::load();
    if entries.is_empty() {
        println!("Nothing is remembered, so nothing was restored.");
        return ExitCode::SUCCESS;
    }
    let mut held = Vec::new();
    let mut failed = 0;
    for e in &entries {
        if !e.archive.is_file() {
            eprintln!(
                "skipped {}: {} is gone",
                e.root.display(),
                e.archive.display()
            );
            failed += 1;
            continue;
        }
        match crate::mount_with(&e.archive, &e.root, Arc::new(NoPassword), e.writable) {
            Ok(m) => {
                println!("mounted {} at {}", e.archive.display(), e.root.display());
                held.push(m);
            }
            Err(err) => {
                eprintln!("skipped {}: {err}", e.root.display());
                failed += 1;
            }
        }
    }
    if held.is_empty() {
        eprintln!("nothing could be restored ({failed} failed).");
        return ExitCode::from(1);
    }
    println!(
        "{} mount(s) restored{}. They stay up until this process is stopped.",
        held.len(),
        if failed > 0 {
            format!(", {failed} skipped")
        } else {
            String::new()
        }
    );
    // Hold them. There is no console to wait on at boot, and dropping `held` would unmount
    // everything that was just restored.
    loop {
        std::thread::park();
    }
}
