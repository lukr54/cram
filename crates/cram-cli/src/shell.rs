//! `cram shell install|uninstall|status` — put Cram on Explorer's right-click menu, or take it off.
//!
//! Everything goes under **HKCU**, never HKLM: no administrator prompt, no machine-wide change, and
//! an uninstall that cannot leave a key behind that another account can see. Studio installs
//! per-user too, so this matches it.
//!
//! Registration is three facts: the class id exists, it is served by `cram_shell.dll`, and Explorer
//! should ask it about files and folders. Written through `reg.exe` rather than a registry crate —
//! the same way this project registers the browser hand-off and start-with-Windows, and it keeps a
//! Windows API dependency out of the CLI.

use cram_core::error::{ArchiveError, Result};

fn err(msg: impl Into<String>) -> ArchiveError {
    ArchiveError::Backend(msg.into())
}

/// Everything that touches the registry, in one module so the platform split is a single boundary
/// rather than an attribute on each item. Without this the helpers below are dead code on Linux and
/// macOS, where only the stubs are compiled — and the workspace lints with `-D warnings`.
#[cfg(windows)]
mod imp {
    use super::err;
    use cram_core::error::Result;
    use std::path::PathBuf;

    /// Must match `CLSID_CRAM_SHELL` in `cram-shell`. Changing either without the other silently
    /// breaks registration: Explorer would look up a class id that nothing serves.
    const CLSID: &str = "{934088FE-F647-4D05-9A52-FDF56127F43C}";

    /// The key name under `ContextMenuHandlers`. Also what an uninstall deletes, so it must not
    /// change.
    const HANDLER: &str = "Cram";

    const DLL: &str = "cram_shell.dll";

    /// Where the handler is registered from. Both entries point at the same class id; the first covers
    /// every file, the second covers folders (so "Add to archive" works on a directory).
    fn handler_keys() -> [String; 2] {
        [
            format!("HKCU\\Software\\Classes\\*\\shellex\\ContextMenuHandlers\\{HANDLER}"),
            format!("HKCU\\Software\\Classes\\Directory\\shellex\\ContextMenuHandlers\\{HANDLER}"),
        ]
    }

    fn clsid_key() -> String {
        format!("HKCU\\Software\\Classes\\CLSID\\{CLSID}")
    }

    /// The DLL that serves the handler, which ships beside `cram.exe`.
    fn dll_path() -> Result<PathBuf> {
        let exe =
            std::env::current_exe().map_err(|e| err(format!("could not find this binary: {e}")))?;
        let dir = exe
            .parent()
            .ok_or_else(|| err("this binary has no parent directory"))?;
        let dll = dir.join(DLL);
        if !dll.is_file() {
            return Err(err(format!(
            "{} is not next to {}.\n  The Explorer menu ships as a separate DLL; if you built from \
             source, run `cargo build --release -p cram-shell` and copy it beside cram.exe",
            DLL,
            exe.display()
        )));
        }
        Ok(dll)
    }

    fn reg(args: &[&str]) -> Result<()> {
        let out = std::process::Command::new("reg")
            .args(args)
            .output()
            .map_err(|e| err(format!("could not run reg.exe: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        let msg = String::from_utf8_lossy(&out.stderr);
        Err(err(format!(
            "reg {} failed: {}",
            args.join(" "),
            msg.trim()
        )))
    }

    /// Is the handler registered, and does it still point at a DLL that exists?
    fn registered() -> Option<String> {
        let out = std::process::Command::new("reg")
            .args(["query", &format!("{}\\InprocServer32", clsid_key()), "/ve"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // `reg query` prints "    (Default)    REG_SZ    C:\path\to\cram_shell.dll".
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .find(|l| l.contains("REG_SZ"))
            .and_then(|l| l.split("REG_SZ").nth(1))
            .map(|p| p.trim().to_string())
    }

    pub fn install() -> Result<()> {
        let dll = dll_path()?;
        let dll_s = dll.to_string_lossy().to_string();
        let clsid = clsid_key();
        let inproc = format!("{clsid}\\InprocServer32");

        reg(&["add", &clsid, "/ve", "/t", "REG_SZ", "/d", "Cram", "/f"])?;
        reg(&["add", &inproc, "/ve", "/t", "REG_SZ", "/d", &dll_s, "/f"])?;
        // Apartment threading: Explorer calls context-menu handlers on an STA, and a handler that
        // claims Both would be called without the message pump its menu work assumes.
        reg(&[
            "add",
            &inproc,
            "/v",
            "ThreadingModel",
            "/t",
            "REG_SZ",
            "/d",
            "Apartment",
            "/f",
        ])?;
        for key in handler_keys() {
            reg(&["add", &key, "/ve", "/t", "REG_SZ", "/d", CLSID, "/f"])?;
        }

        println!("Cram is on Explorer's right-click menu.");
        println!("  handler  {dll_s}");
        println!("  class    {CLSID}");
        for key in handler_keys() {
            println!("  key      {key}");
        }
        println!();
        println!(
            "On Windows 11 it lives under \"Show more options\" (or press Shift+F10), where every"
        );
        println!(
            "context-menu handler goes. Restart Explorer if it does not appear straight away."
        );
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        // Deleting a key that was never there is not a failure: an uninstall must be safe to run twice,
        // and the installer runs it on upgrade.
        let mut removed = 0;
        for key in handler_keys() {
            if reg(&["delete", &key, "/f"]).is_ok() {
                removed += 1;
            }
        }
        if reg(&["delete", &clsid_key(), "/f"]).is_ok() {
            removed += 1;
        }
        if removed == 0 {
            println!("Cram was not on Explorer's right-click menu.");
        } else {
            println!("Removed Cram from Explorer's right-click menu.");
        }
        Ok(())
    }

    pub fn status() -> Result<()> {
        match registered() {
            Some(path) => {
                println!("Registered.");
                println!("  handler  {path}");
                // A handler pointing at a DLL that has been moved or deleted is the state a failed
                // uninstall leaves behind, and it costs Explorer a load attempt on every right-click.
                if !std::path::Path::new(&path).is_file() {
                    println!("  WARNING  that file does not exist; run `cram shell uninstall`");
                }
            }
            None => println!("Not registered. Run `cram shell install`."),
        }
        Ok(())
    }
} // mod imp

#[cfg(not(windows))]
mod imp {
    use super::err;
    use cram_core::error::Result;

    fn unsupported() -> Result<()> {
        Err(err("the Explorer context menu is Windows-only"))
    }
    pub fn install() -> Result<()> {
        unsupported()
    }
    pub fn uninstall() -> Result<()> {
        unsupported()
    }
    pub fn status() -> Result<()> {
        unsupported()
    }
}

pub fn shell_cmd(args: &[String]) -> Result<()> {
    match args.get(2).map(String::as_str) {
        Some("install") | Some("add") => imp::install(),
        Some("uninstall") | Some("remove") => imp::uninstall(),
        Some("status") | None => imp::status(),
        Some(other) => {
            eprintln!("usage: cram shell <install|uninstall|status>");
            Err(err(format!("unknown shell command {other:?}")))
        }
    }
}
