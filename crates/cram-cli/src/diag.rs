//! `cram diag` — the setting, and the report a user attaches to a bug report.
//!
//! The engine side lives in [`cram_core::diag`], which owns the recording and the redaction. This
//! owns the parts only the binary knows: the version and feature set, the machine profile, and the
//! on-disk setting that survives between runs.
//!
//! **The setting is off until someone turns it on**, and it is stored next to the hardware profile
//! and the mount registry so a person who found one of those can find this too. Nothing here sends
//! anything anywhere; there is no code in this binary that could.

use std::path::PathBuf;

use cram_core::diag::{self as core_diag, ReportHeader};
use cram_core::error::{ArchiveError, Result};

pub use cram_core::diag::apply_stored_setting;

fn version_line() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// The optional features this binary was built with, for the report header.
///
/// **Must list every feature `cram --version` does.** A bug report is read against the build that
/// produced it, and `mimalloc` swaps the global allocator, so a report that omits it hides the one
/// thing that changes every allocation in the process.
fn features_line() -> String {
    let enabled: Vec<&str> = [
        ("zstd-c", cfg!(feature = "zstd-c")),
        ("download", cfg!(feature = "download")),
        ("phash", cfg!(feature = "phash")),
        ("mimalloc", cfg!(feature = "mimalloc")),
    ]
    .iter()
    .filter(|(_, on)| *on)
    .map(|(name, _)| *name)
    .collect();
    if enabled.is_empty() {
        "none (pure Rust apart from the UnRAR C++ decoder)".to_string()
    } else {
        enabled.join(", ")
    }
}

/// The command line, with anything secret taken out.
///
/// `cram a x.cram . -p hunter2` puts a password in argv. A report carrying that would be a
/// credential in a public issue, so the value after a password flag never reaches the file. The
/// environment is not captured at all: an allowlist of safe variables is a list somebody has to
/// keep correct forever, and the command line is the part that actually helps.
pub fn redacted_command(args: &[String]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for (i, a) in args.iter().enumerate() {
        if i == 0 {
            // argv[0] is a path to the binary, and on Windows it usually sits under the user's
            // profile, so it names them.
            out.push("cram".to_string());
            continue;
        }
        if redact_next {
            out.push("<redacted>".to_string());
            redact_next = false;
            continue;
        }
        match a.as_str() {
            "-p" | "--password" | "--encrypt" | "-k" | "--key" => {
                redact_next = true;
                out.push(a.clone());
            }
            _ => {
                // `-p<value>` and `--password=<value>` forms too.
                let joined = ["-p", "--password=", "--encrypt=", "--key="]
                    .iter()
                    .find(|p| a.starts_with(**p) && a.len() > p.len());
                match joined {
                    Some(p) => out.push(format!("{p}<redacted>")),
                    None => out.push(a.clone()),
                }
            }
        }
    }
    core_diag::scrub(&out.join(" "))
}

/// Assemble a header describing this build and machine. `operation`, `archive` and `error` are the
/// caller's to fill in when it has them.
pub fn header() -> ReportHeader {
    ReportHeader {
        version: version_line(),
        features: features_line(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        machine: core_diag::machine_block(),
        operation: String::new(),
        archive: String::new(),
        error: None,
    }
}

/// Write a report describing the run that just happened. `err` is `None` when it succeeded.
///
/// Everything worth reporting -- the timings, the archive's structure, the entries that failed --
/// is gathered during the run and lives in this process, so it has to be written here or not at
/// all.
pub fn write_outcome_report(args: &[String], err: Option<&str>) -> Option<PathBuf> {
    let mut h = header();
    h.operation = redacted_command(args);
    h.error = err.map(str::to_string);
    core_diag::write_report(&h, &core_diag::stamp()).ok()
}

/// Write a report for a failed run when the user has opted in, either standing (the setting) or for
/// this one run (`--diag-report`, which `main` has already stripped from `args`, hence the flag).
pub fn write_failure_report(args: &[String], err: &str, asked: bool) -> Option<PathBuf> {
    if !core_diag::diag().is_full() && !asked {
        return None;
    }
    write_outcome_report(args, Some(err))
}

const USAGE: &str = "\
usage: cram diag <command>
  status                 whether detailed diagnostics are on, and where reports go
  on | off               turn detailed recording on or off (persists; off by default)
  report [--full-paths]  write a report now
  where                  print the folder reports are written to

Detailed recording is opt-in and costs a little speed on archives with very many
files. Reports are written to disk and never sent anywhere.";

pub fn diag_cmd(args: &[String]) -> Result<()> {
    let sub = args.get(2).map(String::as_str);
    match sub {
        None | Some("status") => {
            let on = core_diag::detailed_enabled();
            println!("cram {}", version_line());
            println!(
                "detailed diagnostics: {}",
                if on { "ON" } else { "off (default)" }
            );
            match core_diag::report_dir() {
                Some(d) => println!("reports are written to: {}", d.display()),
                None => println!("reports: no per-user directory available on this system"),
            }
            if let Some(p) = core_diag::settings_path() {
                println!("setting stored in: {}", p.display());
            }
            println!();
            println!("{}", core_diag::EXPLAINER);
            println!();
            println!("Turn on with `cram diag on`, then reproduce the problem, then");
            println!("`cram diag report`.");
            Ok(())
        }
        Some("on") | Some("off") => {
            let on = sub == Some("on");
            let path = core_diag::set_detailed(on)?;
            if on {
                core_diag::diag().set_full(true);
                println!("Detailed diagnostics are ON.");
                println!();
                println!("{}", core_diag::EXPLAINER);
                println!();
                println!("Reproduce the problem, then run `cram diag report`.");
            } else {
                core_diag::diag().set_full(false);
                println!("Detailed diagnostics are off.");
            }
            println!("({})", path.display());
            Ok(())
        }
        Some("report") => {
            let full_paths = args.iter().any(|a| a == "--full-paths");
            if full_paths {
                core_diag::diag().set_full_paths(true);
            }
            let mut h = header();
            if !core_diag::diag().is_full() {
                h.operation =
                    "No operation recorded: this report was written from a fresh process with \
                     detailed diagnostics off, so it describes the build and the machine only."
                        .to_string();
            }
            let path = core_diag::write_report(&h, &core_diag::stamp())?;
            println!("Wrote {}", path.display());
            println!();
            if full_paths {
                println!(
                    "This report contains real file and folder names, because you passed\n\
                     --full-paths. Read it before attaching it to anything public."
                );
            } else {
                println!(
                    "File and folder names are described by shape, not included literally,\n\
                     so this is safe to attach to a public bug report. Nothing has been sent\n\
                     anywhere; sending it is up to you."
                );
            }
            if !core_diag::diag().is_full() {
                println!();
                println!(
                    "Detailed diagnostics were off, so there is no per-entry trace. For a\n\
                     fuller report: `cram diag on`, reproduce the problem, then report again."
                );
            }
            Ok(())
        }
        Some("where") => {
            match core_diag::report_dir() {
                Some(d) => println!("{}", d.display()),
                None => {
                    return Err(ArchiveError::Backend(
                        "no per-user directory available on this system".into(),
                    ))
                }
            }
            Ok(())
        }
        Some(other) => Err(ArchiveError::Backend(format!(
            "unknown `cram diag` command '{other}'\n\n{USAGE}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stamp_is_filename_safe() {
        let s = core_diag::stamp();
        assert_eq!(s.len(), 15, "YYYYMMDD-HHMMSS: {s}");
        assert!(
            s.chars().all(|c| c.is_ascii_digit() || c == '-'),
            "a stamp with a colon in it is not a legal Windows filename: {s}"
        );
    }

    #[test]
    fn a_header_names_the_build_and_the_machine() {
        let h = header();
        assert!(!h.version.is_empty());
        assert!(
            !h.version.starts_with("cram"),
            "the label in the report already says cram; this is the version alone: {}",
            h.version
        );
        assert!(!h.machine.is_empty());
        assert!(
            h.machine.contains("cores"),
            "the machine block is the reproduction key and has to carry the core count: {}",
            h.machine
        );
    }

    #[test]
    fn a_password_never_reaches_a_report() {
        // This is the one that matters: a report is meant to be attachable to a public issue, and
        // `-p` puts a credential in argv.
        for form in [
            vec!["cram", "a", "x.cram", ".", "-p", "hunter2"],
            vec!["cram", "a", "x.cram", ".", "-phunter2"],
            vec!["cram", "a", "x.cram", ".", "--password=hunter2"],
            vec!["cram", "conv", "a", "b", "--encrypt", "hunter2"],
            vec!["cram", "sign", "f", "-k", "hunter2"],
        ] {
            let args: Vec<String> = form.iter().map(|s| s.to_string()).collect();
            let out = redacted_command(&args);
            assert!(
                !out.contains("hunter2"),
                "password survived redaction in {form:?}: {out}"
            );
            assert!(out.contains("<redacted>"), "{out}");
        }
    }

    #[test]
    fn the_binary_path_is_not_reported() {
        // argv[0] on Windows normally sits under the user's profile, so it names them.
        let args: Vec<String> = [r"C:\Users\Ada\Tools\cram\cram.exe", "l", "x.zip"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = redacted_command(&args);
        assert!(!out.contains("Ada"), "{out}");
        assert!(out.starts_with("cram l"), "{out}");
    }

    #[test]
    fn a_rendered_report_says_it_was_not_sent() {
        // The promise is the feature. If this line ever goes missing, the report stops making the
        // one claim a nervous user needs from it.
        let out = cram_core::diag::render(&header());
        assert!(out.contains("has not been sent anywhere"), "{out}");
        assert!(out.contains("no telemetry"), "{out}");
    }
}
