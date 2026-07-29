//! Network sources, the `download` feature's bridge from rdm-core to the [`ByteSource`] interface.
//!
//! [`RdmSource`] runs a segmented rdm download on its own thread (with a tokio runtime) and exposes
//! it as a growing [`ByteSource`]: `available()` is the rdm engine's **contiguous watermark**,
//! `read_at` positional-reads the growing output file, and `wait_until` blocks on the watermark
//! condvar. Feeding this to [`crate::engine::stream::extract_stream`] gives true extract-while-
//! download, the archive is unpacked as the bytes land, no full download first.
//!
//! Only a client (outbound) download is opened here; there is **no listening socket**, so this path
//! does not carry the rdm-gui Defender false-positive concern (that was an unsigned GUI *listener*).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use rdm_core::Progress;

use crate::source::{ByteSource, SourceStatus};

/// Positional read at an explicit offset, bridging the differently-named std traits: `seek_read` on
/// Windows, `read_at` on Unix. Neither is relied on to move the file cursor here, every call passes
/// an explicit offset.
#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, off)
}
#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, off)
}

// Re-export the discovery type/predicate so callers (the CLI, a future GUI) reach them through
// cram-core without also depending on rdm-core directly.
pub use rdm_core::discover::is_metalink_ref;
pub use rdm_core::Discovered;

/// How to schedule the segmented download the [`RdmSource`] drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadMode {
    /// **Leading-edge** scheduling: the contiguous frontier is prioritised so an extract-while-
    /// download consumer can unpack bytes as they land. Fixed connection count.
    Stream,
    /// Plain full download, fixed connection count (FIFO scheduling).
    Fixed,
    /// **Adaptive ramping**: grow the connection count from a small start up to `conns` (the ceiling)
    /// while throughput keeps improving, backing off on plateau or when the disk is the bottleneck.
    Auto,
}

/// A segmented download exposed as a growing [`ByteSource`]. Construct with [`RdmSource::start`],
/// hand the `Arc<dyn ByteSource>` to the streaming extractor, and the archive unpacks as it arrives.
pub struct RdmSource {
    prog: Arc<Progress>,
    /// Read handle onto the growing output file (positional `seek_read`; separate from the engine's
    /// write handle, Windows allows the shared read+write handles).
    file: File,
    out: PathBuf,
    worker: Option<JoinHandle<()>>,
}

impl RdmSource {
    /// Begin downloading `sources` (one or more **mirrors of the same file**) into `out` over `conns`
    /// connections. `mode` selects scheduling: [`DownloadMode::Stream`] (leading-edge, for extract-
    /// while-download), [`DownloadMode::Fixed`], or [`DownloadMode::Auto`] (adaptive ramping, `conns`
    /// = the ceiling). When more than one source is given the engine byte-verifies each mirror against
    /// the anchor (`sources[0]`) before striping, a mirror serving a different file is dropped, never
    /// spliced in, and fails over / tail-races across the healthy pool. `headers` are attached to
    /// every request (browser Cookie/Referer/User-Agent). Returns immediately; bytes flow in the
    /// background.
    pub fn start(
        sources: Vec<String>,
        out: PathBuf,
        conns: usize,
        chunk_mb: u64,
        headers: Vec<(String, String)>,
        mode: DownloadMode,
    ) -> io::Result<Self> {
        let prog = Arc::new(Progress::new());

        // Ensure the file exists so our read handle can open before the first byte lands (the engine
        // reopens + set_len's it; create(true) is idempotent). `truncate(false)` preserves any
        // partial file for resume.
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&out)?;
        let file = File::open(&out)?;

        let worker = {
            let prog = prog.clone();
            let out = out.clone();
            thread::spawn(move || {
                // A small multi-thread runtime drives the async engine on this dedicated thread.
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(_) => {
                        // Can't run, signal "no more bytes" so waiters unblock (as an abort).
                        prog.cancel.store(true, Ordering::Relaxed);
                        prog.request_cancel();
                        return;
                    }
                };
                let res = match mode {
                    DownloadMode::Stream => rt.block_on(rdm_core::download_stream(
                        &sources, &out, conns, chunk_mb, prog, &headers,
                    )),
                    DownloadMode::Fixed => rt.block_on(rdm_core::download_with(
                        &sources, &out, conns, chunk_mb, prog, &headers,
                    )),
                    DownloadMode::Auto => rt.block_on(rdm_core::download_auto(
                        &sources, &out, conns, chunk_mb, prog, &headers,
                    )),
                };
                let _ = res;
                // The engine marks the Progress finished on exit (success or give-up).
            })
        };

        Ok(Self {
            prog,
            file,
            out,
            worker: Some(worker),
        })
    }

    /// The output file path (the fully-downloaded archive once finished).
    pub fn output_path(&self) -> &Path {
        &self.out
    }

    /// Request cancellation of the underlying download.
    pub fn cancel(&self) {
        self.prog.request_cancel();
    }

    /// Shared progress handle (for a live progress readout: `done()`/`total()`/`contiguous()`).
    pub fn progress(&self) -> &Arc<Progress> {
        &self.prog
    }

    /// High-water mark of concurrent connections the engine settled on. For a [`DownloadMode::Auto`]
    /// download this is where adaptive ramping plateaued; otherwise it's the fixed connection count.
    pub fn peak_conns(&self) -> usize {
        self.prog.peak_conns.load(Ordering::Relaxed)
    }

    /// Block until the download finishes (or gives up). Returns `true` if the whole file arrived.
    pub fn wait(&self) -> bool {
        let (wm, _) = self.prog.wait_contiguous(u64::MAX);
        self.prog.is_finished() && matches!(self.total(), Some(t) if wm >= t)
    }
}

impl ByteSource for RdmSource {
    fn total(&self) -> Option<u64> {
        match self.prog.total() {
            0 => None, // not yet probed
            t => Some(t),
        }
    }

    fn available(&self) -> u64 {
        self.prog.contiguous()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        // Serve only the in-order prefix [0, watermark): bytes past it may be from not-yet-arrived
        // (or out-of-order, non-contiguous) chunks and must not be handed out.
        let avail = self.prog.contiguous();
        if offset >= avail {
            return Ok(0);
        }
        let want = ((avail - offset) as usize).min(buf.len());
        pread(&self.file, &mut buf[..want], offset)
    }

    fn wait_until(&self, want: u64) -> SourceStatus {
        let (wm, _stopped) = self.prog.wait_contiguous(want);
        if wm >= want {
            return SourceStatus::Available(wm);
        }
        // Stopped before reaching `want`: distinguish a clean finish (whole file present, `want` was
        // past the end) from an aborted/cancelled download.
        let whole = self.prog.is_finished() && matches!(self.total(), Some(t) if wm >= t);
        if whole {
            SourceStatus::Finished(wm)
        } else {
            SourceStatus::Aborted(wm)
        }
    }
}

impl Drop for RdmSource {
    fn drop(&mut self) {
        // Ask the download to stop; don't block the dropping thread waiting for it.
        self.prog.request_cancel();
        if let Some(h) = self.worker.take() {
            drop(h); // detach, the worker observes the cancel and exits on its own
        }
    }
}

/// Run deterministic mirror discovery on `input`, a Metalink document (`.meta4` RFC 5854 / older
/// `.metalink`, given as a URL or a local path) or a plain URL probed for RFC 6249 Metalink/HTTP
/// `Link: rel=duplicate` headers. Returns the discovered mirrors (+ optional whole-file SHA-256), or
/// `Ok(None)` when nothing applies (the caller then downloads `input` directly). No LLM, plain HTTP +
/// parsing. Discovery only *proposes*; [`RdmSource::start`]'s verify gate still byte-checks every
/// mirror, so a bogus discovered link is harmless (it just gets dropped). Builds its own short-lived
/// runtime + client so it's callable from synchronous code (the CLI).
pub fn discover_mirrors(input: &str) -> io::Result<Option<Discovered>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(io::Error::other)?;
        rdm_core::discover(&client, input)
            .await
            .map_err(io::Error::other)
    })
}

/// Fetch a small text document over HTTPS: the GitHub release JSON and the `SHA256SUMS` file that
/// `cram update` reads. Not for payloads, those go through [`RdmSource`] so they get the segmented
/// engine, resume and the verify gate.
///
/// The body is **capped before it is read into memory**: a response is attacker-controlled in size
/// as well as content, and a `Content-Length` cannot be trusted to bound it. `accept` sets the
/// `Accept` header where the endpoint cares (GitHub's API does).
///
/// Builds its own short-lived runtime + client so it is callable from synchronous code, like
/// [`discover_mirrors`].
pub fn fetch_text(url: &str, accept: Option<&str>, max_bytes: usize) -> io::Result<String> {
    use futures_util::StreamExt;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(io::Error::other)?;
        let mut req = client
            .get(url)
            // GitHub rejects requests without one, and it is the only thing identifying us.
            .header("User-Agent", concat!("cram/", env!("CARGO_PKG_VERSION")));
        if let Some(a) = accept {
            req = req.header("Accept", a);
        }
        let resp = req.send().await.map_err(io::Error::other)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(io::Error::other(format!(
                "{url} replied {}",
                status.as_u16()
            )));
        }
        // Read the stream chunk by chunk and stop at the cap, rather than `resp.text()`, which
        // would happily buffer a body sized to exhaust memory.
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(io::Error::other)?;
            if body.len() + chunk.len() > max_bytes {
                return Err(io::Error::other(format!(
                    "{url} sent more than {max_bytes} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).map_err(|_| io::Error::other(format!("{url} sent invalid UTF-8")))
    })
}

/// Stream a finished download through SHA-256 and compare (case-insensitively) to `expected_hex`.
/// Used to verify a completed download against a checksum discovery supplied (e.g. a Metalink), so a
/// mirror set that all agreed on a *wrong* file is still caught end-to-end.
pub fn verify_sha256(path: &Path, expected_hex: &str) -> io::Result<bool> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(got.eq_ignore_ascii_case(expected_hex))
}
