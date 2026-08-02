//! One progress model for both engines. Backends report through the [`ProgressSink`] trait; the
//! concrete [`Progress`] holds atomics the GUI/CLI poll. Sequential engines wrap their output in a
//! [`CountingWriter`] to get accurate per-chunk byte progress (no output-dir watching).

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::model::Entry;

/// What a running extraction/creation reports to. `Sync` so worker threads can share `&dyn`.
pub trait ProgressSink: Sync {
    /// Bytes just written/processed.
    fn on_bytes(&self, n: u64);
    /// One entry finished.
    fn on_file_done(&self, entry: &Entry);
    /// One entry started (optional; default no-op).
    fn on_entry_start(&self, _entry: &Entry) {}
    /// Cooperative cancellation, engines check this between chunks/entries.
    fn is_cancelled(&self) -> bool;
    /// Cooperative pause. Engines call this at the same points they check
    /// [`is_cancelled`](Self::is_cancelled); a paused job blocks here until it is resumed or
    /// cancelled. Default: no-op (never pauses), so existing sinks are unaffected.
    fn wait_if_paused(&self) {}
}

/// Concrete shared progress state. Worker threads bump the counters; the UI polls them.
pub struct Progress {
    done_bytes: AtomicU64,
    done_files: AtomicU64,
    pub total_bytes: u64,
    pub total_files: u64,
    cancel: AtomicBool,
}

impl Progress {
    pub fn new(total_bytes: u64, total_files: u64) -> Self {
        Self {
            done_bytes: AtomicU64::new(0),
            done_files: AtomicU64::new(0),
            total_bytes,
            total_files,
            cancel: AtomicBool::new(false),
        }
    }
    pub fn done_bytes(&self) -> u64 {
        self.done_bytes.load(Ordering::Relaxed)
    }
    pub fn done_files(&self) -> u64 {
        self.done_files.load(Ordering::Relaxed)
    }
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    /// Fraction complete in [0.0, 1.0] by bytes.
    pub fn fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            (self.done_bytes() as f64 / self.total_bytes as f64) as f32
        }
    }
}

impl ProgressSink for Progress {
    fn on_bytes(&self, n: u64) {
        self.done_bytes.fetch_add(n, Ordering::Relaxed);
    }
    fn on_file_done(&self, _entry: &Entry) {
        self.done_files.fetch_add(1, Ordering::Relaxed);
    }
    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// A no-op sink (for `test`/`create`-to-null and benchmarks).
pub struct NullSink;
impl ProgressSink for NullSink {
    fn on_bytes(&self, _n: u64) {}
    fn on_file_done(&self, _entry: &Entry) {}
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Wraps an output writer to report bytes to a `ProgressSink` as they are written, gives
/// sequential engines (tar/7z/raw) precise per-chunk progress without watching the filesystem.
pub struct CountingWriter<'s, W: Write> {
    inner: W,
    sink: &'s dyn ProgressSink,
}

impl<'s, W: Write> CountingWriter<'s, W> {
    pub fn new(inner: W, sink: &'s dyn ProgressSink) -> Self {
        Self { inner, sink }
    }
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sink.on_bytes(n as u64);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Wraps an input reader to report bytes to a `ProgressSink` as they are consumed, the create side
/// (which reads source files *into* a writer) gets the same per-chunk progress the extract side has.
pub struct CountingReader<'s, R: Read> {
    inner: R,
    sink: &'s dyn ProgressSink,
}

impl<'s, R: Read> CountingReader<'s, R> {
    pub fn new(inner: R, sink: &'s dyn ProgressSink) -> Self {
        Self { inner, sink }
    }
}

impl<R: Read> Read for CountingReader<'_, R> {
    /// Same cancel discipline as the extract side's `ProgressWriter`: a create loop only checks
    /// cancellation between entries, so without this a single 200 GB source could not be cancelled
    /// at all until it had been read and compressed in full. Deliberately NOT
    /// `ErrorKind::Interrupted`, `io::copy` and `read_exact` silently *retry* that, which against a
    /// one-way cancel latch spins forever; `Other` unwinds the copy at once.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.sink.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.sink.on_bytes(n as u64);
        Ok(n)
    }
}
