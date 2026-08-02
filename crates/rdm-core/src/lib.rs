//! rdm engine, segmented, resumable, multi-source download with optional request headers.
//!
//! The file is split into fixed chunks held in a shared queue; each connection pulls the
//! next undone chunk (HTTP `Range`) and streams its bytes to a **dedicated writer thread**
//! through a bounded in-RAM buffer, so a slow or bursty disk never blocks the sockets. Fast
//! connections naturally take more (work-stealing). A `.rdm` sidecar journals which chunks
//! are done so an interrupted download resumes exactly where it stopped. Multiple source
//! URLs (mirrors) are spread across the connections, and per-request headers (e.g. a
//! browser `Cookie` / `Referer` / `User-Agent`) are attached to every request so downloads
//! behind a login work.
//!
//! This is the **library** crate: the standalone rdm app (CLI/GUI/native host) and Cram both
//! depend on it. Two features exist specifically for Cram's extract-while-download:
//!   - a **contiguous watermark** ([`Progress::contiguous`]), how many bytes are available
//!     from offset 0, so a consumer can stream the in-order prefix while the tail still downloads;
//!   - a **leading-edge** scheduling mode ([`download_stream`]), dedicate one connection to the
//!     lowest-index pending chunk so the frontier advances fast despite out-of-order writes.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::future::join_all;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Semaphore;

pub type Err = Box<dyn std::error::Error + Send + Sync>;

/// Positional write at an explicit offset without disturbing the file's implicit cursor. Windows
/// exposes this as `seek_write` and Unix as `write_at` (the Windows call advances the cursor, the Unix
/// one doesn't, but the writer thread always passes an explicit offset, so the two are equivalent).
trait PositionalIo {
    fn pwrite(&self, buf: &[u8], off: u64) -> std::io::Result<usize>;
}

impl PositionalIo for File {
    #[cfg(windows)]
    fn pwrite(&self, buf: &[u8], off: u64) -> std::io::Result<usize> {
        std::os::windows::fs::FileExt::seek_write(self, buf, off)
    }
    #[cfg(unix)]
    fn pwrite(&self, buf: &[u8], off: u64) -> std::io::Result<usize> {
        std::os::unix::fs::FileExt::write_at(self, buf, off)
    }
}

pub mod discover;
pub use discover::{discover, Discovered};

/// Size of the in-RAM write-back buffer that decouples network receipt from disk writes. Fetch
/// tasks read continuously into this buffer while a dedicated thread drains it to disk, so a slow
/// or bursty disk (e.g. a DRAM-less QLC SSD whose SLC cache has filled) throttles the connections
/// *smoothly* through backpressure instead of stalling every connection at once on a blocking
/// `WriteFile`. Bigger = absorbs longer disk hiccups at the cost of RAM.
const WRITE_BUFFER_BYTES: usize = 256 * 1024 * 1024;

/// Adaptive ramping (used only when a caller opts in): begin with a small fleet, then grow it while
/// throughput keeps climbing. `RAMP_START` = initial connection count; `RAMP_INTERVAL` = how long to
/// let a new batch ramp (TCP slow-start) before judging it; `RAMP_GAIN` = the minimum throughput
/// improvement a doubling must produce to count as "still helping"; `RAMP_STALLS` = how many
/// *consecutive* below-gain intervals to tolerate before settling. Hysteresis matters because the
/// first interval after a doubling often catches the new connections still in slow-start, one flat
/// sample must NOT be mistaken for the plateau (that stranded a 10G line at 16 conns in testing).
const RAMP_START: usize = 8;
const RAMP_INTERVAL: Duration = Duration::from_millis(1000);
const RAMP_GAIN: f64 = 1.08;
const RAMP_STALLS: u32 = 2;

/// A unit of work for the dedicated disk-writer thread.
enum WriteMsg {
    /// Bytes to write at an absolute file offset.
    Data { offset: u64, bytes: Bytes },
    /// Chunk `idx` has been fully streamed; every `Data` for it precedes this message, so when the
    /// writer reaches it the whole chunk is on disk, safe to mark done and resume-journal.
    Complete { idx: usize },
}

/// Closes the write-back semaphore on drop, so however the writer thread exits; a clean drain, a
/// disk error, or a panic (e.g. a poisoned lock), any fetch task blocked waiting for buffer room is
/// woken (its `acquire` errors) and bails, instead of hanging the whole download forever.
struct SemCloser<'a>(&'a Semaphore);
impl Drop for SemCloser<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Marks the progress finished on **every** exit of the download loop, success, error, an early `?`
/// return, or the future being dropped. Without this a streaming consumer (Cram's extract-while-
/// download) parked in [`Progress::wait_contiguous`] would hang forever whenever `run` bailed before
/// reaching its finish point (a failed probe/open/set_len, or the writer join/panic paths).
struct FinishGuard(Arc<Progress>);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.0.mark_finished();
    }
}

/// Shared progress + control for a download. Besides the total-bytes counter (`done`, incremented
/// out-of-order as chunks land), it publishes the **contiguous watermark** (`contiguous`); the
/// in-order prefix length, plus a `finished` flag and a condvar so a streaming consumer can block
/// until the watermark advances.
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    /// Contiguous bytes available from offset 0 (the streaming watermark).
    pub contiguous: AtomicU64,
    /// High-water mark of concurrent connections the engine settled on (informational; grows during
    /// adaptive ramping, or equals the fixed count for a non-ramp download).
    pub peak_conns: AtomicUsize,
    /// Set once the download loop exits (complete or gave up), no more bytes will arrive.
    finished: AtomicBool,
    wm_lock: Mutex<()>,
    wm_cond: Condvar,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            contiguous: AtomicU64::new(0),
            peak_conns: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            wm_lock: Mutex::new(()),
            wm_cond: Condvar::new(),
        }
    }
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        // wake any streaming waiter so it can observe the cancel/abort promptly.
        let _g = self.wm_lock.lock().unwrap();
        self.wm_cond.notify_all();
    }

    /// Current contiguous watermark (bytes available from offset 0).
    pub fn contiguous(&self) -> u64 {
        self.contiguous.load(Ordering::Relaxed)
    }
    /// Whether the download loop has exited (no more bytes will arrive).
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
    /// Advance the watermark and wake streaming waiters. Only the download loop calls this.
    fn set_contiguous(&self, v: u64) {
        let _g = self.wm_lock.lock().unwrap();
        if v > self.contiguous.load(Ordering::Relaxed) {
            self.contiguous.store(v, Ordering::Relaxed);
            self.wm_cond.notify_all();
        }
    }
    /// Signal that no more bytes will arrive (download done or abandoned).
    fn mark_finished(&self) {
        let _g = self.wm_lock.lock().unwrap();
        self.finished.store(true, Ordering::Relaxed);
        self.wm_cond.notify_all();
    }
    /// Block until the watermark reaches `want`, the download finishes, or it is cancelled.
    /// Returns `(contiguous, finished_or_cancelled)`.
    pub fn wait_contiguous(&self, want: u64) -> (u64, bool) {
        let mut g = self.wm_lock.lock().unwrap();
        loop {
            let c = self.contiguous.load(Ordering::Relaxed);
            let done = self.finished.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed);
            if c >= want || done {
                return (c, done);
            }
            g = self.wm_cond.wait(g).unwrap();
        }
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    total: u64,
    chunk: u64,
    done: Vec<bool>,
}

fn apply(
    mut req: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req
}

/// (total_size, supports_ranges). Tries HEAD, then a 1-byte Range GET.
///
/// A status that says the resource is not there ends the probe. Without that, a 404's own
/// `Content-Length` was taken as the file size and the chunk workers spent the whole per-chunk
/// attempt budget, with escalating backoff, discovering one range at a time what the first response
/// already said. Note what is deliberately NOT checked: a 200 carrying an HTML error page has served
/// that body as the resource, and there is no signal here distinguishing it from a real one.
pub async fn probe(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<(u64, bool), Err> {
    if let Ok(resp) = apply(client.head(url), headers).send().await {
        let status = resp.status().as_u16();
        // Only 404/410 are conclusive from a HEAD. Everything else falls through to the range GET,
        // because plenty of servers answer HEAD with 405, and a URL presigned for GET alone answers
        // 403 while still serving the file.
        if status == 404 || status == 410 {
            return Err(format!("HTTP {status}: no such file at {url}").into());
        }
        if resp.status().is_success() {
            let len = resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let ranges = resp
                .headers()
                .get(ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.contains("bytes"))
                .unwrap_or(false);
            if len > 0 {
                return Ok((len, ranges));
            }
        }
    }
    let resp = apply(client.get(url).header(RANGE, "bytes=0-0"), headers)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}: cannot fetch {url}", resp.status().as_u16()).into());
    }
    let is206 = resp.status().as_u16() == 206;
    let total = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit('/').next().map(str::to_string))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    Ok((total, is206))
}

fn sidecar_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".rdm");
    PathBuf::from(s)
}

/// Write the resume sidecar **atomically** (temp sibling + rename) so a crash / power-loss mid-write
/// can't leave a torn, unparseable `.rdm` that would force a needless full re-download.
fn write_sidecar(scp: &Path, snap: &Sidecar) {
    let Ok(j) = serde_json::to_vec(snap) else {
        return;
    };
    let mut tmp = scp.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if std::fs::write(&tmp, j).is_ok() {
        let _ = std::fs::rename(&tmp, scp);
    }
}

/// Stream one chunk's bytes into the writer channel, the bytes are handed to the dedicated writer
/// thread and never written on this async task, so a blocking/slow disk can't stall the socket.
/// `prog.done` advances per received buffer for a smooth, byte-granular speed readout, and is rolled
/// back if the stream fails partway so a retried chunk isn't double-counted. On success a `Complete`
/// marker follows all the chunk's data so the writer can mark it done once every byte is on disk.
#[allow(clippy::too_many_arguments)]
async fn fetch_chunk(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    tx: &UnboundedSender<WriteMsg>,
    sem: &Semaphore,
    prog: &Progress,
    committed: &AtomicBool,
    idx: usize,
    start: u64,
    end: u64,
) -> Result<(), Err> {
    let resp = apply(
        client
            .get(url)
            .header(RANGE, format!("bytes={start}-{end}")),
        headers,
    )
    .send()
    .await?;

    // Validate the server actually honored the Range. We always send one, so a conforming server
    // answers 206 Partial Content. A server that IGNORES Range answers 200 with the whole file from
    // byte 0, only correct to write when this chunk itself begins at offset 0 (a non-range source is
    // handled upstream as a single whole-file chunk). Any other case (a mirror/proxy answering 200 to
    // a mid-file range, a shifted Content-Range, or an error status) would land the wrong bytes at
    // `start`, so we reject it → the chunk is retried / left not-done rather than silently corrupting
    // the file and journalling it as complete.
    let status = resp.status().as_u16();
    if !(status == 206 || (status == 200 && start == 0)) {
        return Err(format!("chunk {idx}: server did not honor Range (HTTP {status})").into());
    }
    if status == 206 {
        if let Some(cr_start) = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range_start)
        {
            if cr_start != start {
                return Err(format!(
                    "chunk {idx}: Content-Range start {cr_start} != requested {start}"
                )
                .into());
            }
        }
    }

    let mut stream = resp.bytes_stream();
    let mut offset = start;
    let mut counted: u64 = 0; // bytes this attempt added to prog.done, rolled back if it fails.
    loop {
        // Observe cancellation mid-chunk (the outer loop only checks between chunks); with the read
        // timeout above bounding a stall, this keeps request_cancel responsive even on a big chunk.
        if prog.cancel.load(Ordering::Relaxed) {
            prog.done.fetch_sub(counted, Ordering::Relaxed);
            return Err("cancelled".into());
        }
        // Tail-redundancy race: if another worker already committed this same chunk, stop streaming
        // and roll back our bytes. This is NOT a failure (the chunk is done), return Ok so the caller
        // doesn't re-queue it; we simply lost the race to the faster mirror.
        if committed.load(Ordering::Relaxed) {
            prog.done.fetch_sub(counted, Ordering::Relaxed);
            return Ok(());
        }
        let item = match stream.next().await {
            Some(x) => x,
            None => break,
        };
        let bytes = match item {
            Ok(b) => b,
            Err(e) => {
                prog.done.fetch_sub(counted, Ordering::Relaxed);
                return Err(e.into());
            }
        };
        if bytes.is_empty() {
            continue;
        }
        // Never write past the requested range's `end`, even if the server sends more (a 200 whole
        // body on the start==0 chunk, or an over-long 206). Trailing bytes beyond `end` are discarded.
        let room = (end + 1).saturating_sub(offset);
        if room == 0 {
            break;
        }
        let take = (bytes.len() as u64).min(room) as usize;
        let bytes = if take < bytes.len() {
            bytes.slice(0..take)
        } else {
            bytes
        };
        let len = bytes.len();
        // Backpressure: wait until the write-back buffer has room. `forget()` the permits so they
        // aren't auto-released on drop, the writer returns exactly this many once the bytes land.
        let want = len.min(WRITE_BUFFER_BYTES);
        match sem.acquire_many(want as u32).await {
            Ok(p) => p.forget(),
            Err(_) => {
                // Semaphore closed = the writer is gone (finished or errored); abandon this attempt.
                prog.done.fetch_sub(counted, Ordering::Relaxed);
                return Err("disk writer stopped".into());
            }
        }
        if tx.send(WriteMsg::Data { offset, bytes }).is_err() {
            sem.add_permits(want);
            prog.done.fetch_sub(counted, Ordering::Relaxed);
            return Err("disk writer stopped".into());
        }
        offset += len as u64;
        counted += len as u64;
        prog.done.fetch_add(len as u64, Ordering::Relaxed);
    }

    // The stream ended: require the FULL requested range before committing the chunk. A short body
    // (a clean EOF before the range is satisfied, a truncating proxy, or a length-less
    // connection-close response) must NOT be marked done, or resume would skip a permanent gap and
    // the watermark would advance past bytes that aren't on disk. Roll back and fail → retry.
    if offset != end + 1 {
        prog.done.fetch_sub(counted, Ordering::Relaxed);
        return Err(format!(
            "chunk {idx}: short read, {} of {} bytes",
            offset - start,
            end + 1 - start
        )
        .into());
    }
    // Claim the chunk atomically: exactly one worker's fully-streamed copy "wins". If a duplicate
    // (tail-redundancy race) committed first in a photo-finish, roll back our count and DON'T send a
    // second Complete, the winner already marked it done and counted its bytes.
    if committed.swap(true, Ordering::Relaxed) {
        prog.done.fetch_sub(counted, Ordering::Relaxed);
        return Ok(());
    }
    // All bytes handed off; the writer sees this after every Data for the chunk → marks it done.
    let _ = tx.send(WriteMsg::Complete { idx });
    Ok(())
}

/// Parse the start byte from a `Content-Range: bytes START-END/TOTAL` header value.
fn parse_content_range_start(v: &str) -> Option<u64> {
    v.trim()
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Compute the contiguous watermark: advance `frontier` (lowest not-yet-done chunk) over `done`,
/// then publish the byte offset of the new frontier. Called under the `done` lock.
fn advance_watermark(
    done: &[bool],
    frontier: &AtomicUsize,
    chunk: u64,
    total: u64,
    prog: &Progress,
) {
    let n = done.len();
    let mut f = frontier.load(Ordering::Relaxed);
    while f < n && done[f] {
        f += 1;
    }
    frontier.store(f, Ordering::Relaxed);
    let wm = if f >= n { total } else { f as u64 * chunk };
    prog.set_contiguous(wm);
}

/// Fetch a small byte range fully into memory, used to compare two sources' content during
/// verification. Requires a 206 (a source that can't honor this range can't be striped anyway).
async fn fetch_range_bytes(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    start: u64,
    len: usize,
) -> Result<Vec<u8>, Err> {
    let end = start + len as u64 - 1;
    let resp = apply(
        client
            .get(url)
            .header(RANGE, format!("bytes={start}-{end}")),
        headers,
    )
    .send()
    .await?;
    if resp.status().as_u16() != 206 {
        return Err("probe range not honored (no 206)".into());
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Vet the source list before striping. `sources[0]` is the **anchor**, the URL the user actually
/// chose; it is always kept. Every other candidate is admitted only if it (a) reports the same total
/// size, (b) supports Range, and (c) returns byte-identical content at an interior probe offset.
///
/// This single gate is both correctness and safety: a mirror serving a *different* file, a wrong
/// mirror, a stale copy, or malware substituted on a dodgy site; fails the byte match and is dropped,
/// so it can never be merged into the output. Multi-source only ever ACCELERATES the anchor file; it
/// never substitutes a different file by "majority vote" (on a hostile page the majority could be the
/// bad copy). Returns the vetted list (anchor first), the total size, and whether Range is supported.
async fn verify_sources(
    client: &reqwest::Client,
    sources: &[String],
    headers: &[(String, String)],
) -> Result<(Vec<String>, u64, bool), Err> {
    let (total, ranges) = probe(client, &sources[0], headers).await?;
    if total == 0 {
        return Err(
            "could not determine file size (is this a direct file URL, not a page?)".into(),
        );
    }
    // A non-range anchor can't be segmented at all (single whole-file fetch), so mirrors are moot.
    if !ranges || sources.len() == 1 {
        return Ok((vec![sources[0].clone()], total, ranges));
    }

    // Compare an interior 64 KiB window (not the header/first block, which a hostile mirror could
    // fake) at a deterministic offset, clamped for small files.
    let probe_len = (64u64 * 1024).min(total);
    let probe_start = if total > probe_len {
        (total / 2).min(total - probe_len)
    } else {
        0
    };
    let anchor = Arc::new(
        fetch_range_bytes(
            client,
            &sources[0],
            headers,
            probe_start,
            probe_len as usize,
        )
        .await?,
    );

    // Verify every other candidate concurrently; keep only exact byte-for-byte matches.
    let checks = sources[1..].iter().map(|u| {
        let client = client.clone();
        let headers = headers.to_vec();
        let url = u.clone();
        let anchor = anchor.clone();
        async move {
            let (t, r) = probe(&client, &url, &headers).await.ok()?;
            if t != total || !r {
                return None;
            }
            let b = fetch_range_bytes(&client, &url, &headers, probe_start, probe_len as usize)
                .await
                .ok()?;
            (b == *anchor).then_some(url)
        }
    });

    let mut vetted = vec![sources[0].clone()];
    vetted.extend(join_all(checks).await.into_iter().flatten());
    if vetted.len() < sources.len() {
        eprintln!(
            "rdm: {} of {} sources verified identical to the anchor; the rest were dropped",
            vetted.len(),
            sources.len()
        );
    }
    Ok((vetted, total, true))
}

/// Health-tracked pool of vetted sources. Workers `acquire()` the best source for the next chunk,
/// fewest in-flight, weighted by measured throughput, skipping temporarily-benched (repeatedly
/// failing) mirrors, then report the outcome so weighting and benching adapt as the download runs.
struct SrcHealth {
    url: String,
    inflight: u32,
    fail: u32,
    bench_until: Option<Instant>,
    bps: f64,
}

struct Pool {
    inner: Mutex<Vec<SrcHealth>>,
}

impl Pool {
    /// Assumed throughput for a source we haven't measured yet, so a fresh source is neither starved
    /// nor flooded before its first sample lands.
    const DEFAULT_BPS: f64 = 12.0e6;

    fn new(sources: &[String]) -> Self {
        Pool {
            inner: Mutex::new(
                sources
                    .iter()
                    .map(|u| SrcHealth {
                        url: u.clone(),
                        inflight: 0,
                        fail: 0,
                        bench_until: None,
                        bps: 0.0,
                    })
                    .collect(),
            ),
        }
    }

    /// Pick the best source whose index is NOT in `exclude`, increment its in-flight count, and return
    /// it. Prefers the smallest estimated time-to-drain (`(inflight+1) / throughput`), so fast mirrors
    /// attract more chunks and slow ones fewer; benched mirrors are skipped unless every eligible one
    /// is benched (then retry the least-bad, never deadlock). Returns None only when `exclude` covers
    /// every source (a single-mirror download has no alternative to duplicate onto).
    fn acquire_excluding(&self, exclude: &[usize]) -> Option<(usize, String)> {
        let now = Instant::now();
        let mut v = self.inner.lock().unwrap();
        let score = |s: &SrcHealth| -> f64 {
            let bps = if s.bps > 0.0 {
                s.bps
            } else {
                Self::DEFAULT_BPS
            };
            (s.inflight as f64 + 1.0) / bps
        };
        let eligible: Vec<usize> = (0..v.len()).filter(|i| !exclude.contains(i)).collect();
        if eligible.is_empty() {
            return None;
        }
        let ready: Vec<usize> = eligible
            .iter()
            .copied()
            .filter(|&i| v[i].bench_until.is_none_or(|t| now >= t))
            .collect();
        // Prefer not-benched eligible sources; if all are benched, fall back to any eligible one.
        let candidates = if ready.is_empty() { &eligible } else { &ready };
        let pick = *candidates
            .iter()
            .min_by(|&&a, &&b| score(&v[a]).total_cmp(&score(&v[b])))
            .unwrap();
        v[pick].inflight += 1;
        Some((pick, v[pick].url.clone()))
    }

    /// Pick the best source with no exclusions. The pool is never empty, so this always succeeds.
    fn acquire(&self) -> (usize, String) {
        self.acquire_excluding(&[]).expect("pool is never empty")
    }

    /// Record a completed chunk: clear failures, un-bench, and fold the observed rate into the EWMA.
    fn release_ok(&self, i: usize, bytes: u64, secs: f64) {
        let mut v = self.inner.lock().unwrap();
        let s = &mut v[i];
        s.inflight = s.inflight.saturating_sub(1);
        s.fail = 0;
        s.bench_until = None;
        if secs > 0.0 && bytes > 0 {
            let sample = bytes as f64 / secs;
            s.bps = if s.bps > 0.0 {
                0.3 * sample + 0.7 * s.bps
            } else {
                sample
            };
        }
    }

    /// Record a failed chunk: three consecutive strikes bench the source with a growing cooldown
    /// (6..12 s) so a flapping mirror stops being handed chunks for a while, but always gets another
    /// chance once the cooldown lapses.
    fn release_fail(&self, i: usize) {
        let now = Instant::now();
        let mut v = self.inner.lock().unwrap();
        let s = &mut v[i];
        s.inflight = s.inflight.saturating_sub(1);
        s.fail += 1;
        if s.fail >= 3 {
            let secs = (2 * s.fail.min(6)) as u64;
            s.bench_until = Some(now + Duration::from_secs(secs));
        }
    }
}

/// Core download. Returns Ok(true) if fully complete, Ok(false) if interrupted/incomplete
/// (cancelled or a chunk gave up, re-run to resume), Err on setup failure. `leading_edge` dedicates
/// connection 0 to the lowest-index pending chunk (others take the highest) so the contiguous
/// watermark advances fast, used by streaming consumers; `false` keeps the original FIFO behavior.
/// `ramp` grows the connection count adaptively from a small start up to `conns` while throughput
/// improves (stopping on plateau or disk-bound); `false` spawns all `conns` immediately.
#[allow(clippy::too_many_arguments)]
async fn run(
    sources: &[String],
    out: &Path,
    conns: usize,
    chunk_mb: u64,
    prog: Arc<Progress>,
    headers: &[(String, String)],
    leading_edge: bool,
    ramp: bool,
) -> Result<bool, Err> {
    // Wakes any streaming consumer on EVERY exit below (incl. the `?` early returns); must be first.
    let _finish = FinishGuard(prog.clone());
    if sources.is_empty() {
        return Err("no sources".into());
    }
    // Keep the per-host connection pool warm so back-to-back chunk requests reuse the same TCP
    // connections (no slow-start restart, no fresh handshake per chunk); nodelay trims the
    // small-request latency between chunks. read_timeout is the important one: it bounds *inactivity*
    // on a response body, so a server that sends headers then goes silent (a stalled CDN / slow-loris,
    // which TCP keepalive can't detect because the peer keeps ACKing) errors out and the chunk is
    // retried instead of hanging the whole download forever. It is per-read, not per-request, so it
    // never penalizes a large-but-steady chunk.
    let client = reqwest::Client::builder()
        // Force HTTP/1.1. This is CRITICAL for a segmented downloader: over HTTP/2 (which reqwest
        // negotiates by default with any server that offers it, most modern CDNs do), all N chunk
        // requests get MULTIPLEXED onto a SINGLE TCP connection, subject to one connection's congestion
        // window + the server's per-connection HTTP/2 flow control. That silently collapses our
        // parallelism to single-stream throughput (the "fast server, only 100 MB/s" symptom). HTTP/1.1
        // gives each worker its own real TCP connection, separate windows, which is the entire point
        // (beats per-connection throttling + fills a high bandwidth-delay pipe). File servers all speak
        // h1, so there's no downside for downloads.
        .http1_only()
        .pool_max_idle_per_host(conns.max(1))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()?;
    // Verify + vet the source list: keep only mirrors that serve byte-identical content to the anchor
    // (sources[0], the file the user chose). This is correctness and the anti-malware / wrong-mirror
    // gate in one, a source serving a different file is dropped here, never spliced into the output.
    let (sources_vec, total, ranges) = verify_sources(&client, sources, headers).await?;
    let sources: &[String] = &sources_vec;
    prog.total.store(total, Ordering::Relaxed);

    // A source that doesn't support ranges must be fetched as ONE whole-file chunk: a segmented
    // download would send a ranged GET per chunk, each answered with the full 200 body written at the
    // wrong offset. With ranges we split into fixed chunks as usual.
    let chunk = if ranges {
        chunk_mb.max(1) * 1024 * 1024
    } else {
        total
    };
    let n = total.div_ceil(chunk) as usize;

    let scp = sidecar_path(out);
    let mut done = vec![false; n];
    // Only trust the resume bitmap if the output file is still present and full-size. A genuine
    // interrupted download was pre-sized via set_len(total); a file that is now missing or a different
    // size was deleted/truncated out from under us, so the bitmap is stale; start fresh rather than
    // reporting a zero-filled false-complete over bytes that are no longer there.
    let file_full = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0) == total;
    if file_full {
        if let Ok(bytes) = std::fs::read(&scp) {
            if let Ok(prev) = serde_json::from_slice::<Sidecar>(&bytes) {
                if prev.total == total && prev.chunk == chunk && prev.done.len() == n {
                    done = prev.done;
                }
            }
        }
    }

    // create + write but NOT truncate: on resume we reuse the existing partial file and
    // only overwrite the still-missing chunks (truncate(true) here would wipe a resumed download).
    let file = Arc::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(out)?,
    );
    file.set_len(total)?;

    let chunk_len = |i: usize| -> u64 {
        let s = i as u64 * chunk;
        ((i as u64 + 1) * chunk).min(total) - s
    };
    let already: u64 = (0..n).filter(|&i| done[i]).map(chunk_len).sum();
    prog.done.store(already, Ordering::Relaxed);

    // Frontier + initial watermark (accounts for a resumed download's already-done prefix).
    let frontier = Arc::new(AtomicUsize::new(0));
    advance_watermark(&done, &frontier, chunk, total, &prog);

    let pending: VecDeque<(usize, u32)> = (0..n).filter(|&i| !done[i]).map(|i| (i, 0u32)).collect();
    let pending = Arc::new(Mutex::new(pending));
    // Per-chunk "winner" flag (already-done chunks pre-claimed on resume): set when a chunk is fully
    // fetched, and doubled as the cancel signal a losing tail-redundancy duplicate watches. Plus a
    // per-chunk in-flight counter so idle workers at the tail can find a chunk still stuck on one
    // mirror and race a second copy of it.
    let committed: Arc<Vec<AtomicBool>> =
        Arc::new((0..n).map(|i| AtomicBool::new(done[i])).collect());
    // Per-chunk list of source indices currently fetching it (len == number in flight, ≤ 2).
    let inflight = Arc::new(Mutex::new(vec![Vec::<usize>::new(); n]));
    let done = Arc::new(Mutex::new(done));
    let headers = Arc::new(headers.to_vec());

    let conns = if !ranges { 1 } else { conns.max(1) };

    // Dedicated disk writer: owns the file handle and performs every `seek_write`, decoupling disk
    // latency from the network. Fetch tasks hand it (offset, bytes) over an unbounded channel that
    // is bounded in *bytes* by `sem`; the writer returns permits as writes land so the connections
    // can read ahead into the buffer while the disk catches up. When the writer exits (drained or
    // errored) it closes the semaphore so any fetch task blocked for buffer room wakes and bails.
    let (tx, rx) = unbounded_channel::<WriteMsg>();
    let sem = Arc::new(Semaphore::new(WRITE_BUFFER_BYTES));
    let writer = {
        let file = file.clone();
        let done = done.clone();
        let frontier = frontier.clone();
        let prog = prog.clone();
        let sem = sem.clone();
        std::thread::spawn(move || -> Result<(), Err> {
            let mut rx = rx;
            // Guarantees the semaphore is closed on every exit path (drain, error, or panic).
            let _sem_closer = SemCloser(&sem);
            let mut result: Result<(), Err> = Ok(());
            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    WriteMsg::Data { offset, bytes } => {
                        let mut w = 0usize;
                        while w < bytes.len() {
                            match file.pwrite(&bytes[w..], offset + w as u64) {
                                Ok(0) => {
                                    result = Err("disk write returned 0 bytes".into());
                                    break;
                                }
                                Ok(k) => w += k,
                                Err(e) => {
                                    result = Err(e.into());
                                    break;
                                }
                            }
                        }
                        if result.is_err() {
                            break;
                        }
                        sem.add_permits(bytes.len().min(WRITE_BUFFER_BYTES));
                    }
                    WriteMsg::Complete { idx } => {
                        let mut d = done.lock().unwrap();
                        d[idx] = true;
                        advance_watermark(&d, &frontier, chunk, total, &prog);
                    }
                }
            }
            result
        })
    };

    // Vetted sources become a health-tracked pool: workers pull the best available source per chunk
    // (fast, low-inflight, not benched) instead of being pinned to one mirror for life. A chunk whose
    // fetch fails is pushed BACK onto the queue to be retried on a *different* source, so one slow or
    // dead mirror can no longer strand a chunk and fail the whole download at 97% (the straggler bug).
    let pool = Arc::new(Pool::new(sources));
    // Per-chunk attempt budget: enough to rotate through every source several times before giving up.
    // Exhausting it leaves the chunk not-done → the download reports incomplete and resumes later,
    // rather than spinning forever on a byte range no source can serve.
    let attempt_cap = (sources.len() as u32).saturating_mul(4).max(8);

    // A worker pulls chunks (or tail-race duplicates) from the shared queue until there's no work.
    // Factored into a closure so the ramp controller below can spawn MORE of them on demand, each
    // call clones the shared handles into a fresh task.
    let spawn_worker = |w: usize| -> tokio::task::JoinHandle<()> {
        let client = client.clone();
        let headers = headers.clone();
        let prog = prog.clone();
        let pending = pending.clone();
        let tx = tx.clone();
        let sem = sem.clone();
        let pool = pool.clone();
        let committed = committed.clone();
        let inflight = inflight.clone();
        tokio::spawn(async move {
            loop {
                if prog.cancel.load(Ordering::Relaxed) {
                    break;
                }
                // Next chunk to fetch, plus the source to fetch it from; the whole decision is made
                // under the pending lock so a primary's in-flight registration is visible to the next
                // worker that finds the queue empty (otherwise idle workers scan an empty `inflight`
                // table at startup, before pops register, and exit before any tail chunk exists to
                // race). `inflight[idx]` holds the source indices currently fetching that chunk, so a
                // duplicate can pick a DIFFERENT mirror than the primary, a duplicate on the same
                // (slow) mirror would be pointless. Lock order is always pending → inflight → pool.
                let acquired = {
                    let mut q = pending.lock().unwrap();
                    // Leading-edge: conn 0 chases the frontier (lowest index); the rest take the tail
                    // (highest). Otherwise everyone takes the lowest (original FIFO).
                    let popped = if leading_edge && w != 0 {
                        q.pop_back()
                    } else {
                        q.pop_front()
                    };
                    let mut fl = inflight.lock().unwrap();
                    match popped {
                        Some((idx, attempts)) => {
                            // A fresh primary excludes nothing (fl[idx] is normally empty); a re-queued
                            // chunk whose duplicate is still in flight avoids that same source.
                            let (si, url) = pool
                                .acquire_excluding(&fl[idx])
                                .unwrap_or_else(|| pool.acquire());
                            fl[idx].push(si);
                            Some((idx, attempts, false, si, url))
                        }
                        None => {
                            // Queue empty → TAIL. Duplicate the lowest-index chunk in flight exactly
                            // once and not yet committed (len == 1 caps duplication at 2 per chunk),
                            // fetching it from a mirror the primary isn't using. No such chunk, or no
                            // alternative mirror → no useful work for this worker → exit.
                            match (0..n)
                                .find(|&i| {
                                    fl[i].len() == 1 && !committed[i].load(Ordering::Relaxed)
                                })
                                .and_then(|i| pool.acquire_excluding(&fl[i]).map(|su| (i, su)))
                            {
                                Some((i, (si, url))) => {
                                    fl[i].push(si);
                                    Some((i, 0u32, true, si, url))
                                }
                                None => None,
                            }
                        }
                    }
                };
                let Some((idx, attempts, is_race, si, url)) = acquired else {
                    break;
                };
                let start = idx as u64 * chunk;
                let end = ((idx as u64 + 1) * chunk).min(total) - 1;

                let t0 = Instant::now();
                let res = fetch_chunk(
                    &client,
                    &url,
                    headers.as_slice(),
                    &tx,
                    &sem,
                    &prog,
                    &committed[idx],
                    idx,
                    start,
                    end,
                )
                .await;
                inflight.lock().unwrap()[idx].retain(|&s| s != si);

                match res {
                    Ok(()) => pool.release_ok(si, end + 1 - start, t0.elapsed().as_secs_f64()),
                    Err(_) => {
                        pool.release_fail(si);
                        if prog.cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        // Retry on another source, but only the PRIMARY owner re-queues; a race copy
                        // failing is a no-op (the original is still in flight / will itself re-queue),
                        // and a chunk another worker just committed needs no retry.
                        if !is_race && !committed[idx].load(Ordering::Relaxed) {
                            let next = attempts + 1;
                            if next < attempt_cap {
                                // Escalating backoff so an instantly-failing source can't hot-spin.
                                let ms = (200u64 * next as u64).min(2000);
                                tokio::time::sleep(Duration::from_millis(ms)).await;
                                pending.lock().unwrap().push_front((idx, next));
                            }
                        }
                    }
                }
            }
        })
    };

    // Start the fleet. Ramping begins with a small batch and grows below; otherwise all `conns` at once.
    let ceiling = conns;
    let start = if ramp {
        RAMP_START.min(ceiling)
    } else {
        ceiling
    };
    let mut handles: Vec<tokio::task::JoinHandle<()>> = (0..start).map(&spawn_worker).collect();
    let mut active = start;
    prog.peak_conns.store(active, Ordering::Relaxed);

    // Journal the done-bitmap every 2 s for resume.
    let jdone = done.clone();
    let scp2 = scp.clone();
    let journal = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(2000)).await;
            let snap = Sidecar {
                total,
                chunk,
                done: jdone.lock().unwrap().clone(),
            };
            write_sidecar(&scp2, &snap);
        }
    });

    // Ramp + completion loop. Each tick checks for completion (so a finished download returns promptly
    // and any straggler blocked on a slow mirror gets aborted, its bytes are already on disk). Every
    // RAMP_INTERVAL it also makes a ramp decision: grow the fleet while throughput keeps improving, and
    // stop growing for good once it plateaus or the write buffer saturates (disk-bound, more
    // connections would just thrash it). Non-ramp downloads set `settled` immediately and only poll.
    let mut settled = !ramp;
    let mut baseline_rate = 0.0f64; // throughput measured just before the most recent fleet growth
    let mut stalls = 0u32; // consecutive below-gain intervals (hysteresis against slow-start noise)
    let mut last_done = prog.done.load(Ordering::Relaxed);
    let mut since_ramp = Duration::ZERO;
    let tick = Duration::from_millis(250);
    loop {
        tokio::time::sleep(tick).await;
        if prog.cancel.load(Ordering::Relaxed)
            || done.lock().unwrap().iter().all(|&d| d)
            || handles.iter().all(|h| h.is_finished())
        {
            break;
        }
        if settled || active >= ceiling {
            continue;
        }
        since_ramp += tick;
        if since_ramp < RAMP_INTERVAL {
            continue;
        }
        // Throughput over the interval, and whether the disk is now the bottleneck.
        let now_done = prog.done.load(Ordering::Relaxed);
        let rate = now_done.saturating_sub(last_done) as f64 / since_ramp.as_secs_f64();
        last_done = now_done;
        since_ramp = Duration::ZERO;
        if sem.available_permits() < WRITE_BUFFER_BYTES / 8 {
            settled = true; // disk-bound: fetch tasks are backed up on the writer, not the network
        } else if active > start && rate <= baseline_rate * RAMP_GAIN {
            // Below the gain threshold, but the batch we just added may still be in slow-start, so
            // tolerate a couple of flat intervals before declaring the real plateau.
            stalls += 1;
            if stalls >= RAMP_STALLS {
                settled = true;
            }
        } else {
            // First growth (unconditional) or a genuine improvement: double the fleet (capped at the
            // ceiling), remember this rate as the new baseline, and reset the stall counter.
            let add = active.min(ceiling - active).max(1);
            for w in active..active + add {
                handles.push(spawn_worker(w));
            }
            baseline_rate = rate;
            stalls = 0;
            active += add;
            prog.peak_conns.store(active, Ordering::Relaxed);
        }
    }
    // The spawn closure isn't used past this point; its borrows of the shared state (incl. `tx`) end
    // here so the teardown below can move `tx` to signal the writer.
    for h in &handles {
        h.abort();
    }
    for h in handles {
        let _ = h.await;
    }
    // Close the write side and drain: dropping the last sender makes the writer's recv return None;
    // it finishes any buffered writes, marks the final chunks done, then exits. Join it (off the
    // async worker) before reading the done-bitmap so the completeness check sees every landed write.
    drop(tx);
    let writer_res = tokio::task::spawn_blocking(move || writer.join())
        .await
        .map_err(|e| -> Err { format!("writer join task failed: {e}").into() })?
        .map_err(|_| -> Err { "disk writer thread panicked".into() })?;
    // Stop the journal and WAIT for it to actually finish. abort() only cancels at the next await, so
    // awaiting the handle guarantees no in-flight sidecar write can land after the remove/rewrite below
    // (which would otherwise resurrect a stray `.rdm` beside a finished download).
    journal.abort();
    let _ = journal.await;
    writer_res?; // a disk-write error surfaces here (FinishGuard still wakes any consumer on return)

    let complete = done.lock().unwrap().iter().all(|&d| d);
    if complete {
        let _ = std::fs::remove_file(&scp);
    } else {
        let snap = Sidecar {
            total,
            chunk,
            done: done.lock().unwrap().clone(),
        };
        write_sidecar(&scp, &snap);
    }
    // FinishGuard wakes any streaming consumer on drop (return), no explicit mark_finished needed.
    Ok(complete)
}

/// Core download (original FIFO scheduling). Returns Ok(true) if fully complete, Ok(false) if
/// interrupted/incomplete (re-run to resume), Err on setup failure.
pub async fn download_with(
    sources: &[String],
    out: &Path,
    conns: usize,
    chunk_mb: u64,
    prog: Arc<Progress>,
    headers: &[(String, String)],
) -> Result<bool, Err> {
    run(sources, out, conns, chunk_mb, prog, headers, false, false).await
}

/// Streaming download: same as [`download_with`] but with **leading-edge** scheduling so the
/// contiguous watermark advances quickly, for extract-while-download consumers.
pub async fn download_stream(
    sources: &[String],
    out: &Path,
    conns: usize,
    chunk_mb: u64,
    prog: Arc<Progress>,
    headers: &[(String, String)],
) -> Result<bool, Err> {
    run(sources, out, conns, chunk_mb, prog, headers, true, false).await
}

/// Auto-ramping download: same as [`download_with`] but grows the connection count adaptively from a
/// small start up to `conns` (the ceiling) while throughput keeps improving, stopping on plateau or
/// when the disk becomes the bottleneck, so the caller doesn't have to hand-tune the count.
pub async fn download_auto(
    sources: &[String],
    out: &Path,
    conns: usize,
    chunk_mb: u64,
    prog: Arc<Progress>,
    headers: &[(String, String)],
) -> Result<bool, Err> {
    run(sources, out, conns, chunk_mb, prog, headers, false, true).await
}

/// CLI convenience wrapper: prints a live progress line around the download (no headers). `ramp`
/// selects the auto-ramping engine ([`download_auto`]) instead of a fixed connection count.
pub async fn download(
    sources: &[String],
    out: &Path,
    conns: usize,
    chunk_mb: u64,
    ramp: bool,
) -> Result<(), Err> {
    let prog = Arc::new(Progress::new());
    let stop = Arc::new(AtomicBool::new(false));
    let (rp, rs) = (prog.clone(), stop.clone());
    let t0 = Instant::now();
    let printer = tokio::spawn(async move {
        let (mut last, mut last_t) = (0u64, 0.0f64);
        // Exponentially-weighted moving average of the rate. `prog.done` now advances per received
        // buffer (byte-granular) instead of in whole-chunk steps, so the raw samples are already far
        // less jumpy; the EWMA polishes out the residual sub-second jitter for a steady readout.
        let mut ewma = 0.0f64;
        let mut seeded = false;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let total = rp.total.load(Ordering::Relaxed);
            let d = rp.done.load(Ordering::Relaxed);
            let t = t0.elapsed().as_secs_f64();
            let inst = d.saturating_sub(last) as f64 / 1e6 / (t - last_t).max(0.001);
            if !seeded {
                ewma = inst;
                seeded = true;
            } else {
                ewma = 0.25 * inst + 0.75 * ewma;
            }
            let eta = if ewma > 0.1 && total > 0 {
                total.saturating_sub(d) as f64 / 1e6 / ewma
            } else {
                0.0
            };
            if total > 0 {
                print!(
                    "\r  {:.2}/{:.2} GB   {:6.1} MB/s   ETA {:5.0}s     ",
                    d as f64 / 1e9,
                    total as f64 / 1e9,
                    ewma,
                    eta
                );
                let _ = std::io::stdout().flush();
            }
            last = d;
            last_t = t;
            if rs.load(Ordering::Relaxed) {
                break;
            }
        }
    });

    println!("Starting {} ...", out.display());
    let res = if ramp {
        download_auto(sources, out, conns, chunk_mb, prog.clone(), &[]).await
    } else {
        download_with(sources, out, conns, chunk_mb, prog.clone(), &[]).await
    };
    stop.store(true, Ordering::Relaxed);
    let _ = printer.await;
    println!();
    match res {
        Ok(true) => {
            let ramped = if ramp {
                format!(
                    ", ramped to {} connections",
                    prog.peak_conns.load(Ordering::Relaxed)
                )
            } else {
                String::new()
            };
            println!(
                "Done: {} ({:.2} GB{ramped})",
                out.display(),
                prog.total.load(Ordering::Relaxed) as f64 / 1e9
            );
            Ok(())
        }
        Ok(false) => Err("incomplete, re-run the same command to resume".into()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watermark tracks the contiguous in-order prefix, not total bytes done.
    #[test]
    fn watermark_advances_only_over_the_contiguous_prefix() {
        let prog = Progress::new();
        let frontier = AtomicUsize::new(0);
        let chunk = 10u64;
        let total = 45u64; // 5 chunks: 10,10,10,10,5
        let mut done = vec![false; 5];

        // Completing chunk 1 first (out of order) must NOT advance the watermark past 0.
        done[1] = true;
        advance_watermark(&done, &frontier, chunk, total, &prog);
        assert_eq!(prog.contiguous(), 0);

        // Now chunk 0 completes → frontier jumps over 0 and 1 → watermark = 20.
        done[0] = true;
        advance_watermark(&done, &frontier, chunk, total, &prog);
        assert_eq!(prog.contiguous(), 20);

        // Finishing the rest walks to the end → watermark = total.
        done[2] = true;
        done[3] = true;
        done[4] = true;
        advance_watermark(&done, &frontier, chunk, total, &prog);
        assert_eq!(prog.contiguous(), total);
    }

    /// A finished download wakes a waiter even if it never reaches the requested offset.
    #[test]
    fn wait_contiguous_returns_on_finish() {
        let prog = Progress::new();
        prog.mark_finished();
        let (wm, done) = prog.wait_contiguous(1_000);
        assert_eq!(wm, 0);
        assert!(done);
    }
}

/// End-to-end tests that drive a real download through the dedicated-writer / semaphore-backpressure
/// path against a tiny in-process HTTP server, verifying byte-exact output, the progress counter, and
/// resume, the parts the unit tests above can't reach.
#[cfg(test)]
mod net_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Deterministic body: byte `i` = `i mod 251`. 251 is prime and coprime with the 1 MiB chunk, so
    /// any chunk written to the wrong offset (off by a multiple of the chunk size) mismatches.
    fn pat(i: u64) -> u8 {
        (i % 251) as u8
    }

    fn body(start: u64, end_inclusive: u64) -> Vec<u8> {
        (start..=end_inclusive).map(pat).collect()
    }

    /// Minimal HTTP/1.1 server serving the deterministic body, keeping connections alive so the
    /// client's pool reuse is exercised. With `support_ranges` it advertises `Accept-Ranges` and
    /// answers ranged GETs with 206; without it, it omits `Accept-Ranges` and answers every GET;
    /// even a ranged one, with a full 200 body (a Range-ignoring origin).
    async fn serve(total: u64, support_ranges: bool) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        // Accumulate until we have a full request head (\r\n\r\n); GET/HEAD have no body.
                        let head_end = loop {
                            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break p + 4;
                            }
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        };
                        let req = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                        buf.drain(..head_end);

                        let range_spec = if support_ranges {
                            req.lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                                .and_then(|l| l.split('=').nth(1))
                                .map(|s| s.trim().to_string())
                        } else {
                            None
                        };
                        let out: Vec<u8> = if req.starts_with("HEAD") {
                            let ar = if support_ranges {
                                "Accept-Ranges: bytes\r\n"
                            } else {
                                ""
                            };
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n{ar}\r\n")
                                .into_bytes()
                        } else if let Some(spec) = range_spec {
                            let mut it = spec.split('-');
                            let start: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                            let end = it
                                .next()
                                .filter(|s| !s.is_empty())
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(total - 1)
                                .min(total - 1);
                            let b = body(start, end);
                            let mut v = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\n\r\n",
                                b.len()
                            )
                            .into_bytes();
                            v.extend_from_slice(&b);
                            v
                        } else {
                            let b = body(0, total - 1);
                            let mut v =
                                format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\r\n")
                                    .into_bytes();
                            v.extend_from_slice(&b);
                            v
                        };
                        if sock.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn assert_pattern(data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            assert_eq!(*b, pat(i as u64), "byte mismatch at offset {i}");
        }
    }

    /// Full multi-connection download; size is NOT a chunk multiple so the short final
    /// chunk is exercised. Verifies byte-exact output, the counter ending exactly at total, and the
    /// watermark reaching the end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn downloads_correct_bytes_across_connections() {
        let total: u64 = 5 * 1024 * 1024 + 12_345;
        let addr = serve(total, true).await;
        let out = scratch(&format!("rdm_dl_{}.bin", addr.port()));
        let _ = std::fs::remove_file(&out);
        let prog = Arc::new(Progress::new());
        let ok = download_with(&[format!("http://{addr}/f")], &out, 4, 1, prog.clone(), &[])
            .await
            .unwrap();
        assert!(ok, "download should report complete");
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_pattern(&data);
        assert_eq!(prog.done(), total, "counter must end exactly at total");
        assert_eq!(prog.contiguous(), total, "watermark must reach total");
        assert!(!sidecar_path(&out).exists(), "sidecar removed on success");
        let _ = std::fs::remove_file(&out);
    }

    /// A file smaller than one chunk (n == 1), the single-chunk / range edge case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn single_chunk_download() {
        let total: u64 = 100 * 1024;
        let addr = serve(total, true).await;
        let out = scratch(&format!("rdm_small_{}.bin", addr.port()));
        let _ = std::fs::remove_file(&out);
        let prog = Arc::new(Progress::new());
        let ok = download_with(&[format!("http://{addr}/s")], &out, 8, 1, prog.clone(), &[])
            .await
            .unwrap();
        assert!(ok);
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_pattern(&data);
        assert_eq!(prog.done(), total);
        let _ = std::fs::remove_file(&out);
    }

    /// Resume: pre-seed a partial file + matching sidecar (chunks 0 and 2 already done, correctly
    /// written); the downloader must fetch only 1 and 3 and finish byte-exact, with the counter
    /// starting from the resumed prefix and ending at total.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resumes_from_sidecar() {
        let mib = 1024 * 1024u64;
        let total: u64 = 3 * mib + 500; // 4 chunks at 1 MiB: 1M,1M,1M,500
        let addr = serve(total, true).await;
        let out = scratch(&format!("rdm_resume_{}.bin", addr.port()));
        let _ = std::fs::remove_file(&out);
        let scp = sidecar_path(&out);
        let _ = std::fs::remove_file(&scp);

        // Pre-write chunks 0 and 2 with their CORRECT bytes; leave 1 and 3 as zeros.
        {
            let f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&out)
                .unwrap();
            f.set_len(total).unwrap();
            f.pwrite(&body(0, mib - 1), 0).unwrap();
            f.pwrite(&body(2 * mib, 3 * mib - 1), 2 * mib).unwrap();
        }
        let snap = Sidecar {
            total,
            chunk: mib,
            done: vec![true, false, true, false],
        };
        std::fs::write(&scp, serde_json::to_vec(&snap).unwrap()).unwrap();

        let prog = Arc::new(Progress::new());
        let ok = download_with(&[format!("http://{addr}/r")], &out, 4, 1, prog.clone(), &[])
            .await
            .unwrap();
        assert!(ok, "resumed download should complete");
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_pattern(&data);
        assert_eq!(
            prog.done(),
            total,
            "counter must reach total from the resumed prefix"
        );
        assert!(!scp.exists(), "sidecar removed on success");
        let _ = std::fs::remove_file(&out);
    }

    /// A source that ignores Range (200 full body, no Accept-Ranges) and is LARGER than one chunk:
    /// it must be fetched as a single whole-file chunk and land byte-exact at exactly `total` bytes,
    /// not corrupted and grown past total by writing the full body at every chunk offset.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn non_range_server_downloads_whole_file() {
        let total: u64 = 4 * 1024 * 1024 + 321; // > 1 chunk, but the server ignores Range
        let addr = serve(total, false).await;
        let out = scratch(&format!("rdm_norange_{}.bin", addr.port()));
        let _ = std::fs::remove_file(&out);
        let prog = Arc::new(Progress::new());
        let ok = download_with(&[format!("http://{addr}/n")], &out, 8, 1, prog.clone(), &[])
            .await
            .unwrap();
        assert!(ok);
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total, "no overflow past total");
        assert_pattern(&data);
        assert_eq!(prog.done(), total);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(sidecar_path(&out));
    }

    /// A stale sidecar claiming everything is done, but the output file is GONE: the size guard must
    /// reject the bitmap and re-download for real, producing correct bytes; not a zero-filled
    /// false-complete.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ignores_sidecar_when_output_missing() {
        let mib = 1024 * 1024u64;
        let total: u64 = 3 * mib + 77;
        let addr = serve(total, true).await;
        let out = scratch(&format!("rdm_stale_{}.bin", addr.port()));
        let scp = sidecar_path(&out);
        let _ = std::fs::remove_file(&out); // the dangerous precondition: no data file...
        let n = total.div_ceil(mib) as usize;
        let snap = Sidecar {
            total,
            chunk: mib,
            done: vec![true; n], // ...but a sidecar swearing it's 100% done.
        };
        std::fs::write(&scp, serde_json::to_vec(&snap).unwrap()).unwrap();

        let prog = Arc::new(Progress::new());
        let ok = download_with(&[format!("http://{addr}/x")], &out, 4, 1, prog.clone(), &[])
            .await
            .unwrap();
        assert!(ok);
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_pattern(&data); // real bytes, not zeros from a trusted-but-stale bitmap
        assert_eq!(prog.done(), total);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&scp);
    }

    #[derive(Clone, Copy)]
    enum Mode {
        /// Passes verification (correct bytes for the small interior probe) but 500s every real chunk.
        Flaky,
        /// Correct SIZE + Range support, but serves DIFFERENT bytes everywhere (wrong/stale/malicious).
        Wrong,
        /// Correct bytes, but delays every real (large) chunk by 2 s; a reachable but slow mirror.
        Slow,
        /// Correct bytes, each real chunk paced by 150 ms; a per-request-latency source where more
        /// parallel connections raise aggregate throughput (exercises adaptive ramping).
        Paced,
    }

    /// A misbehaving mirror for the multi-source tests. Both modes advertise the correct size and
    /// `Accept-Ranges` on HEAD so they get past `probe`; they diverge on the actual data.
    async fn serve_mode(total: u64, mode: Mode) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        let head_end = loop {
                            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break p + 4;
                            }
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        };
                        let req = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                        buf.drain(..head_end);
                        let range = req
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                            .and_then(|l| l.split('=').nth(1))
                            .map(|s| s.trim().to_string());
                        let fail500 =
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
                                .to_vec();
                        let out: Vec<u8> = if req.starts_with("HEAD") {
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\n\r\n").into_bytes()
                        } else if let Some(spec) = range {
                            let mut it = spec.split('-');
                            let start: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                            let end = it
                                .next()
                                .filter(|s| !s.is_empty())
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(total - 1)
                                .min(total - 1);
                            let len = end - start + 1;
                            match mode {
                                // Flaky: serve small probe ranges correctly, 500 the big real chunks.
                                Mode::Flaky if len > 128 * 1024 => fail500,
                                Mode::Flaky => {
                                    let b = body(start, end);
                                    let mut v = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\n\r\n", b.len()).into_bytes();
                                    v.extend_from_slice(&b);
                                    v
                                }
                                // Wrong: right length, bit-flipped bytes → fails the byte-identity gate.
                                Mode::Wrong => {
                                    let b: Vec<u8> = (start..=end).map(|i| pat(i) ^ 0xFF).collect();
                                    let mut v = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\n\r\n", b.len()).into_bytes();
                                    v.extend_from_slice(&b);
                                    v
                                }
                                // Slow: correct bytes, but stall the big real chunks 2 s (the small probe
                                // range stays instant so verification isn't held up).
                                Mode::Slow => {
                                    if len > 128 * 1024 {
                                        tokio::time::sleep(Duration::from_millis(2000)).await;
                                    }
                                    let b = body(start, end);
                                    let mut v = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\n\r\n", b.len()).into_bytes();
                                    v.extend_from_slice(&b);
                                    v
                                }
                                // Paced: correct bytes, each real chunk delayed 150 ms so aggregate
                                // throughput scales with the number of parallel connections.
                                Mode::Paced => {
                                    if len > 128 * 1024 {
                                        tokio::time::sleep(Duration::from_millis(150)).await;
                                    }
                                    let b = body(start, end);
                                    let mut v = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\n\r\n", b.len()).into_bytes();
                                    v.extend_from_slice(&b);
                                    v
                                }
                            }
                        } else {
                            fail500
                        };
                        if sock.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    /// Multi-source failover: a mirror that verifies OK but then fails every real chunk (reachable but
    /// broken mid-download) must not sink the download, its chunks are re-queued and completed by the
    /// healthy anchor, byte-exact. This is the fix for the "failed at 97%" straggler.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failover_past_a_broken_mirror() {
        let total: u64 = 5 * 1024 * 1024; // exact 1 MiB chunks so the broken mirror fails all of them
        let good = serve(total, true).await;
        let bad = serve_mode(total, Mode::Flaky).await;
        let out = scratch(&format!("rdm_failover_{}.bin", good.port()));
        let _ = std::fs::remove_file(&out);
        let prog = Arc::new(Progress::new());
        let ok = download_with(
            &[format!("http://{good}/g"), format!("http://{bad}/b")],
            &out,
            8,
            1,
            prog.clone(),
            &[],
        )
        .await
        .unwrap();
        assert!(ok, "download must complete despite the broken mirror");
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_pattern(&data);
        assert_eq!(prog.done(), total);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(sidecar_path(&out));
    }

    /// Verification gate: a mirror advertising the right SIZE but serving DIFFERENT bytes (a wrong or
    /// stale mirror, or malware swapped in on a dodgy page) is dropped before striping, so none of its
    /// bytes reach the output. The download completes from the anchor alone, byte-exact.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wrong_content_mirror_is_rejected() {
        let total: u64 = 4 * 1024 * 1024 + 4321;
        let good = serve(total, true).await;
        let evil = serve_mode(total, Mode::Wrong).await;
        let out = scratch(&format!("rdm_reject_{}.bin", good.port()));
        let _ = std::fs::remove_file(&out);
        let prog = Arc::new(Progress::new());
        let ok = download_with(
            &[format!("http://{good}/g"), format!("http://{evil}/e")],
            &out,
            8,
            1,
            prog.clone(),
            &[],
        )
        .await
        .unwrap();
        assert!(ok);
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_pattern(&data); // canonical bytes only, the wrong mirror contributed nothing
        assert_eq!(prog.done(), total);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(sidecar_path(&out));
    }

    /// Tail redundancy: a slow-but-alive mirror holding a tail chunk must not gate completion. Idle
    /// fast workers duplicate the in-flight chunks and win the race, so the whole download finishes far
    /// sooner than the 2 s/chunk the slow mirror would take, byte-exact, with an exact progress count
    /// (no double-count from the discarded loser), and no straggler left hanging the return.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tail_redundancy_beats_a_slow_mirror() {
        let total: u64 = 6 * 1024 * 1024; // 6 exact 1 MiB chunks
        let fast = serve(total, true).await;
        let slow = serve_mode(total, Mode::Slow).await; // 2 s per real chunk
        let out = scratch(&format!("rdm_tail_{}.bin", fast.port()));
        let _ = std::fs::remove_file(&out);
        let prog = Arc::new(Progress::new());
        let t0 = Instant::now();
        // 12 connections over 6 chunks → guaranteed idle workers to race the slow mirror's chunks.
        let ok = download_with(
            &[format!("http://{fast}/f"), format!("http://{slow}/s")],
            &out,
            12,
            1,
            prog.clone(),
            &[],
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed();
        assert!(ok);
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_pattern(&data);
        assert_eq!(
            prog.done(),
            total,
            "exact byte count, no double-count from the raced duplicate"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "tail redundancy should finish well under the 2 s slow-chunk delay, took {elapsed:?}"
        );
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(sidecar_path(&out));
    }

    /// Adaptive ramping: on a sustained, per-request-latency source (throughput scales with
    /// connections), `download_auto` grows the fleet above its small start and still finishes
    /// byte-exact, proving dynamic worker spawn/join/abort is correct.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ramping_grows_connections() {
        // Sized so it lasts through at least one ramp interval at the 8-connection start.
        let total: u64 = 120 * 1024 * 1024; // 120 × 1 MiB chunks, each paced 150 ms
        let srv = serve_mode(total, Mode::Paced).await;
        let out = scratch(&format!("rdm_ramp_{}.bin", srv.port()));
        let _ = std::fs::remove_file(&out);
        let prog = Arc::new(Progress::new());
        let ok = download_auto(
            &[format!("http://{srv}/p")],
            &out,
            16, // ceiling
            1,
            prog.clone(),
            &[],
        )
        .await
        .unwrap();
        assert!(ok);
        let data = std::fs::read(&out).unwrap();
        assert_eq!(data.len() as u64, total);
        assert_pattern(&data);
        assert_eq!(prog.done(), total);
        let peak = prog.peak_conns.load(Ordering::Relaxed);
        assert!(
            peak > RAMP_START,
            "should have ramped above the start of {RAMP_START}, peaked at {peak}"
        );
        assert!(peak <= 16, "must not exceed the ceiling, peaked at {peak}");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(sidecar_path(&out));
    }

    /// A server that answers everything with one status and an error page, the shape a hosting
    /// provider's 404 actually takes: it carries a `Content-Length`, which is what the probe used to
    /// read as the file size. Counts the requests it received, since the cost of the old behaviour
    /// was the requests, not the outcome.
    async fn serve_status(
        status: u16,
        reason: &'static str,
    ) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        let head_end = loop {
                            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break p + 4;
                            }
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        };
                        let req = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                        buf.drain(..head_end);
                        counter.fetch_add(1, Ordering::Relaxed);
                        let page = b"<html>not found</html>";
                        let mut out = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                            page.len()
                        )
                        .into_bytes();
                        if !req.starts_with("HEAD") {
                            out.extend_from_slice(page);
                        }
                        if sock.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (addr, hits)
    }

    /// A missing file is reported as missing, from the first answer. It used to be read as a 22-byte
    /// file and then rediscovered one ranged request at a time, through the whole per-chunk attempt
    /// budget and its escalating backoff, before surfacing as "the download did not complete".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_404_is_reported_as_one_and_costs_one_request() {
        let (addr, hits) = serve_status(404, "Not Found").await;
        let out = scratch(&format!("rdm_404_{}.bin", addr.port()));
        let _ = std::fs::remove_file(&out);
        let prog = Arc::new(Progress::new());

        let e = download_with(
            &[format!("http://{addr}/missing.zip")],
            &out,
            4,
            1,
            prog,
            &[],
        )
        .await
        .expect_err("a 404 must be an error, not a 22-byte download");

        assert!(
            e.to_string().contains("404"),
            "the error must name the status, said: {e}"
        );
        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "one request settles it; more than one is the retry storm"
        );
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(sidecar_path(&out));
    }
}
