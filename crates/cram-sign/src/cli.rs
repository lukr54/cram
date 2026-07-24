//! The `sign` / `verify` / `keygen` command-line, called by the unified `cram` binary. `args` is the
//! slice starting at the subcommand (e.g. `["sign", <file>, "-k", <keyfile>]`), so `args[0]` is the
//! verb. Behavior is identical to the former standalone `cram-sign` binary — only the entry shape and
//! the program name in the usage text changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn sidecar_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_os_string();
    s.push(".cramsig");
    PathBuf::from(s)
}

fn usage() -> ExitCode {
    eprintln!("usage:");
    eprintln!("  cram keygen <keyfile>              create a signing key (prints its public key)");
    eprintln!("  cram sign   <file> -k <keyfile>    write <file>.cramsig");
    eprintln!("  cram verify <file> [--key <hex>]   check <file> against its signature");
    ExitCode::from(2)
}

fn opt<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::FAILURE
}

/// Run the signing CLI. `args[0]` is the subcommand (`keygen` / `sign` / `verify`).
pub fn main(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first().cloned() else {
        return usage();
    };
    // First positional after the command is the target file/keyfile.
    let Some(target) = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-') && !is_flag_value(args, a))
        .cloned()
    else {
        return usage();
    };
    let target = PathBuf::from(target);

    match cmd.as_str() {
        "keygen" => run_keygen(&target),
        "sign" => match opt(args, "-k").or_else(|| opt(args, "--keyfile")) {
            Some(kf) => run_sign(&target, Path::new(kf)),
            None => {
                eprintln!("sign needs a key: -k <keyfile>");
                usage()
            }
        },
        "verify" => run_verify(&target, opt(args, "--key")),
        "-h" | "--help" => usage(),
        other => {
            eprintln!("unknown command: {other}");
            usage()
        }
    }
}

/// Is `val` the value that immediately follows a value-taking flag? Such a token must not be mistaken
/// for the positional file argument.
fn is_flag_value(args: &[String], val: &str) -> bool {
    for w in args.windows(2) {
        if matches!(w[0].as_str(), "-k" | "--keyfile" | "--key") && w[1] == val {
            return true;
        }
    }
    false
}

fn run_keygen(keyfile: &Path) -> ExitCode {
    if keyfile.exists() {
        return fail(&format!(
            "{} already exists — refusing to overwrite a key",
            keyfile.display()
        ));
    }
    let (bytes, pub_hex) = match crate::generate_key() {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    if let Err(e) = fs::write(keyfile, &bytes) {
        return fail(&format!("write {}: {e}", keyfile.display()));
    }
    println!("wrote signing key to {}", keyfile.display());
    println!("public key: {pub_hex}");
    println!("(share/pin this public key so others can verify your signatures)");
    ExitCode::SUCCESS
}

fn run_sign(file: &Path, keyfile: &Path) -> ExitCode {
    // Load the key BEFORE hashing — a bad key should fail instantly, not after streaming 200 GB.
    let key_bytes = match fs::read(keyfile) {
        Ok(d) => d,
        Err(e) => return fail(&format!("read key {}: {e}", keyfile.display())),
    };
    let key = match crate::load_key(&key_bytes) {
        Ok(k) => k,
        Err(e) => return fail(&e),
    };
    // Stream the file's hash so an archive larger than RAM can still be signed.
    let sig = match crate::sign_file(file, &key) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let out = sidecar_path(file);
    if let Err(e) = fs::write(&out, &sig) {
        return fail(&format!("write {}: {e}", out.display()));
    }
    println!("signed {} → {}", file.display(), out.display());
    ExitCode::SUCCESS
}

fn run_verify(file: &Path, expect_pubkey: Option<&str>) -> ExitCode {
    // Read the sidecar first — a missing signature should fail instantly, not after hashing the file.
    let sp = sidecar_path(file);
    let sig = match fs::read(&sp) {
        Ok(d) => d,
        Err(e) => return fail(&format!("read signature {}: {e}", sp.display())),
    };
    // Stream the file's hash (bounded memory) — the archive may be far larger than RAM.
    match crate::verify_file(file, &sig, expect_pubkey) {
        Ok(v) => {
            println!("{}: signature OK", file.display());
            println!("  signed by public key: {}", v.public_key_hex);
            println!("  file hash (blake3):   {}", v.file_hash_hex);
            if expect_pubkey.is_none() {
                println!(
                    "  note: no --key given, so this proves the file is intact — not who signed it"
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}: VERIFICATION FAILED — {e}", file.display());
            ExitCode::FAILURE
        }
    }
}
