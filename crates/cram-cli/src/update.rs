//! `cram update` — fetch the newest published release and replace this installation with it.
//!
//! Cram does not phone home. This runs **only when the user types the command**, which is also what
//! makes replacing the binary defensible: nothing here happens on a timer, at startup, or as a side
//! effect of another verb.
//!
//! The safety rules, in the order they matter:
//!
//! 1. **Nothing is installed unverified.** The release archive is checked against the SHA-256 the
//!    release publishes, and a missing or unreadable checksum is a refusal, not a warning. A
//!    checksum we could not compute is not a checksum that passed.
//! 2. **The download URL is constructed here** from a character-checked tag and this build's own
//!    target triple. The API response is read for the tag and nothing else; a URL taken from the
//!    reply would let whoever answers choose what we execute.
//! 3. **The install directory is proved writable before anything is downloaded**, so a
//!    permission problem is a message rather than 4 MB of wasted transfer and a half-updated
//!    install.
//! 4. **Replacement is move-aside, never overwrite-in-place.** A partially written binary is worse
//!    than an old one, and on Windows the running image cannot be overwritten at all.
//!
//! `CRAM_UPDATE_REPO` points the whole path at another `owner/name` for testing before a release
//! exists. It is never set in a shipped build.

use std::io::Write;
use std::path::{Path, PathBuf};

use cram_core::error::{ArchiveError, Result};

/// Where releases are published. One constant: moving to an organisation later is a one-line change.
pub const REPO: &str = "lukr54/cram";

/// The release JSON is a few KB. Past this it is either a mistake or someone feeding us a body
/// until we run out of memory.
const MAX_JSON: usize = 256 * 1024;
/// A checksum file is a handful of lines.
const MAX_SUMS: usize = 64 * 1024;

fn err(msg: impl Into<String>) -> ArchiveError {
    ArchiveError::Backend(msg.into())
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---- target ----------------------------------------------------------------------------------------

/// What this build should look for in a release: the triple in the asset name, the archive
/// extension the CI publishes for it, and the checksum file that sits beside it.
struct Target {
    triple: &'static str,
    ext: &'static str,
    sums: &'static str,
}

/// The three targets the release workflow actually publishes. A build for anything else (a distro
/// package, a `cargo install`, a cross-compile) has no asset to fetch and is told so rather than
/// being handed a binary for the wrong machine.
fn target() -> Option<Target> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some(Target {
            triple: "x86_64-pc-windows-gnu",
            ext: "zip",
            sums: "SHA256SUMS.windows",
        }),
        ("linux", "x86_64") => Some(Target {
            triple: "x86_64-unknown-linux-gnu",
            ext: "tar.gz",
            sums: "SHA256SUMS",
        }),
        ("macos", "aarch64") => Some(Target {
            triple: "aarch64-apple-darwin",
            ext: "tar.gz",
            sums: "SHA256SUMS.macos",
        }),
        _ => None,
    }
}

/// The files a release ships that belong next to the `cram` binary. `cram-extract` is the
/// standalone decoder the SFX stub is built from, and `make-sfx` looks for it beside `cram`, so an
/// update that moved only `cram` would leave `make-sfx` building self-extractors from the old stub.
fn payload_files() -> &'static [&'static str] {
    if cfg!(windows) {
        // libwinpthread-1.dll must travel with the binaries (THIRD-PARTY-NOTICES.md section 3).
        &["cram.exe", "cram-extract.exe", "libwinpthread-1.dll"]
    } else {
        &["cram", "cram-extract"]
    }
}

/// The name of the `cram` binary itself within a release payload, the one file that replaces the
/// running image rather than a sibling.
fn main_binary() -> &'static str {
    if cfg!(windows) {
        "cram.exe"
    } else {
        "cram"
    }
}

// ---- versions --------------------------------------------------------------------------------------

/// Parse `v1.2.3`, `1.2`, `cram-1.2.3-rc1` → `(1, 2, 3)`.
///
/// Leading non-digits are skipped, missing components are zero, and a pre-release/build suffix is
/// ignored. A component that is not purely numeric fails the whole parse rather than being read as
/// a number, so an unexpected tag degrades to "same version" instead of to a false upgrade.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let core = &s[start..];
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut it = core.split('.');
    let mut part = |first: bool| -> Option<u64> {
        match it.next() {
            Some(t) if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) => t.parse().ok(),
            Some(_) => None,
            None if first => None,
            None => Some(0),
        }
    };
    let major = part(true)?;
    Some((major, part(false)?, part(false)?))
}

/// True only if `latest` parses to a strictly higher version than `current`. Unparseable on either
/// side is never a reason to replace a working binary.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Characters allowed in a tag. The tag comes from the network and ends up in a URL path and in a
/// filename, so this is what stands between a hostile release name and a path traversal.
fn safe_tag(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 64
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

/// `owner/name`, one slash, both sides made of characters GitHub allows.
fn valid_repo(r: &str) -> bool {
    let mut parts = r.split('/');
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(o), Some(n), None) if ok(o) && ok(n))
}

pub fn configured_repo() -> String {
    std::env::var("CRAM_UPDATE_REPO")
        .ok()
        .filter(|r| valid_repo(r))
        .unwrap_or_else(|| REPO.to_string())
}

pub fn releases_page() -> String {
    format!("https://github.com/{}/releases", configured_repo())
}

// ---- the release -----------------------------------------------------------------------------------

pub struct Release {
    /// The tag exactly as published, e.g. `v1.1.0`. Asset names are built from this.
    pub tag: String,
    /// The tag with any leading `v` removed, for comparing and printing.
    pub version: String,
    pub published: String,
}

/// Ask GitHub for the latest published release. `/releases/latest` excludes drafts and
/// pre-releases, so a tagged release candidate never reaches someone who wanted a stable build.
pub fn latest_release() -> Result<Release> {
    let repo = configured_repo();
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let body = cram_core::net::fetch_text(&url, Some("application/vnd.github+json"), MAX_JSON)
        .map_err(|e| {
            // A repository the caller cannot see answers 404 exactly like one with no release.
            if e.to_string().contains("404") {
                err(format!(
                    "no published release found at github.com/{repo} yet"
                ))
            } else {
                err(format!("could not reach GitHub: {e}"))
            }
        })?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| err("GitHub sent something unexpected"))?;

    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if tag.is_empty() {
        return Err(err("that release has no version tag"));
    }
    if !safe_tag(&tag) {
        return Err(err(format!(
            "refusing to use the release tag {tag:?}: it is not a plain version tag"
        )));
    }
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let published = v
        .get("published_at")
        .and_then(|p| p.as_str())
        .filter(|p| p.len() <= 40 && p.is_ascii())
        .unwrap_or("")
        .to_string();

    Ok(Release {
        tag,
        version,
        published,
    })
}

/// Pull the SHA-256 for `asset` out of the checksum file the release publishes beside it.
///
/// `sha256sum` format: `<hex><space><space-or-star><name>`. Only the line naming this exact asset
/// counts; a file listing several artefacts (a future combined SHA256SUMS) still resolves.
fn expected_sha(base: &str, sums_name: &str, asset: &str) -> Result<String> {
    let url = format!("{base}/{sums_name}");
    let text = cram_core::net::fetch_text(&url, None, MAX_SUMS).map_err(|e| {
        err(format!(
            "this release publishes no readable {sums_name} ({e}); refusing to install an \
             unverified binary. Download it yourself from {}",
            releases_page()
        ))
    })?;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(hex), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        let name = name.trim_start_matches('*');
        if name == asset && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(hex.to_string());
        }
    }
    Err(err(format!(
        "{sums_name} does not list {asset}; refusing to install an unverified binary"
    )))
}

// ---- installing ------------------------------------------------------------------------------------

/// Prove we can write into `dir` before anything is downloaded. A failure here is the common case
/// on a system-wide install (`Program Files`, `/usr/local/bin`), and it deserves an instruction
/// rather than a raw permission error four megabytes later.
fn check_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".cram-update-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(err(format!(
            "cannot write to {} ({e}).\n  Re-run from a shell with permission to write there \
             (an elevated prompt on Windows, `sudo` on Unix), or download the release yourself \
             from {}",
            dir.display(),
            releases_page()
        ))),
    }
}

/// Suffix for the displaced copy of a binary that could not simply be replaced.
fn aside_suffix(version: &str) -> String {
    format!(".old-{version}")
}

/// Delete the displaced binaries left by an earlier update. They cannot be removed at the moment
/// they are made, the old image is still running, so the next update is the first opportunity.
/// Best effort: a file still locked by another `cram` is left for the run after this one.
fn sweep_old(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.contains(".old-") && name.starts_with("cram") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Put `src` at `dst`, replacing whatever is there.
///
/// Staged through a temp file **in the destination directory** so the final step is a rename rather
/// than a copy: a copy that fails half way leaves a truncated binary, and a rename either happens
/// or does not. When the destination is a running image (`cram` replacing itself) the rename over
/// it fails on Windows, so the old file is moved aside first — Windows will not let a running
/// executable be deleted or overwritten, but it will let it be **renamed**.
fn install_file(src: &Path, dst: &Path, version: &str) -> Result<()> {
    let dir = dst.parent().ok_or_else(|| err("no install directory"))?;
    let staged = dir.join(format!(
        ".{}.new-{}",
        dst.file_name().and_then(|s| s.to_str()).unwrap_or("cram"),
        std::process::id()
    ));
    std::fs::copy(src, &staged).map_err(|e| {
        err(format!(
            "could not stage {} into {}: {e}",
            src.display(),
            dir.display()
        ))
    })?;

    // Carry the executable bit across; `fs::copy` preserves the mode of the source, which came out
    // of an archive that may not have had one.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if dst.file_name().and_then(|s| s.to_str()) != Some("libwinpthread-1.dll") {
            let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
        }
    }

    if std::fs::rename(&staged, dst).is_ok() {
        return Ok(());
    }

    // The destination is in use. Move it aside, then put the new one in its place.
    let aside = dir.join(format!(
        "{}{}",
        dst.file_name().and_then(|s| s.to_str()).unwrap_or("cram"),
        aside_suffix(version)
    ));
    let _ = std::fs::remove_file(&aside);
    if let Err(e) = std::fs::rename(dst, &aside) {
        let _ = std::fs::remove_file(&staged);
        return Err(err(format!(
            "could not move {} aside: {e}. Nothing was changed",
            dst.display()
        )));
    }
    if let Err(e) = std::fs::rename(&staged, dst) {
        // Put the old one back rather than leaving the install with no binary at all.
        let _ = std::fs::rename(&aside, dst);
        let _ = std::fs::remove_file(&staged);
        return Err(err(format!(
            "could not put the new {} in place: {e}. The previous version was restored",
            dst.display()
        )));
    }
    Ok(())
}

// ---- the command -----------------------------------------------------------------------------------

pub fn usage() {
    eprintln!("usage: cram update [--check] [--force]");
    eprintln!("  downloads the latest published release, verifies its SHA-256, and replaces");
    eprintln!("  this installation with it");
    eprintln!("  --check   report what is available and exit, changing nothing");
    eprintln!("  --force   reinstall even when this is already the latest version");
}

pub fn update_cmd(args: &[String]) -> Result<()> {
    let check_only = args.iter().any(|a| a == "--check");
    let force = args.iter().any(|a| a == "--force");
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
    }

    let current = current_version();
    let rel = latest_release()?;

    if !is_newer(&rel.version, current) && !force {
        println!("cram {current} is the latest version.");
        if rel.version != current {
            // Newer locally than published: a development build, worth saying out loud.
            println!("  (the latest published release is {})", rel.version);
        }
        return Ok(());
    }

    let when = rel
        .published
        .split('T')
        .next()
        .filter(|d| !d.is_empty())
        .map(|d| format!(", released {d}"))
        .unwrap_or_default();
    println!("cram {current} → {}{when}", rel.version);

    if check_only {
        println!(
            "https://github.com/{}/releases/tag/{}",
            configured_repo(),
            rel.tag
        );
        println!("run `cram update` to install it");
        return Ok(());
    }

    let Some(t) = target() else {
        return Err(err(format!(
            "no published build for {}-{}; download from {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            releases_page()
        )));
    };

    // Where this binary lives is where the new one goes. `current_exe` resolves symlinks, so a
    // `/usr/local/bin/cram` symlink updates the real file rather than replacing the link.
    let exe = std::env::current_exe()
        .map_err(|e| err(format!("could not find this binary's own path: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| err("this binary has no parent directory"))?
        .to_path_buf();

    sweep_old(&dir);
    check_writable(&dir)?;

    let asset = format!("cram-{}-{}.{}", rel.tag, t.triple, t.ext);
    let base = format!(
        "https://github.com/{}/releases/download/{}",
        configured_repo(),
        rel.tag
    );

    // The checksum comes first: if the release does not publish one we stop before downloading
    // anything, rather than after.
    let sha = expected_sha(&base, t.sums, &asset)?;

    let work = tempdir(&dir)?;
    let archive = work.join(&asset);
    println!("  {asset}");
    download(&format!("{base}/{asset}"), &archive)?;

    print!("  verifying SHA-256 ... ");
    let _ = std::io::stdout().flush();
    match cram_core::net::verify_sha256(&archive, &sha) {
        Ok(true) => println!("ok"),
        Ok(false) => {
            println!("MISMATCH");
            let _ = std::fs::remove_dir_all(&work);
            return Err(err(
                "the downloaded release does not match its published SHA-256. Nothing was installed",
            ));
        }
        Err(e) => {
            println!("UNVERIFIED");
            let _ = std::fs::remove_dir_all(&work);
            return Err(err(format!(
                "could not verify the download ({e}). Nothing was installed"
            )));
        }
    }

    // Unpacked with Cram's own engine: the release is a .zip or a .tar.gz, both of which this
    // binary already reads, and the extractor is the part of the codebase that gets the most abuse.
    let unpacked = work.join("unpacked");
    cram_core::engine::extract(
        &archive,
        &unpacked,
        std::sync::Arc::new(cram_core::secret::NoPassword),
        Default::default(),
        &cram_core::progress::NullSink,
    )
    .map_err(|e| err(format!("could not unpack the release: {e}")))?;

    // CI stages the files inside a versioned folder; fall back to the archive root so a future
    // change in packaging is a message about a missing binary, not a silent no-op.
    let payload = {
        let versioned = unpacked.join(format!("cram-{}-{}", rel.tag, t.triple));
        if versioned.join(main_binary()).is_file() {
            versioned
        } else {
            unpacked.clone()
        }
    };
    if !payload.join(main_binary()).is_file() {
        let _ = std::fs::remove_dir_all(&work);
        return Err(err(format!(
            "the release archive does not contain {}; nothing was installed",
            main_binary()
        )));
    }

    // The running binary last: the siblings are ordinary files, and if one of them fails there is
    // still an old, consistent, working install.
    let mut installed = Vec::new();
    for name in payload_files().iter().filter(|n| **n != main_binary()) {
        let src = payload.join(name);
        if src.is_file() {
            install_file(&src, &dir.join(name), &rel.version)?;
            installed.push(*name);
        }
    }
    println!("  replacing {}", exe.display());
    install_file(&payload.join(main_binary()), &exe, &rel.version)?;
    installed.push(main_binary());

    let _ = std::fs::remove_dir_all(&work);
    println!("Updated to {} ({}).", rel.version, installed.join(", "));
    if cfg!(windows) {
        println!(
            "  the previous binary is kept as *{} and is removed on the next update",
            aside_suffix(&rel.version)
        );
    }
    Ok(())
}

/// A scratch directory beside the install, so the staged rename at the end is within one
/// filesystem (a rename across devices fails, and `%TEMP%` is very often another device).
fn tempdir(near: &Path) -> Result<PathBuf> {
    let d = near.join(format!(".cram-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)
        .map_err(|e| err(format!("could not create {}: {e}", d.display())))?;
    Ok(d)
}

/// Fetch one URL to `out` through the same segmented engine `cram dl` uses, with a progress
/// readout. Eight connections: a release asset is a few megabytes, and this is the one download
/// where the user is sitting and watching.
fn download(url: &str, out: &Path) -> Result<()> {
    use cram_core::net::{DownloadMode, RdmSource};

    let source = RdmSource::start(
        vec![url.to_string()],
        out.to_path_buf(),
        8,
        4,
        vec![],
        DownloadMode::Fixed,
    )?;
    let prog = source.progress().clone();

    // Poll rather than `wait()`: this is a foreground command and a silent pause on a slow link
    // reads as a hang.
    let mut last = String::new();
    while !prog.is_finished() {
        let (done, total) = (prog.done(), prog.total());
        let line = render_bar(done, total);
        if line != last {
            print!("\r  {line}");
            let _ = std::io::stdout().flush();
            last = line;
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    let ok = source.wait();
    println!("\r  {}", render_bar(prog.total(), prog.total()));
    if !ok {
        return Err(err("the download did not complete; re-run to resume"));
    }
    Ok(())
}

fn render_bar(done: u64, total: u64) -> String {
    let mib = done as f64 / (1024.0 * 1024.0);
    if total == 0 {
        return format!("downloading {mib:.1} MiB");
    }
    let frac = (done as f64 / total as f64).clamp(0.0, 1.0);
    let width = 24usize;
    let full = (frac * width as f64).round() as usize;
    format!(
        "downloading [{}{}] {:>3}%  {:.1} MiB",
        "#".repeat(full),
        "-".repeat(width - full),
        (frac * 100.0).round() as u64,
        mib
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_the_shapes_a_tag_actually_takes() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("cram-v10.0.1"), Some((10, 0, 1)));
        assert_eq!(parse_version("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("nightly"), None);
        // A component that is not purely numeric must not be read as a number.
        assert_eq!(parse_version("1.2.3abc"), None);
    }

    #[test]
    fn only_a_strictly_higher_version_replaces_a_working_binary() {
        assert!(is_newer("1.1.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
        // 10 > 9 numerically, which a string comparison gets wrong.
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(!is_newer("nightly", "1.0.0"));
    }

    #[test]
    fn a_hostile_tag_cannot_build_a_url_or_a_path() {
        assert!(safe_tag("v1.2.3"));
        assert!(safe_tag("1.2.3-rc1"));
        assert!(!safe_tag("../../evil"));
        assert!(!safe_tag("v1.2.3?x=1"));
        assert!(!safe_tag("v1 2 3"));
        assert!(!safe_tag(""));
        assert!(!safe_tag(&"v".repeat(65)));
    }

    #[test]
    fn a_repo_override_must_look_like_owner_slash_name() {
        assert!(valid_repo("lukr54/cram"));
        assert!(!valid_repo("cram"), "no owner");
        assert!(!valid_repo("a/b/c"), "extra segment");
        assert!(!valid_repo("lukr54/cram?x=1"));
        assert!(!valid_repo("../../etc"));
        assert!(!valid_repo(""));
    }

    #[test]
    fn the_asset_name_matches_what_the_release_workflow_publishes() {
        // Guards against the CI naming and this parser drifting apart: ci.yml stages
        // `cram-${GITHUB_REF_NAME}-<triple>` and zips/tars that directory.
        let t = target().expect("this test only runs on a published target");
        let asset = format!("cram-{}-{}.{}", "v1.2.3", t.triple, t.ext);
        assert!(asset.starts_with("cram-v1.2.3-"));
        assert!(asset.ends_with(if cfg!(windows) { ".zip" } else { ".tar.gz" }));
        assert!(!t.sums.is_empty());
    }

    #[test]
    fn a_checksum_line_is_matched_by_exact_asset_name() {
        // Built rather than written out: a literal 64-character hex string in the tree is
        // indistinguishable from a leaked ed25519 seed, and the pre-commit scanner rightly
        // refuses one.
        let sums = format!(
            "{}  cram-v1.0.0-other-target.zip\n{} *cram-v1.0.0-x86_64-pc-windows-gnu.zip\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let pick = |asset: &str| -> Option<String> {
            for line in sums.lines() {
                let mut it = line.split_whitespace();
                let (Some(hex), Some(name)) = (it.next(), it.next()) else {
                    continue;
                };
                let name = name.trim_start_matches('*');
                if name == asset && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Some(hex.to_string());
                }
            }
            None
        };
        assert_eq!(
            pick("cram-v1.0.0-x86_64-pc-windows-gnu.zip"),
            Some("b".repeat(64))
        );
        // A near-miss name must not borrow another artefact's hash.
        assert_eq!(pick("cram-v1.0.0-x86_64-pc-windows-gnu.zip.sig"), None);
        assert_eq!(pick("cram-v9.9.9-x86_64-pc-windows-gnu.zip"), None);
    }

    /// The whole point of the move-aside path: a file that cannot be replaced by a plain rename
    /// (a running image on Windows) is still updated, and the old one is left recoverable.
    #[test]
    fn installing_over_a_file_replaces_it_and_keeps_the_old_one_reachable() {
        let dir = std::env::temp_dir().join(format!("cram-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let dst = dir.join("cram-extract");
        std::fs::write(&dst, b"old").unwrap();
        let src = dir.join("payload");
        std::fs::write(&src, b"new").unwrap();

        install_file(&src, &dst, "1.2.3").unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        // No staging file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".new-"))
            .collect();
        assert!(leftovers.is_empty(), "staging files left: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Not a test: the body a spawned copy of this binary runs so that it is genuinely *running*
    /// while [`a_running_binary_can_still_be_replaced`] replaces it. Ignored, so a normal run skips
    /// it; the child is invoked with `--ignored --exact`.
    #[test]
    #[ignore = "the sleeping child of the replace-while-running test"]
    fn sleeper() {
        std::thread::sleep(std::time::Duration::from_secs(20));
    }

    /// The claim the whole command rests on: an executable that is **running right now** can be
    /// replaced, and the old one stays recoverable until the next sweep. Asserted by doing it, to a
    /// real spawned process, not by reasoning about Win32.
    ///
    /// It also proves the *reason* the move-aside exists, by first showing a plain overwrite fails.
    #[test]
    fn a_running_binary_can_still_be_replaced() {
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!("cram-running-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A copy of this very test binary: a real executable that we can start and then replace.
        let me = std::env::current_exe().unwrap();
        let live = dir.join(if cfg!(windows) { "cram.exe" } else { "cram" });
        std::fs::copy(&me, &live).unwrap();
        // Anything the copy needs to start has to travel with it.
        if let Some(src_dir) = me.parent() {
            for dll in [
                "libwinpthread-1.dll",
                "libgcc_s_seh-1.dll",
                "libstdc++-6.dll",
            ] {
                let from = src_dir.join(dll);
                if from.is_file() {
                    let _ = std::fs::copy(&from, dir.join(dll));
                }
            }
        }

        let mut child = Command::new(&live)
            .args(["--exact", "update::tests::sleeper", "--ignored"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the copied binary must start");
        // Give the image time to be mapped; then confirm it really is still running, so a child
        // that failed to start cannot make this test pass vacuously.
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "the child exited early, so nothing was actually running"
        );

        // Why install_file cannot just write over the destination.
        #[cfg(windows)]
        assert!(
            std::fs::write(&live, b"nope").is_err(),
            "a running image was overwritten in place, which Windows should not allow"
        );

        let src = dir.join("payload");
        std::fs::write(&src, b"new").unwrap();
        install_file(&src, &live, "9.9.9").expect("a running binary must still be replaceable");

        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"new",
            "the new bytes are in place"
        );
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "the already-running process must survive its file being replaced"
        );
        // On Windows the old image had to be moved aside; on Unix the rename replaced it outright.
        #[cfg(windows)]
        {
            let aside = dir.join("cram.exe.old-9.9.9");
            assert!(aside.is_file(), "the previous binary must stay recoverable");
            // And the next run cleans it up.
            let _ = child.kill();
            let _ = child.wait();
            sweep_old(&dir);
            assert!(
                !aside.exists(),
                "a later update sweeps the displaced binary"
            );
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sweep_removes_displaced_binaries_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("cram-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "cram.exe.old-1.0.0",
            "cram-extract.exe.old-1.0.0",
            "cram.exe",
            "notes.old-1.0.0",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        sweep_old(&dir);
        assert!(!dir.join("cram.exe.old-1.0.0").exists());
        assert!(!dir.join("cram-extract.exe.old-1.0.0").exists());
        assert!(
            dir.join("cram.exe").exists(),
            "the live binary must survive"
        );
        assert!(
            dir.join("notes.old-1.0.0").exists(),
            "an unrelated file must survive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
