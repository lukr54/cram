//! Growing byte sources — the seam for **extract-while-download**.
//!
//! A [`ByteSource`] is a byte stream whose *contiguous prefix* grows over time: a segmented download
//! in progress publishes a **watermark** (how many bytes are available from offset 0), and the
//! streaming extractor reads that prefix, blocking at the frontier until more arrives. A completed
//! local file is just the degenerate case (watermark = length, already finished).
//!
//! This module is deliberately network-free: it only defines the abstraction + a blocking
//! [`SourceReader`] adapter, so the streaming engine ([`crate::engine::stream`]) is source-agnostic
//! and fully testable without a download. The rdm-backed implementation lives behind the `download`
//! Cargo feature and plugs in here by implementing [`ByteSource`].

use std::io::{self, Read};
use std::sync::Arc;

/// Result of waiting for a watermark on a [`ByteSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    /// At least the requested offset is now available; more may still arrive. Carries the current
    /// watermark.
    Available(u64),
    /// The source is complete — no more bytes will arrive. Carries the final total.
    Finished(u64),
    /// The download failed or was cancelled before completing. Carries the watermark reached.
    Aborted(u64),
}

/// A byte stream whose contiguous prefix grows over time. `Send + Sync` so the extractor (which may
/// run on a worker thread) can hold an `Arc<dyn ByteSource>`.
pub trait ByteSource: Send + Sync {
    /// Total size once known (`None` while still being determined).
    fn total(&self) -> Option<u64>;

    /// Bytes contiguously available from offset 0 (the watermark). Never blocks.
    fn available(&self) -> u64;

    /// Read up to `buf.len()` bytes at `offset` from the already-available prefix. May return a
    /// short read at the frontier; returns `Ok(0)` only if `offset >= available()`. Never blocks.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    /// Block until the watermark reaches `want`, the source finishes, or it aborts.
    fn wait_until(&self, want: u64) -> SourceStatus;
}

/// A blocking [`Read`] over a [`ByteSource`]: serves the available prefix and, at the frontier,
/// waits for the watermark to advance. Feeding this to the existing tar/codec stack gives
/// extract-while-download for free (the backend just sees a `Read` that occasionally blocks).
pub struct SourceReader {
    source: Arc<dyn ByteSource>,
    pos: u64,
}

impl SourceReader {
    pub fn new(source: Arc<dyn ByteSource>) -> Self {
        Self { source, pos: 0 }
    }

    /// Current read position (bytes consumed so far).
    pub fn position(&self) -> u64 {
        self.pos
    }
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.source.available() {
                let n = self.source.read_at(self.pos, buf)?;
                self.pos += n as u64;
                return Ok(n);
            }
            // At the frontier — block until more bytes, completion, or abort.
            match self.source.wait_until(self.pos + 1) {
                SourceStatus::Available(_) => continue, // re-check available() and read
                SourceStatus::Finished(total) => {
                    if self.pos >= total {
                        return Ok(0); // clean EOF
                    }
                    continue; // final bytes arrived with the finish signal
                }
                SourceStatus::Aborted(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "download aborted before completion",
                    ));
                }
            }
        }
    }
}

/// A fully-available in-memory source — the degenerate [`ByteSource`] (watermark = length, already
/// finished). Useful for extracting from a buffer and as a test double.
pub struct BufferSource {
    data: Vec<u8>,
}

impl BufferSource {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl ByteSource for BufferSource {
    fn total(&self) -> Option<u64> {
        Some(self.data.len() as u64)
    }
    fn available(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = (offset as usize).min(self.data.len());
        let n = (self.data.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        Ok(n)
    }
    fn wait_until(&self, _want: u64) -> SourceStatus {
        SourceStatus::Finished(self.data.len() as u64) // everything is already here
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_reader_reads_a_buffer_source_to_eof() {
        let data = b"the quick brown fox".repeat(100);
        let src: Arc<dyn ByteSource> = Arc::new(BufferSource::new(data.clone()));
        let mut r = SourceReader::new(src);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }
}
