//! Diagnostics: the file a user attaches to a bug report.
//!
//! # What this is for
//!
//! When something goes wrong the useful facts are on the reporter's machine and nowhere else: which
//! core count picked which lane count, what the archive's pack layout was, which entry failed and
//! what its name looked like. This collects those into one text file the user can read, then attach
//! to an email or a pull request.
//!
//! # Three rules it keeps
//!
//! **Nothing ever leaves the machine.** There is no network code in this module and no caller of it
//! sends anything anywhere. A report is a file on disk; moving it is the user's action, in their own
//! mail client. That is the whole design, not a setting.
//!
//! **Nothing is written unless asked, and turning the setting on is the asking.** With diagnostics
//! off -- the default -- no file is ever produced: not on error, not on panic, not on exit. With
//! them on, a report is written when an operation fails, because the alternative does not work: the
//! recording lives in this process's memory, so a later `cram diag report` in a fresh process would
//! find an empty ring and describe nothing. Someone who switched recording on did so in order to
//! capture a failure, so capturing it is the instruction, and the text on the setting says as much.
//!
//! **Paths are redacted by default.** For an archiver the paths *are* the sensitive part: client
//! names, project names, personal folders. Reports are meant to be attachable to a public pull
//! request without reading them first, so by default an entry is described by its shape rather than
//! its name (see [`PathShape`]). `--full-paths` opts out for people who would rather just send the
//! literal names.
//!
//! # Two tiers, because recording is not free
//!
//! [`Detail::Basic`] needs nothing switched on and costs nothing. Everything in it -- the version,
//! the hardware profile, the failing error, the archive summary, the list of entries that failed --
//! already exists in memory at the moment of failure, so it can always be produced after the fact.
//!
//! [`Detail::Full`] records every entry as it is processed, and that is a real cost on a corpus of
//! tens of thousands of files: an event per entry, through a lock, on a path whose entire selling
//! point is speed. So it is **off by default and opt-in**, and the opt-in says so. Turn it on, do
//! the thing that went wrong again, then write the report.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// How much a report can say, which is decided by whether recording was on when it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Everything knowable after the fact. Always available.
    Basic,
    /// Basic, plus the per-entry trace recorded during the run.
    Full,
}

/// The one-line explanation shown wherever the setting is offered. Kept here, next to the code that
/// honours it, so the promise and the implementation cannot drift apart.
pub const EXPLAINER: &str = "\
Detailed diagnostics record what cram does with each file so a failure can be diagnosed later.

  * Nothing is ever sent anywhere. cram has no telemetry and does not contact any server.
    A report is a text file on your disk, and sending it is something you do by hand.
  * File and folder names are replaced by a description of their shape, so a report can be
    attached to a public bug report without leaking what you were archiving.
  * While this is on, a report file is written whenever an operation fails, and cram tells
    you where. While it is off, no file is ever written.
  * Recording costs a little speed on archives with very many files. It is off unless you
    turn it on.";

// ---------------------------------------------------------------------------------------------
// Path shapes
// ---------------------------------------------------------------------------------------------

/// Which alphabet a name is written in. Coarse on purpose.
///
/// This exists because a whole class of archiver bug is *about* the name -- an encoding mishandled,
/// a normalisation form assumed, a byte sequence that is not valid UTF-8 at all -- and a report that
/// redacted names down to nothing could never show it. Naming the script is enough to reproduce
/// those without carrying the name itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    Ascii,
    Latin,
    Greek,
    Cyrillic,
    Hebrew,
    Arabic,
    Cjk,
    Kana,
    Hangul,
    Symbol,
    Mixed,
    /// Not valid UTF-8. Almost always the interesting case.
    NotUtf8,
}

impl Charset {
    fn name(self) -> &'static str {
        match self {
            Charset::Ascii => "ascii",
            Charset::Latin => "latin",
            Charset::Greek => "greek",
            Charset::Cyrillic => "cyrillic",
            Charset::Hebrew => "hebrew",
            Charset::Arabic => "arabic",
            Charset::Cjk => "cjk",
            Charset::Kana => "kana",
            Charset::Hangul => "hangul",
            Charset::Symbol => "symbol",
            Charset::Mixed => "mixed",
            Charset::NotUtf8 => "not-utf8",
        }
    }

    fn of(name: &str) -> Charset {
        let mut seen: Option<Charset> = None;
        for ch in name.chars() {
            let c = match ch as u32 {
                0x00..=0x7f => Charset::Ascii,
                0x80..=0x24f => Charset::Latin,
                0x370..=0x3ff => Charset::Greek,
                0x400..=0x4ff => Charset::Cyrillic,
                0x590..=0x5ff => Charset::Hebrew,
                0x600..=0x6ff => Charset::Arabic,
                0x3040..=0x30ff => Charset::Kana,
                0xac00..=0xd7af | 0x1100..=0x11ff => Charset::Hangul,
                0x4e00..=0x9fff | 0x3400..=0x4dbf => Charset::Cjk,
                _ => Charset::Symbol,
            };
            seen = Some(match seen {
                None => c,
                // ASCII mixes with anything without making the name "mixed": almost every real
                // filename has an ASCII extension or separator in it, so counting that as mixed
                // would make the field say nothing.
                Some(Charset::Ascii) => c,
                Some(prev) if c == Charset::Ascii || c == prev => prev,
                Some(_) => Charset::Mixed,
            });
        }
        seen.unwrap_or(Charset::Ascii)
    }
}

/// Everything a report says about one path, with the name itself left out unless asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathShape {
    /// Lowercased extension, kept literally. An extension is not private and is usually the first
    /// thing worth knowing.
    pub ext: Option<String>,
    /// Components in the path, counting the file itself and, on Windows, the drive. So
    /// `D:\a\b\c.txt` is 4 and `a/b/c.txt` is 3; a Windows path reads one deeper than the Unix
    /// path it mirrors. What matters here is depth bugs and recursion, not cross-platform
    /// comparison, so the drive is left in rather than special-cased.
    pub depth: usize,
    /// Characters in the final component.
    pub name_len: usize,
    pub charset: Charset,
    /// Reserved device name on Windows (`CON`, `NUL`, `LPT1`, ...), which is refused by the OS
    /// whatever the archive says.
    pub reserved: bool,
    /// Trailing dot or space: legal in an archive, silently rewritten by Windows.
    pub trailing_dot_or_space: bool,
    /// Control characters in the name.
    pub control_chars: bool,
    /// Bytes in the whole path, which is what trips the 260-character limit.
    pub path_len: usize,
    /// Entry size when known.
    pub size: Option<u64>,
    /// Only ever `Some` when the user asked for full paths.
    pub literal: Option<String>,
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

impl PathShape {
    /// Describe `path`. `literal` decides whether the name survives into the report.
    pub fn of(path: &str, size: Option<u64>, literal: bool) -> PathShape {
        let norm = path.replace('\\', "/");
        let name = norm.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
        let stem_upper = name
            .split('.')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_uppercase();
        let ext = name.rsplit_once('.').and_then(|(base, e)| {
            // A leading-dot file is not an extension, it is the whole name.
            if base.is_empty() || e.is_empty() || e.len() > 16 {
                None
            } else {
                Some(e.to_ascii_lowercase())
            }
        });
        PathShape {
            ext,
            depth: norm.split('/').filter(|s| !s.is_empty()).count(),
            name_len: name.chars().count(),
            charset: Charset::of(name),
            reserved: RESERVED.contains(&stem_upper.as_str()),
            trailing_dot_or_space: name.ends_with('.') || name.ends_with(' '),
            control_chars: name.chars().any(|c| c.is_control()),
            path_len: path.len(),
            size,
            literal: literal.then(|| path.to_string()),
        }
    }

    fn render(&self) -> String {
        let mut s = String::new();
        match &self.literal {
            Some(p) => s.push_str(p),
            None => {
                s.push_str("ext=");
                s.push_str(self.ext.as_deref().unwrap_or("(none)"));
            }
        }
        if let Some(n) = self.size {
            s.push_str(&format!("  size={n}"));
        }
        s.push_str(&format!("  depth={}", self.depth));
        if self.literal.is_none() {
            s.push_str(&format!(
                "\n    name: len={} {}",
                self.name_len,
                self.charset.name()
            ));
            let mut flags = Vec::new();
            if self.reserved {
                flags.push("RESERVED-DEVICE-NAME");
            }
            if self.trailing_dot_or_space {
                flags.push("TRAILING-DOT-OR-SPACE");
            }
            if self.control_chars {
                flags.push("CONTROL-CHARS");
            }
            if self.path_len > 255 {
                flags.push("LONG-PATH");
            }
            s.push_str(if flags.is_empty() { " ok" } else { " <<" });
            if !flags.is_empty() {
                s.push_str(&flags.join(" "));
            }
        }
        s
    }
}

// ---------------------------------------------------------------------------------------------
// Message scrubbing
// ---------------------------------------------------------------------------------------------

/// Replace path-looking runs in free text with `<path>`.
///
/// Redacting entry names is pointless if an error message carries the same name through in prose,
/// and backend errors do exactly that. Rust's own `io::Error` usually does not embed the path, but
/// unrar and sevenz messages sometimes do, and a `Backend(String)` can hold anything at all.
///
/// Deliberately conservative: it strips runs that clearly look like filesystem paths and leaves
/// everything else alone, because an over-eager scrubber that eats the error text produces a report
/// nobody can read, which is a worse failure than a leaked directory name.
pub fn scrub(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let bytes: Vec<char> = msg.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let drive = i + 1 < bytes.len()
            && bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == ':'
            && i + 2 < bytes.len()
            && (bytes[i + 2] == '\\' || bytes[i + 2] == '/');
        let unix = bytes[i] == '/' && i + 1 < bytes.len() && !bytes[i + 1].is_whitespace();
        let unc = bytes[i] == '\\' && i + 1 < bytes.len() && bytes[i + 1] == '\\';
        if drive || unix || unc {
            let mut j = i;
            let mut seps = 0;
            while j < bytes.len()
                && !bytes[j].is_whitespace()
                && bytes[j] != '"'
                && bytes[j] != '\''
            {
                if bytes[j] == '/' || bytes[j] == '\\' {
                    seps += 1;
                }
                j += 1;
            }
            // A lone "/" or a bare "and/or" is not a path. Windows roots are self-evident.
            if drive || unc || seps >= 2 {
                out.push_str("<path>");
                i = j;
                continue;
            }
        }
        out.push(bytes[start]);
        i = start + 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Event {
    /// An operation began.
    Op {
        verb: String,
        detail: String,
    },
    /// One entry was handled. Only recorded at [`Detail::Full`].
    Entry {
        shape: PathShape,
        outcome: &'static str,
    },
    /// An entry failed. Always recorded, at either tier, because failures are rare and are the
    /// point.
    Failed {
        shape: PathShape,
        message: String,
    },
    /// A named measurement, e.g. a stage timing or a lane count.
    Metric {
        name: String,
        value: String,
    },
    Note(String),
}

/// The recorder. One per process; cheap to consult when off.
pub struct Diag {
    /// Checked on every entry, so it is an atomic rather than a lock.
    full: AtomicBool,
    full_paths: AtomicBool,
    ring: Mutex<VecDeque<Event>>,
    dropped: AtomicU64,
    cap: usize,
    /// How the archive in play is put together. Set by the format backend, which is the only thing
    /// that knows, and read when a report is written. One string rather than a ring entry because
    /// the newest one wins and the old ones are noise.
    archive: Mutex<Option<String>>,
    /// What is running right now, mirrored to disk once a second. The ring above lives only in
    /// memory, so a process that dies without unwinding -- a stack overflow is not a panic, and
    /// takes the ring with it -- would otherwise leave nothing at all behind.
    checkpoint: Mutex<Option<CheckpointState>>,
    /// Milliseconds since `process_start` at the last checkpoint write. Read on every tick, so it
    /// is an atomic and not part of the lock above.
    checkpoint_last_ms: AtomicU64,
    process_start: std::time::Instant,
}

static DIAG: OnceLock<Diag> = OnceLock::new();

/// The process-wide recorder.
pub fn diag() -> &'static Diag {
    DIAG.get_or_init(|| Diag {
        full: AtomicBool::new(false),
        full_paths: AtomicBool::new(false),
        ring: Mutex::new(VecDeque::new()),
        dropped: AtomicU64::new(0),
        // Enough to cover a failing run's neighbourhood without letting a long job hold a
        // gigabyte of strings.
        cap: 20_000,
        archive: Mutex::new(None),
        checkpoint: Mutex::new(None),
        checkpoint_last_ms: AtomicU64::new(0),
        process_start: std::time::Instant::now(),
    })
}

impl Diag {
    /// Turn per-entry recording on. Off is the default and stays the default; this is the opt-in.
    pub fn set_full(&self, on: bool) {
        self.full.store(on, Ordering::Relaxed);
    }
    /// Keep literal paths in reports instead of shapes.
    pub fn set_full_paths(&self, on: bool) {
        self.full_paths.store(on, Ordering::Relaxed);
    }
    pub fn is_full(&self) -> bool {
        self.full.load(Ordering::Relaxed)
    }
    pub fn keeps_full_paths(&self) -> bool {
        self.full_paths.load(Ordering::Relaxed)
    }
    pub fn detail(&self) -> Detail {
        if self.is_full() {
            Detail::Full
        } else {
            Detail::Basic
        }
    }

    fn push(&self, e: Event) {
        let Ok(mut ring) = self.ring.lock() else {
            return; // a poisoned ring must never take the operation down with it
        };
        if ring.len() >= self.cap {
            ring.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ring.push_back(e);
    }

    /// Record an entry. **Returns immediately when recording is off**, which is the whole reason
    /// the flag is an atomic: this sits in the per-entry path of every operation.
    pub fn entry(&self, path: &str, size: Option<u64>, outcome: &'static str) {
        if !self.is_full() {
            return;
        }
        let shape = PathShape::of(path, size, self.keeps_full_paths());
        self.push(Event::Entry { shape, outcome });
    }

    /// Record a failure. Kept at both tiers: failures are rare, and a report that omits them would
    /// be pointless.
    pub fn failed(&self, path: &str, size: Option<u64>, message: &str) {
        let shape = PathShape::of(path, size, self.keeps_full_paths());
        self.push(Event::Failed {
            shape,
            message: scrub(message),
        });
    }

    pub fn op(&self, verb: impl Into<String>, detail: impl Into<String>) {
        self.push(Event::Op {
            verb: verb.into(),
            detail: scrub(&detail.into()),
        });
    }

    pub fn metric(&self, name: impl Into<String>, value: impl Into<String>) {
        self.push(Event::Metric {
            name: name.into(),
            value: value.into(),
        });
    }

    pub fn note(&self, msg: impl Into<String>) {
        self.push(Event::Note(scrub(&msg.into())));
    }

    /// Describe the archive being read or written. Costs one small string per archive, so it is
    /// recorded whether or not detailed diagnostics are on: it is the section a maintainer reads
    /// first, and it is exactly what a reporter cannot be asked to work out by hand.
    pub fn set_archive(&self, summary: impl Into<String>) {
        if let Ok(mut a) = self.archive.lock() {
            *a = Some(summary.into());
        }
    }

    pub fn archive_summary(&self) -> Option<String> {
        self.archive.lock().ok().and_then(|a| a.clone())
    }

    /// Everything recorded so far, oldest first.
    pub fn events(&self) -> Vec<Event> {
        self.ring
            .lock()
            .map(|r| r.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Forget everything. Used between operations in a long-lived process such as Studio, so a
    /// report describes the failure the user is reporting rather than the whole session.
    pub fn clear(&self) {
        if let Ok(mut r) = self.ring.lock() {
            r.clear();
        }
        if let Ok(mut a) = self.archive.lock() {
            *a = None;
        }
        self.dropped.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------
// Writing a report
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// The stored setting
// ---------------------------------------------------------------------------------------------

/// Where the on/off setting lives, next to `profile.toml` and `mounts.txt`.
///
/// **Deliberately shared between the CLI and Studio.** Two settings files would mean turning
/// diagnostics on in the app and finding the command line had never heard about it, which is the
/// kind of thing that wastes a bug reporter's afternoon.
pub fn settings_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| Path::new(&a).join("cram").join("settings.txt"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
            .map(|b| b.join("cram").join("settings.txt"))
    }
}

/// Is detailed recording switched on? Absent or unreadable means off, because the safe reading of
/// "I cannot tell" is "do not record".
pub fn detailed_enabled() -> bool {
    let Some(p) = settings_path() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return false;
    };
    text.lines()
        .filter_map(|l| l.split_once('='))
        .any(|(k, v)| k.trim() == "diagnostics" && v.trim() == "detailed")
}

/// Persist the setting and apply it to this process.
pub fn set_detailed(on: bool) -> crate::error::Result<PathBuf> {
    let path = settings_path().ok_or_else(|| {
        crate::error::ArchiveError::Backend(
            "no per-user config directory to store the setting in".into(),
        )
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Preserve any other keys: this file is shared with whatever settings come later, and a
    // hand-edited line should survive a toggle.
    let mut kept: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            !l.split_once('=')
                .map(|(k, _)| k.trim() == "diagnostics")
                .unwrap_or(false)
        })
        .map(str::to_string)
        .collect();
    kept.retain(|l| !l.trim().is_empty());
    kept.push(format!(
        "diagnostics={}",
        if on { "detailed" } else { "off" }
    ));
    let tmp = path.with_extension("txt.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        for line in &kept {
            writeln!(f, "{line}")?;
        }
        f.flush()?;
    }
    std::fs::rename(&tmp, &path)?;
    diag().set_full(on);
    Ok(path)
}

/// Apply the stored setting to this process. Called once at start-up, before any work.
pub fn apply_stored_setting() {
    if detailed_enabled() {
        diag().set_full(true);
    }
    // Claim any checkpoint left by a run that died, here rather than lazily when a report is
    // written: adoption is what clears the file, and doing it at a known point at start-up keeps it
    // from happening halfway through a session, or not at all.
    let _ = adopt_stale_checkpoints();
}

/// The machine section of a report. cram picks thread counts, chunk lanes and pack sizes from these
/// numbers, so a fault can hang on them as much as on the archive.
pub fn machine_block() -> String {
    let hw = crate::hw::HwProfile::detect();
    let mut s = String::new();
    s.push_str(&format!(
        "cores         {} logical, {} physical{}\n",
        hw.logical,
        hw.physical,
        if hw.smt { ", SMT" } else { "" }
    ));
    s.push_str(&format!(
        "memory        {:.1} GiB total, {:.1} GiB available\n",
        hw.ram_total as f64 / (1024.0 * 1024.0 * 1024.0),
        hw.ram_avail as f64 / (1024.0 * 1024.0 * 1024.0),
    ));
    // The drive matters because extraction is write-bound: "slow" on a spinning disk and "slow" on
    // NVMe are different bugs.
    match &hw.work_drive {
        Some(d) => s.push_str(&format!(
            "work drive    disk {}, {}, {:?} bus\n",
            d.number,
            match d.ssd {
                Some(true) => "solid state",
                Some(false) => "rotational",
                None => "unknown type",
            },
            d.bus
        )),
        None => s.push_str("work drive    unknown\n"),
    }
    s
}

/// Where reports go. Alongside the other per-user state, so someone who found one can find them
/// all.
pub fn report_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|a| Path::new(&a).join("cram").join("diagnostics"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| Path::new(&h).join(".local").join("state"))
            })
            .map(|b| b.join("cram").join("diagnostics"))
    }
}

/// Build the report text. Separated from writing it so it can be tested, and so Studio can show it
/// before anything touches the disk -- a user who is about to attach this to a public issue should
/// be able to read it first.
pub fn render(header: &ReportHeader) -> String {
    let d = diag();
    let mut s = String::new();
    s.push_str("cram diagnostic report\n");
    s.push_str("======================\n\n");
    s.push_str(
        "This file was written locally and has not been sent anywhere. cram has no telemetry.\n\
         Attach it to a bug report yourself if you want it looked at.\n\n",
    );
    if d.keeps_full_paths() {
        s.push_str(
            "!! FULL PATHS ARE INCLUDED. This report contains real file and folder names,\n\
             !! because it was produced with --full-paths. Read it before sending it.\n\n",
        );
    } else {
        s.push_str(
            "File and folder names are described by shape, not included literally.\n\
             Re-run with --full-paths if a maintainer asks for the real names.\n\n",
        );
    }

    s.push_str("-- build --------------------------------------------------------------\n");
    s.push_str(&format!("cram          {}\n", header.version));
    s.push_str(&format!("features      {}\n", header.features));
    s.push_str(&format!("platform      {} {}\n", header.os, header.arch));
    s.push_str(&format!("detail        {:?}\n\n", d.detail()));

    s.push_str("-- machine ------------------------------------------------------------\n");
    s.push_str(
        "cram sizes its thread counts, chunk lanes and pack sizes from the machine, so a\n\
         fault can depend on this section as much as on the archive.\n",
    );
    s.push_str(&header.machine);
    s.push('\n');

    // The headline, when there is one. A run that died without unwinding wrote no events and left
    // no error, so this is the only thing in the report that can say what was happening -- and it
    // is why "cram just vanished" is answerable at all.
    let adopted = adopt_stale_checkpoints();
    if !adopted.is_empty() {
        s.push_str("-- a previous run did not finish --------------------------------------\n");
        s.push_str(
            "cram left a checkpoint behind, which only happens when it stopped without being\n\
             able to tidy up: killed, out of memory, or a crash that unwinds nothing.\n\n",
        );
        for c in adopted {
            s.push_str(c);
            s.push('\n');
        }
    }

    // The caller's operation string wins; otherwise fall back to the last recorded command, so a
    // report written with recording off still says what was run.
    let operation = if header.operation.is_empty() {
        d.events()
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Op { verb, detail } if verb == "command" => Some(detail.clone()),
                _ => None,
            })
            .unwrap_or_default()
    } else {
        header.operation.clone()
    };
    if !operation.is_empty() {
        s.push_str("-- operation ----------------------------------------------------------\n");
        s.push_str(&scrub(&operation));
        s.push_str("\n\n");
    }

    // Same fallback as the operation: the backend knows the archive's shape and the caller does
    // not, so whatever it recorded stands in when the caller had nothing.
    let archive = if header.archive.is_empty() {
        d.archive_summary().unwrap_or_default()
    } else {
        header.archive.clone()
    };
    if !archive.is_empty() {
        s.push_str("-- archive ------------------------------------------------------------\n");
        s.push_str(&scrub(&archive));
        s.push('\n');
    }

    if let Some(err) = &header.error {
        s.push_str("-- error --------------------------------------------------------------\n");
        s.push_str(&scrub(err));
        s.push_str("\n\n");
    }

    let events = d.events();
    let failures: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Failed { .. }))
        .collect();
    if !failures.is_empty() {
        s.push_str("-- failed entries -----------------------------------------------------\n");
        for e in &failures {
            if let Event::Failed { shape, message } = e {
                s.push_str(&format!("{}\n    {}\n", shape.render(), message));
            }
        }
        s.push('\n');
    }

    let metrics: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Metric { .. }))
        .collect();
    if !metrics.is_empty() {
        s.push_str("-- measurements -------------------------------------------------------\n");
        for e in &metrics {
            if let Event::Metric { name, value } = e {
                s.push_str(&format!("{name:<28} {value}\n"));
            }
        }
        s.push('\n');
    }

    if d.detail() == Detail::Full {
        s.push_str("-- trace --------------------------------------------------------------\n");
        if d.dropped() > 0 {
            s.push_str(&format!(
                "({} earlier events dropped; the ring keeps the most recent {})\n",
                d.dropped(),
                d.cap
            ));
        }
        for e in &events {
            match e {
                Event::Op { verb, detail } => s.push_str(&format!("[op]   {verb} {detail}\n")),
                Event::Entry { shape, outcome } => {
                    s.push_str(&format!("[{outcome}] {}\n", shape.render()))
                }
                Event::Failed { shape, message } => {
                    s.push_str(&format!("[FAIL] {}\n    {message}\n", shape.render()))
                }
                Event::Note(m) => s.push_str(&format!("[note] {m}\n")),
                Event::Metric { .. } => {}
            }
        }
    } else {
        s.push_str("-- trace --------------------------------------------------------------\n");
        s.push_str(
            "Not recorded. Detailed diagnostics were off during this run, so only what could\n\
             be reconstructed afterwards is above. To capture a per-entry trace: turn detailed\n\
             diagnostics on, reproduce the problem, then write a new report.\n",
        );
    }
    s
}

/// What only the caller can know: it owns the version constant, the hardware profile and the
/// operation that just failed.
#[derive(Debug, Default, Clone)]
pub struct ReportHeader {
    pub version: String,
    pub features: String,
    pub os: String,
    pub arch: String,
    pub machine: String,
    pub operation: String,
    pub archive: String,
    pub error: Option<String>,
}

/// A filename-safe UTC stamp for a report name. Lives here rather than in the CLI because `time` is
/// already a dependency of this crate, and a colon is not a legal Windows filename character.
pub fn stamp() -> String {
    let now = time::OffsetDateTime::from(std::time::SystemTime::now());
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

// ---------------------------------------------------------------------------------------------
// The checkpoint: what survives a run that dies without unwinding
// ---------------------------------------------------------------------------------------------

/// How often the running operation rewrites its checkpoint.
const CHECKPOINT_INTERVAL_MS: u64 = 1000;

/// A checkpoint not rewritten within this long belongs to a process that is no longer updating it,
/// which is the only portable way to tell a crashed run from a concurrently running one. A live run
/// touches its file every second, so the margin is generous by a factor of sixty.
const CHECKPOINT_STALE_MS: u128 = 60_000;

/// The live checkpoint for this process. Per-pid, so a second cram running alongside cannot delete
/// or overwrite the evidence of the first.
fn checkpoint_path() -> Option<PathBuf> {
    report_dir().map(|d| d.join(format!("running-{}.txt", std::process::id())))
}

/// State behind the checkpoint. Small, and only touched once a second.
struct CheckpointState {
    operation: String,
    phase: String,
    started: std::time::SystemTime,
}

/// The stale checkpoints adopted at start-up: what the previous run was doing when it died.
static ADOPTED: OnceLock<Vec<String>> = OnceLock::new();

impl Diag {
    /// Begin an operation. Writes the first checkpoint immediately, so a run that dies in its first
    /// second still leaves its name behind.
    pub fn checkpoint_begin(&self, operation: impl Into<String>) {
        if let Ok(mut cp) = self.checkpoint.lock() {
            *cp = Some(CheckpointState {
                operation: operation.into(),
                phase: String::new(),
                started: std::time::SystemTime::now(),
            });
        }
        self.checkpoint_last_ms.store(0, Ordering::Relaxed);
        self.write_checkpoint(0, None);
    }

    /// Name the stage now running. Phases are few, so this always writes.
    pub fn checkpoint_phase(&self, phase: impl Into<String>) {
        if let Ok(mut cp) = self.checkpoint.lock() {
            let Some(state) = cp.as_mut() else { return };
            state.phase = phase.into();
        }
        self.write_checkpoint(0, None);
    }

    /// Report progress within the current phase.
    ///
    /// **This sits in the per-item path of operations that visit hundreds of thousands of them**, so
    /// the rate limit is checked with one relaxed atomic load before anything is locked, formatted
    /// or written. In the overwhelmingly common case this function is that load and a comparison.
    pub fn checkpoint_tick(&self, done: u64, current: Option<&Path>) {
        let now = self.process_start.elapsed().as_millis() as u64;
        let last = self.checkpoint_last_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < CHECKPOINT_INTERVAL_MS {
            return;
        }
        // Losing this race means another thread is writing the checkpoint right now, which is just
        // as good as writing it here.
        if self
            .checkpoint_last_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.write_checkpoint(done, current);
    }

    /// The operation finished under its own power. Removes the checkpoint, which is what makes a
    /// leftover one mean "this run died".
    pub fn checkpoint_end(&self) {
        if let Ok(mut cp) = self.checkpoint.lock() {
            *cp = None;
        }
        if let Some(p) = checkpoint_path() {
            let _ = std::fs::remove_file(p);
        }
    }

    fn write_checkpoint(&self, done: u64, current: Option<&Path>) {
        let Some(path) = checkpoint_path() else {
            return;
        };
        let Ok(cp) = self.checkpoint.lock() else {
            return;
        };
        let Some(state) = cp.as_ref() else { return };

        let mut s = String::with_capacity(512);
        s.push_str(
            "cram was in the middle of this when the process stopped writing this file.\n\
             If cram exited normally this file is deleted, so its presence means it did not.\n\n",
        );
        s.push_str(&format!("operation     {}\n", state.operation));
        if !state.phase.is_empty() {
            s.push_str(&format!("phase         {}\n", state.phase));
        }
        s.push_str(&format!("pid           {}\n", std::process::id()));
        if let Ok(elapsed) = state.started.elapsed() {
            s.push_str(&format!("running for   {:.1}s\n", elapsed.as_secs_f64()));
        }
        if done > 0 {
            s.push_str(&format!("progress      {done} items\n"));
        }
        if let Some(c) = current {
            // Scrubbed exactly like everything else, so a checkpoint is no more revealing than the
            // report it ends up in. The shape still carries depth, which is the field that named
            // this class of failure.
            let shape = PathShape::of(&c.to_string_lossy(), None, self.keeps_full_paths());
            s.push_str(&format!("current       {}\n", shape.render()));
        }

        // Written through a temp file and renamed, so a reader never catches a half-written
        // checkpoint. Once a second, this costs nothing worth measuring.
        let tmp = path.with_extension("tmp");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::File::create(&tmp) {
            if f.write_all(s.as_bytes()).is_ok() && f.flush().is_ok() {
                drop(f);
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

/// Adopt any checkpoint left behind by a run that died, and return what they said.
///
/// Called once at start-up, before any work, so the evidence is taken before this process starts
/// overwriting it. A checkpoint still being rewritten by a live process is left alone.
pub fn adopt_stale_checkpoints() -> &'static [String] {
    ADOPTED.get_or_init(|| report_dir().map(|d| adopt_from(&d)).unwrap_or_default())
}

/// The scan behind [`adopt_stale_checkpoints`], separated from the process-wide `OnceLock` so the
/// rule it encodes -- fresh means a live run, stale means a dead one -- can actually be tested.
fn adopt_from(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        let is_checkpoint = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("running-") && n.ends_with(".txt"))
            .unwrap_or(false);
        if !is_checkpoint {
            continue;
        }
        // Age is measured from the last write, not from creation: a scan that has been running for
        // an hour has an old file, but it is still being touched, and that one is alive.
        let fresh = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.elapsed().ok())
            .map(|age| age.as_millis() < CHECKPOINT_STALE_MS)
            .unwrap_or(false);
        if fresh {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            found.push(text);
        }
        let _ = std::fs::remove_file(&path);
    }
    found
}

/// Write a report and return where it went.
///
/// Only ever called from an explicit user action. Nothing in the library calls this on failure.
pub fn write_report(header: &ReportHeader, stamp: &str) -> crate::error::Result<PathBuf> {
    let dir = report_dir().ok_or_else(|| {
        crate::error::ArchiveError::Backend("no per-user directory to write a report into".into())
    })?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("cram-diagnostic-{stamp}.txt"));
    let mut f = std::fs::File::create(&path)?;
    f.write_all(render(header).as_bytes())?;
    f.flush()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorder is process-global, which is right for one CLI invocation and wrong for a test
    /// binary running several tests at once: without this, one test's events land in another's
    /// ring and the failure looks like a bug in the code under test. Any test that touches
    /// `diag()` takes this first.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        // A panicking test must not wedge every test after it.
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A checkpoint left behind is the only evidence a hard death produces, so the rule that
    /// decides which ones to believe is worth pinning: a file still being rewritten belongs to a
    /// run that is alive, and only one that has gone quiet describes a run that died.
    ///
    /// Without this distinction the feature reports every concurrently running cram as a crash.
    #[test]
    fn only_a_checkpoint_that_stopped_being_written_counts_as_a_crash() {
        let dir = std::env::temp_dir().join(format!("cram-cp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let dead = dir.join("running-1111.txt");
        let alive = dir.join("running-2222.txt");
        let unrelated = dir.join("cram-diagnostic-20260811-120000.txt");
        std::fs::write(
            &dead,
            "operation     dedup scan
phase         walk
",
        )
        .unwrap();
        std::fs::write(
            &alive,
            "operation     create
",
        )
        .unwrap();
        std::fs::write(&unrelated, "a past report").unwrap();

        // Backdate the dead one past the staleness margin. `alive` keeps its just-now mtime, which
        // is what a process rewriting it every second looks like.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&dead)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let found = adopt_from(&dir);
        assert_eq!(found.len(), 1, "only the abandoned checkpoint is adopted");
        assert!(found[0].contains("dedup scan"), "{:?}", found[0]);

        assert!(
            !dead.exists(),
            "an adopted checkpoint is consumed, not re-reported forever"
        );
        assert!(alive.exists(), "a live run's checkpoint is left alone");
        assert!(unrelated.exists(), "past reports are not checkpoints");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shape_describes_without_naming() {
        let s = PathShape::of(r"D:\Clients\Meridian\build\auth.dll", Some(1148928), false);
        assert_eq!(s.ext.as_deref(), Some("dll"));
        assert_eq!(s.depth, 5, "D:, Clients, Meridian, build, auth.dll");
        assert_eq!(s.name_len, 8);
        assert!(s.literal.is_none());
        let out = s.render();
        assert!(out.contains("ext=dll"), "{out}");
        assert!(
            !out.contains("Meridian") && !out.contains("auth"),
            "the client name and the file name both have to be absent: {out}"
        );
    }

    #[test]
    fn full_paths_opt_in_keeps_the_name() {
        let s = PathShape::of(r"D:\Clients\Meridian\build\auth.dll", None, true);
        assert!(s.render().contains("Meridian"));
    }

    #[test]
    fn the_flags_that_are_the_bug_survive_redaction() {
        // Redacting names is worthless if it also hides the name-shaped faults.
        assert!(PathShape::of("dir/CON.txt", None, false).reserved);
        assert!(PathShape::of("dir/report.", None, false).trailing_dot_or_space);
        assert!(PathShape::of("dir/tab\there", None, false).control_chars);
        assert_eq!(
            PathShape::of("dir/Фото.jpg", None, false).charset,
            Charset::Cyrillic,
            "an encoding bug is invisible unless the script is reported"
        );
    }

    #[test]
    fn scrub_removes_paths_from_prose_but_keeps_the_sentence() {
        let got = scrub(r"failed to open D:\Clients\Meridian\x.dll: access denied");
        assert!(got.contains("access denied"), "{got}");
        assert!(!got.contains("Meridian"), "{got}");
        let unix = scrub("cannot stat /home/ada/secret/thing.txt now");
        assert!(!unix.contains("ada"), "{unix}");
        assert!(unix.contains("cannot stat"), "{unix}");
    }

    #[test]
    fn scrub_leaves_ordinary_text_alone() {
        // An over-eager scrubber that eats the message is worse than one that misses a path.
        for s in ["read/write mismatch", "and/or", "50/50", "a ratio of 1/3"] {
            assert_eq!(scrub(s), s, "scrubbed something that was not a path: {s}");
        }
    }

    #[test]
    fn the_archive_summary_reaches_a_report_without_recording_being_on() {
        // The pack layout and the create timings are the two things a "why is this slow" report
        // needs, and both are gathered during the run. If they only landed when detailed recording
        // was on, the common case -- a user who has not turned anything on -- would report nothing
        // useful.
        let _g = exclusive();
        let d = diag();
        d.clear();
        d.set_full(false);
        d.set_archive("format            .cram\npacks in archive          7\n");
        d.metric("create wall", "21.1 ms");
        let out = render(&ReportHeader::default());
        assert!(out.contains("packs in archive"), "{out}");
        assert!(out.contains("create wall"), "{out}");
        d.clear();
        assert!(
            d.archive_summary().is_none(),
            "clear() has to drop the archive too, or a report describes the previous job"
        );
    }

    #[test]
    fn recording_is_off_until_asked() {
        let _g = exclusive();
        let d = diag();
        d.clear();
        d.set_full(false);
        d.entry("some/file.txt", Some(1), "ok");
        assert!(
            d.events().is_empty(),
            "entries must not be recorded until the user opts in"
        );
        // Failures still land, because they are rare and are the reason to have a report at all.
        d.failed("some/file.txt", Some(1), "boom");
        assert_eq!(d.events().len(), 1);
        d.clear();
    }
}
