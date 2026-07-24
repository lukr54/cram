//! The `rec create` / `rec verify` / `rec repair` command-line, called by the unified `cram` binary.
//! `args` is the slice starting at the subcommand (e.g. `["create", <file>, "-r", "10"]`), so `args[0]`
//! is the verb. Behavior is identical to the former standalone `cram-rec` binary — only the entry shape
//! and the program name in the usage text changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn sidecar_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_os_string();
    s.push(".cramrec");
    PathBuf::from(s)
}

fn usage() -> ExitCode {
    eprintln!("usage:");
    eprintln!("  cram rec create <file> [-r <percent>]      write <file>.cramrec (default 10%)");
    eprintln!("  cram rec verify <file>                     check <file> against its sidecar");
    eprintln!("  cram rec repair <file> [-o <out>] [--in-place]   reconstruct a damaged <file>");
    ExitCode::from(2)
}

fn opt<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}
fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Run the recovery CLI. `args[0]` is the subcommand (`create` / `verify` / `repair`).
pub fn main(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().cloned() else {
        return usage();
    };
    // First positional after the command is the target file.
    let Some(file) = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-') && !is_flag_value(args, a))
        .cloned()
    else {
        return usage();
    };
    let file = PathBuf::from(file);

    match cmd.as_str() {
        "create" => {
            let redundancy = opt(args, "-r")
                .and_then(|s| s.parse::<f64>().ok())
                .map(|pct| pct / 100.0)
                .unwrap_or(0.10);
            run_create(&file, redundancy)
        }
        "verify" => run_verify(&file),
        "repair" => {
            let out = opt(args, "-o").map(PathBuf::from);
            let in_place = has(args, "--in-place");
            run_repair(&file, out, in_place)
        }
        "-h" | "--help" => usage(),
        other => {
            eprintln!("unknown command: {other}");
            usage()
        }
    }
}

/// Is `val` the value that immediately follows a value-taking flag (-r / -o)? Such a token must not be
/// mistaken for the positional file argument.
fn is_flag_value(args: &[String], val: &str) -> bool {
    for w in args.windows(2) {
        if (w[0] == "-r" || w[0] == "-o") && w[1] == val {
            return true;
        }
    }
    false
}

fn run_create(file: &Path, redundancy: f64) -> ExitCode {
    let data = match fs::read(file) {
        Ok(d) => d,
        Err(e) => return fail(&format!("read {}: {e}", file.display())),
    };
    if data.is_empty() {
        return fail("cannot protect an empty file");
    }
    let side = match crate::create_sidecar(&data, redundancy) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let out = sidecar_path(file);
    if let Err(e) = fs::write(&out, &side) {
        return fail(&format!("write {}: {e}", out.display()));
    }
    let (n, m, _ss) = crate::geometry(data.len() as u64, redundancy);
    println!(
        "wrote {} ({} bytes): {n} data + {m} parity shards, tolerates up to {m} lost shard(s)",
        out.display(),
        side.len()
    );
    ExitCode::SUCCESS
}

fn run_verify(file: &Path) -> ExitCode {
    let (data, side) = match load_pair(file) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    match crate::verify(&data, &side) {
        Ok(true) => {
            println!("{}: intact", file.display());
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!(
                "{}: DAMAGED but recoverable — run `cram rec repair`",
                file.display()
            );
            ExitCode::from(1)
        }
        Err(e) => fail(&format!("{}: {e}", file.display())),
    }
}

fn run_repair(file: &Path, out: Option<PathBuf>, in_place: bool) -> ExitCode {
    let (data, side) = match load_pair(file) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let repair = match crate::repair(&data, &side) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    // Default: non-destructive — write <file>.repaired. --in-place overwrites the original.
    let dest = if in_place {
        file.to_path_buf()
    } else {
        out.unwrap_or_else(|| {
            let mut s = file.as_os_str().to_os_string();
            s.push(".repaired");
            PathBuf::from(s)
        })
    };
    // Write to a sibling temp then atomically rename over `dest`. This matters most for `--in-place`:
    // the original (the only copy) is never left half-overwritten if the write fails midway — it is
    // replaced only once the full reconstructed content is safely on disk.
    if let Err(e) = write_atomic(&dest, &repair.data) {
        return fail(&format!("write {}: {e}", dest.display()));
    }
    if repair.repaired_shards == 0 {
        println!(
            "{}: already intact; wrote a verified copy to {}",
            file.display(),
            dest.display()
        );
    } else {
        println!(
            "repaired {} shard(s); wrote reconstructed file to {}",
            repair.repaired_shards,
            dest.display()
        );
    }
    ExitCode::SUCCESS
}

/// Write `data` to `dest` durably: stage it in a sibling temp file, then rename over `dest`. `rename`
/// replaces an existing file atomically on the same volume (Windows `MoveFileEx`), so a mid-write
/// failure can never truncate or corrupt `dest` — critical for the in-place case where `dest` is the
/// user's only copy.
fn write_atomic(dest: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp = dest.as_os_str().to_os_string();
    tmp.push(".cram-rec.tmp");
    let tmp = PathBuf::from(tmp);
    // Write + fsync BEFORE the rename. The rename is journaled metadata; without the data fsync a
    // power cut can replay the rename over never-flushed blocks, leaving `dest` full of zeros —
    // fatal for `--in-place`, where `dest` is the user's ONLY copy.
    let write_synced = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, data)?;
        f.sync_all()
    })();
    if let Err(e) = write_synced.and_then(|()| fs::rename(&tmp, dest)) {
        let _ = fs::remove_file(&tmp); // don't leave the temp behind on failure
        return Err(e);
    }
    Ok(())
}

fn load_pair(file: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let data = fs::read(file).map_err(|e| format!("read {}: {e}", file.display()))?;
    let sp = sidecar_path(file);
    let side = fs::read(&sp).map_err(|e| format!("read sidecar {}: {e}", sp.display()))?;
    Ok((data, side))
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::FAILURE
}
