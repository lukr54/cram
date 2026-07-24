//! `cram` — the single Cram command line. One binary, subcommand-dispatched (like `git` / `7z`):
//!
//! ```text
//! cram l  <archive>                            list entries
//! cram x  <archive> [-o <dir>] [-p <pw>]       extract (parallel per-entry for ZIP) [--skip]
//! cram a  <archive> <input...> [-p <pw>]       create [--store|--fast|--best] [--encrypt-names]
//! cram t  <archive> [-p <pw>]                  test integrity (decode + checksums, no extract)
//! cram conv <in> <out> [-p <pw>] [--encrypt <pw>]   convert to <out>'s format
//! cram dl <url…|FILE.meta4> [-o <out>] [--extract <dir>]   download (segmented, multi-mirror,
//!                                              --discover / --auto / --sha256) [--features download]
//! cram dedup <folder|file…> [--similar]        find duplicate files across folders/drives
//!                                              (read-only report; --similar also flags
//!                                              visually-alike images for human review)
//! cram mount [--selftest] [-p <pw>] <archive> <dir>   mount as a virtual folder (ProjFS)
//! cram rec <create|verify|repair> <file> …     Reed-Solomon recovery sidecar
//! cram sign|verify|keygen …                    ed25519 signing
//! cram make-sfx <archive.cram> <out.exe>       build a self-extracting executable
//! ```
//!
//! Archive verbs are handled here (calling cram-core's engine); the sidecar/mount tools delegate to
//! their crates' `cli::main`; `make-sfx` shells out to the co-located `cram-extract` stub.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use cram_core::engine::ExtractOptions;
use cram_core::error::Result;
use cram_core::format::{Codec, Container, Format};
use cram_core::progress::NullSink;
use cram_core::secret::{
    EncryptSpec, FixedPassword, HeaderMode, NoPassword, PasswordProvider, Secret,
};
use cram_core::writer::{CreateOptions, Level};
use cram_core::{engine, formats, sniff};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // RAR is decoded by the UnRAR C++ library, which can fault the *whole process* on a crafted
    // archive (why the fuzz harness excludes it). When a verb would read a RAR and we are not already
    // the sacrificial worker, run the command in a child process so a fault kills only the child — the
    // user's shell/session reports a clean error instead of vanishing.
    if let Some(code) = maybe_isolate_rar(&args) {
        return code;
    }
    match args.get(1).map(String::as_str) {
        // Asked for by SECURITY.md when reporting a bug, so it has to exist and be exact. Names the
        // feature set too: a `zstd-c` build writes different `.cram` bytes than the pure-Rust
        // default, which is the first thing worth knowing about a reported archive.
        Some("--version") | Some("-V") | Some("version") => {
            println!("cram {}", env!("CARGO_PKG_VERSION"));
            let enabled: Vec<&str> = [
                ("zstd-c", cfg!(feature = "zstd-c")),
                ("download", cfg!(feature = "download")),
                ("libdeflate", cfg!(feature = "libdeflate")),
                ("phash", cfg!(feature = "phash")),
            ]
            .iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| *name)
            .collect();
            if enabled.is_empty() {
                println!("features: none (pure Rust apart from the UnRAR C++ decoder)");
            } else {
                println!("features: {}", enabled.join(", "));
            }
            ExitCode::SUCCESS
        }
        // Sub-crate CLIs own their own exit code. `rec` is namespaced (its create/verify collide with
        // the archive verbs), so its subcommand starts at args[2]; the others align at args[1].
        Some("mount") => cram_mount::cli::main(&args[1..]),
        Some("rec") | Some("recovery") => cram_recovery::cli::main(&args[2..]),
        Some("sign") | Some("verify") | Some("keygen") => cram_sign::cli::main(&args[1..]),
        Some("make-sfx") => make_sfx(&args),
        // Archive verbs return `Result` and share one error rendering + exit code.
        _ => match run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("cram: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Env flag set on the isolated child so it runs the RAR decode in-process (it *is* the sacrificial
/// worker) instead of recursively re-spawning.
const RAR_WORKER_ENV: &str = "CRAM_RAR_WORKER";

/// If this invocation would drive the UnRAR C++ decoder over an untrusted archive, re-run the whole
/// command in a child process and return its outcome — a fault in the child (a Windows structured
/// exception, or a Unix signal) is reported as a clean error rather than taking down this process.
/// Returns `None` (→ proceed in-process) when isolation doesn't apply: we're already the worker, the
/// verb never reads a RAR, or no argument names an existing RAR file.
fn maybe_isolate_rar(args: &[String]) -> Option<ExitCode> {
    // Already the sacrificial worker → do the work here (this process is the one allowed to die).
    if std::env::var_os(RAR_WORKER_ENV).is_some() {
        return None;
    }
    // Only the archive-reading verbs can reach the RAR decoder.
    let cmd = args.get(1).map(String::as_str)?;
    if !matches!(
        cmd,
        "l" | "list" | "x" | "extract" | "t" | "test" | "conv" | "convert" | "mount"
    ) {
        return None;
    }
    // Isolate only when an argument actually names an existing RAR file (a cheap header sniff), so
    // every non-RAR archive keeps the zero-overhead in-process path.
    if !args.iter().skip(2).any(|a| names_rar_file(a)) {
        return None;
    }

    let exe = std::env::current_exe().ok()?;
    let status = std::process::Command::new(exe)
        .args(&args[1..])
        .env(RAR_WORKER_ENV, "1")
        .status();
    Some(child_exit_code(
        status,
        "cram: the RAR decoder crashed on this archive — isolated, so your session is unaffected. \
         The file is likely corrupt or malicious.",
        "could not launch the isolated RAR worker",
    ))
}

/// Map a child process's exit status to our own [`ExitCode`]. A **normal** exit (0..=255 — including
/// the child's own clean error codes) is passed through unchanged. Anything else means the child
/// **crashed**: a Unix signal yields no code, and a Windows structured exception (e.g. an access
/// violation 0xC0000005) yields a code cast to a *negative* / out-of-range `i32` — either way we print
/// `crash_msg` and return `EX_SOFTWARE` (70). Clamping the raw code into 0..=255 is not an option: it
/// folds a crash into an ordinary exit status, and a negative code lands on 0 — a silent false
/// success. `launch_ctx` labels a spawn failure.
fn child_exit_code(
    status: std::io::Result<std::process::ExitStatus>,
    crash_msg: &str,
    launch_ctx: &str,
) -> ExitCode {
    match status {
        Ok(s) => match s.code() {
            Some(c) if (0..=255).contains(&c) => ExitCode::from(c as u8),
            _ => {
                eprintln!("{crash_msg}");
                ExitCode::from(70)
            }
        },
        Err(e) => {
            eprintln!("cram: {launch_ctx}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Extract a just-downloaded RAR in a sacrificial `cram x` child (worker env set so it does the decode
/// in-process and doesn't re-spawn). A crash in the child (Windows structured exception / Unix signal)
/// is turned into a clean `Err` here, so a malicious download can never take down the `dl` process.
#[cfg(feature = "download")]
fn isolated_rar_extract(archive: &Path, dir: &Path, args: &[String]) -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| cram_core::error::ArchiveError::Backend(format!("current_exe: {e}")))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("x")
        .arg(archive)
        .arg("-o")
        .arg(dir)
        .env(RAR_WORKER_ENV, "1");
    if let Some(pw) = opt(args, "-p") {
        cmd.arg("-p").arg(pw);
    }
    match cmd.status() {
        Ok(s) => match s.code() {
            Some(0) => Ok(()),
            Some(c) if (1..=255).contains(&c) => Err(cram_core::error::ArchiveError::Backend(
                format!("isolated RAR extraction failed (exit {c})"),
            )),
            _ => Err(cram_core::error::ArchiveError::Backend(
                "the RAR decoder crashed on this download — isolated, so your session is \
                 unaffected. The file is likely corrupt or malicious."
                    .into(),
            )),
        },
        Err(e) => Err(cram_core::error::ArchiveError::Backend(format!(
            "could not launch the isolated RAR worker: {e}"
        ))),
    }
}

/// Does `arg` name an existing file whose magic says RAR? Flags and non-files are ignored, and a sniff
/// failure (unreadable / not an archive) is treated as "not RAR" — the check only *adds* isolation, it
/// never blocks a normal path.
fn names_rar_file(arg: &str) -> bool {
    if arg.starts_with('-') {
        return false;
    }
    let p = Path::new(arg);
    p.is_file()
        && sniff::sniff_path(p)
            .map(|f| f.container == Container::Rar)
            .unwrap_or(false)
}

fn run(args: &[String]) -> Result<()> {
    let cmd = args.get(1).map(String::as_str);
    match cmd {
        Some("l") | Some("list") => list(args),
        Some("x") | Some("extract") => extract(args),
        Some("a") | Some("add") | Some("create") => create(args),
        Some("t") | Some("test") => test_cmd(args),
        Some("conv") | Some("convert") => convert_cmd(args),
        Some("dl") | Some("download") => download_cmd(args),
        Some("dedup") | Some("dupes") => dedup_cmd(args),
        _ => {
            usage();
            Ok(())
        }
    }
}

fn usage() {
    eprintln!("usage: cram <command> …");
    eprintln!("  l  <archive>                        list entries");
    eprintln!("  x  <archive> [-o <dir>] [-p <pw>]   extract [--skip]");
    eprintln!(
        "  a  <archive> <input...> [-p <pw>]   create [--store|--fast|--best] [--encrypt-names]"
    );
    eprintln!(
        "  t  <archive> [-p <pw>]              test integrity (decode + checksums, no extract)"
    );
    eprintln!("  conv <in> <out> [-p <pw>] [--encrypt <pw>]   convert to <out>'s format [--best|--fast|--store]");
    eprintln!("  dl <url…|FILE.meta4> [-o <out>] [--extract <dir>] [-n <conns>] [--chunk <mb>]");
    eprintln!(
        "       several urls = mirrors of one file · --discover finds mirrors · --auto ramps"
    );
    eprintln!(
        "       connections · --sha256 <hex> verifies (a Metalink supplies it automatically)"
    );
    eprintln!(
        "  mount [--selftest] [-p <pw>] <archive> <dir>   mount as a virtual folder (ProjFS)"
    );
    eprintln!("  dedup <folder|file…> [--similar] [--min-size <bytes>] [--json]");
    eprintln!("       find duplicate files across folders/drives — reports only by default");
    eprintln!("       [--link] [--quarantine <dir>] [--keep shortest|oldest|first] [--apply]");
    eprintln!(
        "       reclaim space: --link hard-links copies (every path stays put), --quarantine"
    );
    eprintln!("       moves them aside. Previews unless --apply. Nothing is ever deleted.");
    eprintln!("  rec <create|verify|repair> <file> …   Reed-Solomon recovery sidecar");
    eprintln!("  sign <file> -k <keyfile> | verify <file> [--key <hex>] | keygen <keyfile>");
    eprintln!("  make-sfx <archive.cram> <out.exe>   build a self-extracting executable");
    eprintln!(
        "  --version                           version + which optional features are compiled in"
    );
}

/// Is `flag` present anywhere in `args`?
fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// `dl` is only available when built with `--features download` (pulls in rdm-core + tokio).
#[cfg(not(feature = "download"))]
fn download_cmd(_args: &[String]) -> Result<()> {
    Err(cram_core::error::ArchiveError::Backend(
        "download support not compiled in — rebuild with `--features download`".into(),
    ))
}

/// The tar-family format for a download name, if it's stream-extractable (so we can unpack while
/// downloading). `None` → download fully, then sniff the file and extract.
#[cfg(feature = "download")]
fn streamable_fmt_from_name(name: &str) -> Option<Format> {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(Format::tar(Codec::Gzip))
    } else if n.ends_with(".tar.xz") || n.ends_with(".txz") {
        Some(Format::tar(Codec::Xz))
    } else if n.ends_with(".tar.bz2") || n.ends_with(".tbz2") || n.ends_with(".tbz") {
        Some(Format::tar(Codec::Bzip2))
    } else if n.ends_with(".tar.zst") || n.ends_with(".tzst") {
        Some(Format::tar(Codec::Zstd))
    } else if n.ends_with(".tar") {
        Some(Format::tar(Codec::None))
    } else {
        None
    }
}

/// Last path segment of a URL (before any `?`/`#`), used as the default output filename.
///
/// The URL is UNTRUSTED — Metalink discovery replaces the source list with `<url>` elements from
/// the metalink file, so this name controls where the download is written. It must come out as one
/// plain, safe component: a segment like `a\..\..\evil.bat` (backslashes are legal URL characters)
/// would otherwise become a traversal on Windows. `EntryPath::from_raw` applies the same rules as
/// archive entries (rejects `..`/absolute/`:`/NUL, mangles device names); anything that doesn't
/// reduce to exactly one component falls back to `download.bin`.
#[cfg(feature = "download")]
fn filename_from_url(url: &str) -> String {
    let stem = url.split(['?', '#']).next().unwrap_or(url);
    let seg = stem.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    match cram_core::model::EntryPath::from_raw(seg) {
        Some(p) if p.safe().components().count() == 1 => p.safe().to_string_lossy().into_owned(),
        _ => "download.bin".to_string(),
    }
}

/// Positional download sources = every `dl` arg that isn't a flag or a flag's value. One URL is a
/// plain download (or a Metalink to discover from); several are **mirrors of the same file** that the
/// engine verifies against the anchor and stripes across.
#[cfg(feature = "download")]
fn dl_sources(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 2; // skip "dl"
    while i < args.len() {
        match args[i].as_str() {
            // flags that take a value → skip the flag and its value
            "-o" | "-n" | "--chunk" | "--extract" | "-p" | "--sha256" => i += 2,
            // boolean flags (--discover / --auto / --skip) → skip just the flag
            s if s.starts_with('-') => i += 1,
            s => {
                out.push(s.to_string());
                i += 1;
            }
        }
    }
    out
}

/// `" (+ N mirrors)"` for a multi-source download, else empty.
#[cfg(feature = "download")]
fn mirror_note(n: usize) -> String {
    if n > 1 {
        format!(" (+ {} mirrors)", n - 1)
    } else {
        String::new()
    }
}

/// A download-side error (keeps the call sites terse).
#[cfg(feature = "download")]
fn dl_err(msg: impl Into<String>) -> cram_core::error::ArchiveError {
    cram_core::error::ArchiveError::Backend(msg.into())
}

/// If a whole-file SHA-256 is known (discovered from a Metalink, or passed via `--sha256`), hash the
/// finished file and compare. A mismatch is fatal (returns `Err` — so we never extract a bad file),
/// and so is an I/O error during hashing: a checksum we could NOT compute is not a checksum that
/// passed — failing open here would let a transiently-unreadable (e.g. AV-locked) malicious file skip
/// verification entirely and proceed to extraction.
#[cfg(feature = "download")]
fn verify_download(out: &Path, expected_sha: Option<&str>) -> Result<()> {
    let Some(want) = expected_sha else {
        return Ok(());
    };
    print!("verifying SHA-256 ... ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    match cram_core::net::verify_sha256(out, want) {
        Ok(true) => {
            println!("OK ✓");
            Ok(())
        }
        Ok(false) => {
            println!("MISMATCH ✗");
            Err(dl_err(
                "downloaded file does not match the expected SHA-256 — delete it and retry",
            ))
        }
        Err(e) => {
            println!("UNVERIFIED ✗");
            Err(dl_err(format!(
                "could not verify SHA-256 ({e}) — refusing to treat the file as good; retry when it is readable"
            )))
        }
    }
}

#[cfg(feature = "download")]
fn download_cmd(args: &[String]) -> Result<()> {
    use cram_core::engine::stream;
    use cram_core::net::{self, DownloadMode, RdmSource};
    use cram_core::source::ByteSource;

    // Positional sources: 1 = a plain URL / Metalink, N = explicit mirrors of one file.
    let mut sources = dl_sources(args);
    if sources.is_empty() {
        usage();
        return Err(dl_err("missing download URL"));
    }

    let discover = has(args, "--discover");
    let auto = has(args, "--auto");
    let conns_explicit = opt(args, "-n").is_some();
    let conns: usize = opt(args, "-n").and_then(|s| s.parse().ok()).unwrap_or(8);
    let chunk: u64 = opt(args, "--chunk")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let extract = opt(args, "--extract").map(PathBuf::from);
    let mut expected_sha = opt(args, "--sha256").map(str::to_string);

    // Mirror discovery: a `.meta4`/`.metalink` input, or `--discover` on a single URL, expands to a
    // verified mirror list (+ maybe a whole-file SHA-256). Only for a single input — several given
    // URLs are already an explicit mirror set. Discovery only proposes; the engine's verify gate
    // still byte-checks every discovered mirror, so a bad discovered link can't corrupt the download.
    if sources.len() == 1 && (discover || net::is_metalink_ref(&sources[0])) {
        match net::discover_mirrors(&sources[0]) {
            Ok(Some(d)) => {
                println!(
                    "discovered {} mirror(s) via {}{}",
                    d.sources.len(),
                    d.via,
                    if d.sha256.is_some() { " + SHA-256" } else { "" }
                );
                if expected_sha.is_none() {
                    expected_sha = d.sha256;
                }
                if !d.sources.is_empty() {
                    sources = d.sources;
                }
            }
            Ok(None) => eprintln!("discovery: no mirrors found; downloading the URL directly"),
            Err(e) => eprintln!("discovery failed: {e}; downloading the URL directly"),
        }
    }

    // Output name comes from the (post-discovery) anchor source, so a `.meta4` input yields the real
    // filename rather than "foo.meta4".
    let out = opt(args, "-o")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(filename_from_url(&sources[0])));
    let name = out.file_name().and_then(|s| s.to_str()).unwrap_or("");
    // --auto ramps up to a ceiling: -n if the user set one, else a sane 64.
    let ceiling = if auto && !conns_explicit { 64 } else { conns };

    // Stream-extract path: tar-family → unpack while the download runs (leading-edge scheduling).
    // ONLY when no checksum is expected: streaming inherently extracts before the whole-file hash
    // can be computed, so with a known SHA-256 the hostile-mirror case would land tampered files in
    // the destination before verification failed. Verify-before-extract wins over streaming speed.
    if let Some(dir) = &extract {
        if expected_sha.is_some() && streamable_fmt_from_name(name).is_some() {
            println!(
                "a SHA-256 is expected — downloading fully and verifying BEFORE extracting \
                 (stream-extract is skipped so unverified bytes never land in {})",
                dir.display()
            );
        }
        if expected_sha.is_none() {
            if let Some(fmt) = streamable_fmt_from_name(name) {
                if auto {
                    eprintln!(
                    "note: --auto is ignored while stream-extracting — leading-edge scheduling is \
                     used so the extract frontier keeps advancing"
                );
                }
                println!(
                    "streaming {}{} → {} while downloading...",
                    sources[0],
                    mirror_note(sources.len()),
                    dir.display()
                );
                let source = RdmSource::start(
                    sources,
                    out.clone(),
                    conns,
                    chunk,
                    vec![],
                    DownloadMode::Stream,
                )?;
                let src: Arc<dyn ByteSource> = Arc::new(source);
                let report =
                    stream::extract_stream(src, fmt, dir, ExtractOptions::default(), &NullSink)?;
                println!(
                    "extracted {} files ({:.1} MiB) to {}",
                    report.extracted,
                    report.bytes as f64 / (1024.0 * 1024.0),
                    dir.display()
                );
                report_issues(&report)?;
                return Ok(());
            }
        }
    }

    // Otherwise download fully (progress readout), verify, then extract from the file if requested.
    let mode = if auto {
        DownloadMode::Auto
    } else {
        DownloadMode::Fixed
    };
    println!(
        "downloading {}{} → {} ({})...",
        sources[0],
        mirror_note(sources.len()),
        out.display(),
        if auto {
            format!("auto up to {ceiling} connections")
        } else {
            format!("{ceiling} connections")
        }
    );
    let source = RdmSource::start(sources, out.clone(), ceiling, chunk, vec![], mode)?;
    let ok = source.wait();
    if !ok {
        return Err(dl_err("download incomplete — re-run to resume"));
    }
    let total = source.progress().total();
    let ramped = if auto {
        format!(", ramped to {} connections", source.peak_conns())
    } else {
        String::new()
    };
    println!(
        "downloaded {} ({:.1} MiB{ramped})",
        out.display(),
        total as f64 / (1024.0 * 1024.0)
    );

    // Verify BEFORE extracting so a checksum failure never reaches the extractor.
    verify_download(&out, expected_sha.as_deref())?;

    if let Some(dir) = &extract {
        // A downloaded RAR is attacker-controlled and decodes through the UnRAR C++ library, which can
        // fault the whole process. The startup isolation (maybe_isolate_rar) cannot cover this: the
        // file does not exist yet when it runs, and `dl` isn't in its verb list. So isolate the
        // extract here — run it in a sacrificial `cram x` child (unless we're already that worker).
        if std::env::var_os(RAR_WORKER_ENV).is_none()
            && names_rar_file(out.to_str().unwrap_or_default())
        {
            return isolated_rar_extract(&out, dir, args);
        }
        let fmt = sniff::sniff_path(&out)?;
        let report = engine::extract(
            &out,
            dir,
            password_provider(args),
            Default::default(),
            &NullSink,
        )?;
        println!(
            "extracted {} files ({}) to {}",
            report.extracted,
            fmt.label(),
            dir.display()
        );
        report_issues(&report)?;
    }
    Ok(())
}

/// Print per-entry extraction failures / cancellation to stderr, and return an error when the
/// extraction was **not fully clean** so the process exits non-zero. Extraction is best-effort
/// (failures are collected, not fatal), so a partial/failed/cancelled extract would otherwise look
/// like a success — a chained script (`cram x deps.zip -o build/ && run-build`) must be able to tell
/// via the exit code, not just stderr text. Every command that extracts routes through here.
fn report_issues(report: &cram_core::error::Report) -> Result<()> {
    if !report.failed.is_empty() {
        eprintln!("{} failures:", report.failed.len());
        for (name, err) in &report.failed {
            eprintln!("  {name}: {err}");
        }
    }
    if report.cancelled {
        eprintln!("(cancelled)");
    }
    if !report.failed.is_empty() {
        return Err(cram_core::error::ArchiveError::Backend(format!(
            "extraction completed with {} failure(s)",
            report.failed.len()
        )));
    }
    Ok(())
}

/// Value of a `-flag value` option.
fn opt<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn password_provider(args: &[String]) -> Arc<dyn PasswordProvider> {
    match opt(args, "-p") {
        Some(pw) => Arc::new(FixedPassword(Secret::new(pw))),
        None => Arc::new(NoPassword),
    }
}

fn list(args: &[String]) -> Result<()> {
    let archive = args.get(2).map(PathBuf::from).ok_or_else(|| {
        usage();
        cram_core::error::ArchiveError::Backend("missing archive path".into())
    })?;
    let fmt = sniff::sniff_path(&archive)?;
    // Honor `-p` so an encrypted-names archive (7z `-mhe`, `.cram`) can still be listed.
    let reader = formats::open(&archive, fmt, password_provider(args))?;
    let entries = reader.entries()?;
    println!("{} ({} entries)", fmt.label(), entries.len());
    let mut total = 0u64;
    for e in entries {
        let kind = if e.is_dir() { "d" } else { "-" };
        let lock = if e.encrypted { " [encrypted]" } else { "" };
        println!("  {kind} {:>12}  {}{lock}", e.size, e.name());
        // Declared sizes are untrusted header fields (ZIP64 permits u64::MAX per entry): the sum
        // must saturate, not wrap (release) or panic (debug), on a hostile listing.
        total = total.saturating_add(e.size);
    }
    println!("  total uncompressed: {total} bytes");
    Ok(())
}

fn extract(args: &[String]) -> Result<()> {
    let archive = args.get(2).map(PathBuf::from).ok_or_else(|| {
        usage();
        cram_core::error::ArchiveError::Backend("missing archive path".into())
    })?;
    let dest = opt(args, "-o")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_dest(&archive));
    let pw = password_provider(args);
    let opts = ExtractOptions {
        skip_existing: has(args, "--skip"),
    };

    let t0 = Instant::now();
    let report = engine::extract(&archive, &dest, pw, opts, &NullSink)?;
    let secs = t0.elapsed().as_secs_f64();

    let mib = report.bytes as f64 / (1024.0 * 1024.0);
    let skipped = if report.skipped > 0 {
        format!(", {} skipped", report.skipped)
    } else {
        String::new()
    };
    println!(
        "extracted {} files{} ({:.1} MiB) to {} in {:.2}s ({:.0} MiB/s)",
        report.extracted,
        skipped,
        mib,
        dest.display(),
        secs,
        if secs > 0.0 { mib / secs } else { 0.0 }
    );
    report_issues(&report)?;
    Ok(())
}

/// `test` — decode every entry and verify stored checksums without extracting. Exit non-zero if any
/// entry fails, so it's usable in scripts / CI ("is this archive still good?").
fn test_cmd(args: &[String]) -> Result<()> {
    let archive = args.get(2).map(PathBuf::from).ok_or_else(|| {
        usage();
        cram_core::error::ArchiveError::Backend("missing archive path".into())
    })?;
    let pw = password_provider(args);

    let t0 = Instant::now();
    let rep = engine::verify::verify(&archive, pw, &NullSink)?;
    let secs = t0.elapsed().as_secs_f64();
    let mib = rep.bytes as f64 / (1024.0 * 1024.0);

    if rep.ok() {
        println!(
            "OK: {} entries verified ({} by CRC), {:.1} MiB in {:.2}s",
            rep.checked, rep.crc_verified, mib, secs
        );
        Ok(())
    } else {
        for (name, why) in &rep.failures {
            eprintln!("  FAIL {name}: {why}");
        }
        let total = rep.checked + rep.failures.len() as u64;
        Err(cram_core::error::ArchiveError::Backend(format!(
            "integrity check failed: {} of {total} entries bad",
            rep.failures.len()
        )))
    }
}

// ---- dedup -----------------------------------------------------------------------------------

/// `cram dedup <paths…>` — find duplicate files across folders and drives.
///
/// **Read-only.** It reports; it never deletes, moves, or links anything. That is deliberate for a
/// first release over irreplaceable data: the scan has to earn trust before it is allowed to act.
fn dedup_cmd(args: &[String]) -> Result<()> {
    use cram_core::engine::dedup::{self, DedupOptions, GroupKind};

    let roots: Vec<PathBuf> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            *i >= 2
                && !a.starts_with("--")
                && !matches!(
                    args.get(i.wrapping_sub(1)).map(|s| s.as_str()),
                    Some("--min-size")
                        | Some("--similar-distance")
                        | Some("--quarantine")
                        | Some("--keep")
                )
        })
        .map(|(_, a)| PathBuf::from(a))
        .collect();
    if roots.is_empty() {
        eprintln!(
            "usage: cram dedup <folder|file…> [--similar [--similar-distance <0-15>]] \
             [--min-size <bytes>] [--json]"
        );
        return Err(cram_core::error::ArchiveError::Backend(
            "no paths given".into(),
        ));
    }
    let json = args.iter().any(|a| a == "--json");
    let opts = DedupOptions {
        min_size: args
            .iter()
            .position(|a| a == "--min-size")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(1),
        similar_images: args.iter().any(|a| a == "--similar"),
        similar_distance: args
            .iter()
            .position(|a| a == "--similar-distance")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(dedup::similar::DEFAULT_DISTANCE),
    };
    if opts.similar_images && !cfg!(feature = "phash") {
        eprintln!(
            "cram: this build has no image support, so --similar can't run \
             (rebuild with --features phash). Exact duplicates are unaffected."
        );
    }

    // A scan can run for hours over a large pile, so report progress to stderr — which also keeps
    // `--json` on stdout clean and pipeable.
    let prog = Arc::new(cram_core::progress::Progress::new(0, 0));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Only animate for a human at a terminal: redirected to a file or a pipe, the carriage returns
    // would be written literally and corrupt the output being captured.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let ticker = {
        let (prog, done) = (prog.clone(), done.clone());
        std::thread::spawn(move || {
            while interactive && !done.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if done.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                eprint!("\r  scanning… {} read", bytes_human(prog.done_bytes()));
            }
        })
    };

    let t0 = Instant::now();
    let result = dedup::scan(&roots, &opts, prog.as_ref());
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = ticker.join();
    if interactive {
        eprint!("\r{:60}\r", ""); // wipe the ticker line
    }
    let rep = result?;
    let secs = t0.elapsed().as_secs_f64();

    if json {
        print_dedup_json(&rep);
        return Ok(());
    }

    println!(
        "Scanned {} files ({}) in {:.1}s — read {} to be certain.",
        thousands(rep.files_scanned),
        bytes_human(rep.bytes_scanned),
        secs,
        bytes_human(rep.bytes_hashed)
    );
    if rep.unreadable > 0 {
        println!(
            "{} file(s) could not be read and were skipped.",
            rep.unreadable
        );
    }

    let exact: Vec<_> = rep.exact_groups().collect();
    if exact.is_empty() {
        println!("\nNo byte-identical duplicates found.");
    } else {
        println!(
            "\n{} duplicate set(s) · {} redundant cop(ies) · {} reclaimable",
            thousands(exact.len() as u64),
            thousands(rep.redundant_files()),
            bytes_human(rep.reclaimable())
        );
        for g in exact.iter().take(50) {
            println!(
                "\n  {} reclaimable · {} copies × {}",
                bytes_human(g.reclaimable),
                g.files.len(),
                bytes_human(g.files.first().map(|f| f.size).unwrap_or(0))
            );
            for f in &g.files {
                println!("      {}", f.path.display());
            }
        }
        if exact.len() > 50 {
            println!(
                "\n  … and {} more set(s). Use --json for the full list.",
                exact.len() - 50
            );
        }
    }

    let similar: Vec<_> = rep.similar_groups().collect();
    if !similar.is_empty() {
        println!(
            "\n{} set(s) of visually similar images — REVIEW BY HAND.",
            thousands(similar.len() as u64)
        );
        println!(
            "These are NOT byte-identical; they may be different shots. Nothing here is safe to"
        );
        println!("delete automatically, so no space is counted as reclaimable.");
        for g in similar.iter().take(25) {
            println!("\n  {} similar images", g.files.len());
            for f in &g.files {
                println!("      {} ({})", f.path.display(), bytes_human(f.size));
            }
        }
        if similar.len() > 25 {
            println!(
                "\n  … and {} more set(s). Use --json for the full list.",
                similar.len() - 25
            );
        }
    }

    if rep.cancelled {
        println!("\nScan was cancelled — the results above cover only what was scanned.");
    }
    let _ = GroupKind::Exact; // keeps the import honest if the loops above are ever refactored

    // ---- optional: reclaim the space --------------------------------------------------------
    let want_link = args.iter().any(|a| a == "--link");
    let quarantine = args
        .iter()
        .position(|a| a == "--quarantine")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    if !want_link && quarantine.is_none() {
        println!("\nNothing was changed: `dedup` only reports.");
        println!(
            "To reclaim the space, re-run with --link (replace copies with hard links) and/or"
        );
        println!(
            "--quarantine <dir> (move copies aside). Both preview first; --apply performs it."
        );
        return Ok(());
    }
    reclaim_phase(&rep, args, want_link, quarantine)
}

/// Plan — and, only with `--apply`, carry out — the space reclamation for a finished scan.
fn reclaim_phase(
    rep: &cram_core::engine::dedup::DedupReport,
    args: &[String],
    link: bool,
    quarantine: Option<PathBuf>,
) -> Result<()> {
    use cram_core::engine::reclaim::{self, Action, KeepPolicy, ReclaimOptions};

    let keep = match args
        .iter()
        .position(|a| a == "--keep")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
    {
        Some("oldest") => KeepPolicy::Oldest,
        Some("first") => KeepPolicy::First,
        Some("shortest") | None => KeepPolicy::ShortestPath,
        Some(other) => {
            return Err(cram_core::error::ArchiveError::Backend(format!(
                "unknown --keep policy '{other}' (use shortest, oldest, or first)"
            )))
        }
    };
    let opts = ReclaimOptions {
        link,
        quarantine,
        keep,
    };
    let plan = reclaim::plan(rep, &opts);
    reclaim::validate(&plan)?;

    if plan.actions.is_empty() {
        println!("\nNothing to reclaim: no eligible duplicate copies.");
        for (path, why) in plan.skipped.iter().take(10) {
            println!("  skipped {}: {}", path.display(), why);
        }
        return Ok(());
    }

    let apply = args.iter().any(|a| a == "--apply");
    println!(
        "\n{} — {} hard link(s), {} quarantine move(s), {} to reclaim.",
        if apply {
            "APPLYING"
        } else {
            "DRY RUN (nothing will change)"
        },
        plan.links(),
        plan.quarantines(),
        bytes_human(plan.bytes())
    );
    println!(
        "Keeping: the {} copy of each set.",
        match keep {
            KeepPolicy::ShortestPath => "shortest-path",
            KeepPolicy::Oldest => "oldest",
            KeepPolicy::First => "first-by-name",
        }
    );

    for a in plan.actions.iter().take(40) {
        match a.action {
            Action::Link => {
                println!("\n  LINK  {}", a.victim.display());
                println!("     -> hard link to {}", a.keeper.display());
            }
            Action::Quarantine => {
                println!("\n  MOVE  {}", a.victim.display());
                println!(
                    "     -> {}",
                    a.dest
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default()
                );
            }
        }
    }
    if plan.actions.len() > 40 {
        println!("\n  … and {} more action(s).", plan.actions.len() - 40);
    }
    for (path, why) in plan.skipped.iter().take(10) {
        println!("\n  SKIP  {}\n     ({})", path.display(), why);
    }
    if plan.skipped.len() > 10 {
        println!("\n  … and {} more skipped.", plan.skipped.len() - 10);
    }

    if !apply {
        println!("\nThis was a preview. Re-run with --apply to perform it.");
        if plan.links() > 0 {
            println!(
                "Note: hard-linked copies become one file, so an editor that rewrites a photo \
                 in place\nwould change it under every name. Tools that save a new file are unaffected."
            );
        }
        return Ok(());
    }

    let prog = Arc::new(cram_core::progress::Progress::new(0, 0));
    let done = reclaim::apply(&plan, prog.as_ref())?;
    println!(
        "\nDone: {} linked, {} quarantined, {} reclaimed.",
        done.linked,
        done.quarantined,
        bytes_human(done.bytes_reclaimed)
    );
    if done.skipped_changed > 0 {
        println!(
            "{} file(s) had changed since the scan and were left untouched.",
            done.skipped_changed
        );
    }
    if !done.failed.is_empty() {
        println!(
            "{} action(s) failed (originals left intact):",
            done.failed.len()
        );
        for (path, why) in done.failed.iter().take(20) {
            println!("  {}: {}", path.display(), why);
        }
    }
    if done.quarantined > 0 {
        println!(
            "\nQuarantined files were MOVED, not deleted — the space is freed only once you delete\n\
             the quarantine folder yourself, after checking you are happy with it."
        );
    }
    Ok(())
}

/// Machine-readable report. Hand-rolled because cram-cli deliberately has no serde dependency.
fn print_dedup_json(rep: &cram_core::engine::dedup::DedupReport) {
    fn esc(s: &str) -> String {
        let mut o = String::with_capacity(s.len() + 8);
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                '\r' => o.push_str("\\r"),
                '\t' => o.push_str("\\t"),
                c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                c => o.push(c),
            }
        }
        o
    }
    println!("{{");
    println!("  \"filesScanned\": {},", rep.files_scanned);
    println!("  \"bytesScanned\": {},", rep.bytes_scanned);
    println!("  \"bytesRead\": {},", rep.bytes_hashed);
    println!("  \"unreadable\": {},", rep.unreadable);
    println!("  \"reclaimableBytes\": {},", rep.reclaimable());
    println!("  \"cancelled\": {},", rep.cancelled);
    println!("  \"groups\": [");
    for (gi, g) in rep.groups.iter().enumerate() {
        let kind = match g.kind {
            cram_core::engine::dedup::GroupKind::Exact => "exact",
            cram_core::engine::dedup::GroupKind::Similar => "similar",
        };
        println!("    {{");
        println!("      \"kind\": \"{kind}\",");
        println!("      \"reclaimableBytes\": {},", g.reclaimable);
        println!("      \"files\": [");
        for (fi, f) in g.files.iter().enumerate() {
            let comma = if fi + 1 == g.files.len() { "" } else { "," };
            println!(
                "        {{ \"path\": \"{}\", \"size\": {} }}{comma}",
                esc(&f.path.to_string_lossy()),
                f.size
            );
        }
        println!("      ]");
        println!(
            "    }}{}",
            if gi + 1 == rep.groups.len() { "" } else { "," }
        );
    }
    println!("  ]");
    println!("}}");
}

/// Byte count in the largest unit that keeps it readable.
fn bytes_human(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{:.0} {}", v, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// Thousands separators, so a six-figure file count is readable at a glance.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Positional inputs for `create` = every arg after the archive that isn't a flag or a flag value.
///
/// A `--` ends option parsing (everything after it is an input, the Unix convention), and a
/// `-`-prefixed arg that is NOT a known create flag but names an existing path is treated as an
/// input: rejecting every `-`-prefixed arg as an option would silently drop any file whose name
/// starts with `-` from the archive, which is data loss with no warning. Unknown dash-args that name
/// nothing get a loud warning.
fn create_inputs(args: &[String]) -> Vec<PathBuf> {
    const CREATE_FLAGS: &[&str] = &["--best", "--fast", "--store", "--encrypt-names"];
    let mut out = Vec::new();
    let mut positional_only = false;
    let mut i = 3; // skip "a" and <archive>
    while i < args.len() {
        let s = args[i].as_str();
        if positional_only {
            out.push(PathBuf::from(s));
        } else if s == "--" {
            positional_only = true;
        } else if s == "-p" {
            i += 1; // skip the flag's value too
        } else if s.starts_with('-') {
            if CREATE_FLAGS.contains(&s) {
                // known boolean flag — nothing to collect
            } else if Path::new(s).exists() {
                out.push(PathBuf::from(s)); // a real file that happens to start with '-'
            } else {
                eprintln!(
                    "warning: ignoring unknown option {s:?} — put `--` before file names that start with '-'"
                );
            }
        } else {
            out.push(PathBuf::from(s));
        }
        i += 1;
    }
    out
}

/// Pick the create format from the archive extension (the file doesn't exist yet, so no magic).
fn fmt_for_create(archive: &Path) -> Result<Format> {
    let name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".zip") {
        Ok(Format::zip())
    } else if name.ends_with(".7z") {
        Ok(Format::sevenz())
    } else if name.ends_with(".cram") {
        Ok(Format::cram(Codec::None))
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(Format::tar(Codec::Gzip))
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        Ok(Format::tar(Codec::Xz))
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") || name.ends_with(".tbz") {
        Ok(Format::tar(Codec::Bzip2))
    } else if name.ends_with(".tar.lz4") {
        Ok(Format::tar(Codec::Lz4))
    } else if name.ends_with(".tar.br") {
        Ok(Format::tar(Codec::Brotli))
    } else if name.ends_with(".tar.zst") || name.ends_with(".tzst") {
        Ok(Format::tar(Codec::Zstd))
    } else if name.ends_with(".tar") {
        Ok(Format::tar(Codec::None))
    } else {
        Err(cram_core::error::ArchiveError::Backend(
            "create supports .zip / .7z / .cram / .tar[.gz|.xz|.bz2|.lz4|.br|.zst]".into(),
        ))
    }
}

fn create(args: &[String]) -> Result<()> {
    let archive = args.get(2).map(PathBuf::from).ok_or_else(|| {
        usage();
        cram_core::error::ArchiveError::Backend("missing archive path".into())
    })?;
    let inputs = create_inputs(args);
    if inputs.is_empty() {
        usage();
        return Err(cram_core::error::ArchiveError::Backend(
            "no input files/dirs to add".into(),
        ));
    }
    let fmt = fmt_for_create(&archive)?;

    let level = if has(args, "--best") {
        Level::Best
    } else if has(args, "--fast") {
        Level::Fastest
    } else {
        Level::Auto
    };
    let encrypt = opt(args, "-p").map(|pw| {
        let mut spec = EncryptSpec::new(Secret::new(pw));
        // `--encrypt-names` hides the file listing too (7z / .cram only; ignored by ZIP/tar).
        if has(args, "--encrypt-names") {
            spec.header = HeaderMode::NamesToo;
        }
        spec
    });
    let opts = CreateOptions {
        level,
        encrypt,
        codec: has(args, "--store").then_some(Codec::None),
        solid: false,
        threads: None,
    };

    let t0 = Instant::now();
    let report = engine::create::create(&archive, fmt, &inputs, opts, &NullSink)?;
    let secs = t0.elapsed().as_secs_f64();

    let in_mib = report.in_bytes as f64 / (1024.0 * 1024.0);
    let out_mib = report.out_bytes as f64 / (1024.0 * 1024.0);
    let ratio = if report.in_bytes > 0 {
        report.out_bytes as f64 / report.in_bytes as f64
    } else {
        0.0
    };
    // Auto mode stores already-compressed entries verbatim; surface how many so the ratio makes sense.
    let stored = if report.stored > 0 {
        format!(", {} stored (incompressible)", report.stored)
    } else {
        String::new()
    };
    // `.cram` reports bytes eliminated by cross-file dedup.
    let dedup = if report.dedup_saved > 0 {
        format!(
            ", {:.1} MiB deduped",
            report.dedup_saved as f64 / (1024.0 * 1024.0)
        )
    } else {
        String::new()
    };
    println!(
        "created {} ({} entries{}{}, {:.1} MiB → {:.1} MiB, {:.0}% ratio) in {:.2}s",
        archive.display(),
        report.entries,
        stored,
        dedup,
        in_mib,
        out_mib,
        ratio * 100.0,
        secs,
    );
    Ok(())
}

/// `cram conv <in> <out> [-p <src-pw>] [--encrypt <dst-pw>] [--best|--fast|--store]` — re-export any
/// readable archive into `<out>`'s format. `-p` opens an encrypted SOURCE; `--encrypt` encrypts the
/// DESTINATION (independent passwords). The interop escape hatch: a `.cram` is never a dead end.
fn convert_cmd(args: &[String]) -> Result<()> {
    let src = args.get(2).map(PathBuf::from).ok_or_else(|| {
        usage();
        cram_core::error::ArchiveError::Backend("missing source archive".into())
    })?;
    let dst = args.get(3).map(PathBuf::from).ok_or_else(|| {
        usage();
        cram_core::error::ArchiveError::Backend("missing destination archive".into())
    })?;
    let src_fmt = sniff::sniff_path(&src)?; // magic-sniff the existing source
    let dst_fmt = fmt_for_create(&dst)?; // format from the destination extension

    let level = if has(args, "--best") {
        Level::Best
    } else if has(args, "--fast") {
        Level::Fastest
    } else {
        Level::Auto
    };
    let encrypt = opt(args, "--encrypt").map(|pw| {
        let mut spec = EncryptSpec::new(Secret::new(pw));
        if has(args, "--encrypt-names") {
            spec.header = HeaderMode::NamesToo;
        }
        spec
    });
    let opts = CreateOptions {
        level,
        encrypt,
        codec: has(args, "--store").then_some(Codec::None),
        solid: false,
        threads: None,
    };

    let t0 = Instant::now();
    let report = engine::convert::convert(
        &src,
        src_fmt,
        &dst,
        dst_fmt,
        &opts,
        password_provider(args),
        &NullSink,
    )?;
    let secs = t0.elapsed().as_secs_f64();
    let in_mib = report.in_bytes as f64 / (1024.0 * 1024.0);
    let out_mib = report.out_bytes as f64 / (1024.0 * 1024.0);
    let dedup = if report.dedup_saved > 0 {
        format!(
            ", {:.1} MiB deduped",
            report.dedup_saved as f64 / (1024.0 * 1024.0)
        )
    } else {
        String::new()
    };
    println!(
        "converted {} ({}) → {} ({}): {} entries, {:.1} MiB → {:.1} MiB{} in {:.2}s",
        src.display(),
        src_fmt.label(),
        dst.display(),
        dst_fmt.label(),
        report.entries,
        in_mib,
        out_mib,
        dedup,
        secs,
    );
    Ok(())
}

/// Default output dir = the archive's stem next to it (`foo.zip` → `./foo/`).
fn default_dest(archive: &Path) -> PathBuf {
    let stem = archive
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("out");
    archive.parent().unwrap_or(Path::new(".")).join(stem)
}

/// `cram make-sfx <archive.cram> <out.exe>` — build a self-extracting executable. Delegates to the
/// co-located `cram-extract` stub, which appends itself onto the payload. Kept a separate small binary
/// on purpose: the SFX stub must stay tiny and carry none of the engine, so the unified `cram` locates
/// and invokes it rather than embedding it.
fn make_sfx(args: &[String]) -> ExitCode {
    use std::process::Command;
    let (Some(payload), Some(out)) = (args.get(2), args.get(3)) else {
        eprintln!("usage: cram make-sfx <archive.cram> <out.exe>");
        return ExitCode::from(2);
    };
    let stub = match locate_stub() {
        Some(p) => p,
        None => {
            eprintln!(
                "cram: cram-extract stub not found next to this binary — it builds the SFX; \
                 ship cram-extract alongside cram"
            );
            return ExitCode::FAILURE;
        }
    };
    let status = Command::new(&stub)
        .arg("--make-sfx")
        .arg(payload)
        .arg(out)
        .status();
    // Classify like the RAR worker: a crashed stub (negative/out-of-range Windows code, or a signal)
    // must report failure. Clamping the code into 0..=255 would floor a crash to 0 and report
    // SUCCESS, which lets `cram make-sfx … && upload out.exe` ship a half-written executable.
    child_exit_code(
        status,
        "cram: the cram-extract stub crashed while building the SFX — the output is incomplete.",
        "failed to run cram-extract stub",
    )
}

/// Find the `cram-extract` stub sitting next to this executable (release ships them together).
fn locate_stub() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    ["cram-extract.exe", "cram-extract"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_rar_file_detects_only_a_real_rar() {
        let dir = std::env::temp_dir().join(format!("cram-cli-rar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A real RAR magic prefix → detected (triggers isolation).
        let rar = dir.join("a.rar");
        std::fs::write(&rar, b"Rar!\x1a\x07\x01\x00 body").unwrap();
        // A ZIP → not RAR (stays on the fast in-process path).
        let zip = dir.join("a.zip");
        std::fs::write(&zip, b"PK\x03\x04 body").unwrap();

        assert!(names_rar_file(rar.to_str().unwrap()));
        assert!(!names_rar_file(zip.to_str().unwrap()));
        assert!(!names_rar_file("-p")); // a flag is never a target
        assert!(!names_rar_file(dir.join("missing").to_str().unwrap())); // no such file
        let _ = std::fs::remove_dir_all(&dir);
    }
}
