//! Streaming extraction, extract entries from a growing [`ByteSource`] as its bytes arrive
//! (**extract-while-download**).
//!
//! Two container families stream front-to-back:
//! * **tar-family** (`.tar`/`.tar.gz`/`.tar.xz`/…), a header-then-data sequence; the decoder consumes
//!   the growing prefix and yields each entry the moment its bytes land.
//! * **zip**, physically a run of *local file header + data* records, so we read them sequentially via
//!   `zip::read::read_zipfile_from_stream` (no central directory needed). This covers STORE + DEFLATE
//!   and zip64 (large game files). Two zip shapes can't be streamed and are reported as
//!   [`ArchiveError::StreamUnsupported`] so the caller extracts them normally once fully downloaded:
//!   entries whose sizes live in a trailing **data descriptor** (general-purpose bit 3), and
//!   **encrypted** entries (the streaming reader has no password hand-off point). Real repack zips written to a
//!   file by 7-Zip/WinRAR carry their sizes in the local header, so they stream.
//!
//! 7z/rar still need the whole file (header structures / solid blocks), download to completion first.
//!
//! Mechanically this is the sequential write loop over a blocking [`SourceReader`]: when the parser
//! reaches the download frontier, the reader blocks until the watermark advances. Path safety,
//! progress, cancellation, and skip-already-correct all carry over unchanged.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use crate::codec::decode_stream;
use crate::engine::{skip, ExtractOptions, ProgressWriter};
use crate::error::{ArchiveError, Report, Result};
use crate::format::{Container, Format};
use crate::model::{Entry, EntryKind, EntryPath};
use crate::progress::ProgressSink;
use crate::source::{ByteSource, SourceReader};

const WRITE_BUF: usize = 8 * 1024 * 1024;

/// Can `fmt` be *attempted* as an extract-while-download (front-to-back streamable)? Tar-family and
/// zip. A zip may still turn out non-streamable at read time (data-descriptor sizes / encryption), in
/// which case [`extract_stream`] returns [`ArchiveError::StreamUnsupported`] and the caller falls back
/// to a normal post-download extract.
pub fn is_streamable(fmt: Format) -> bool {
    matches!(fmt.container, Container::Tar | Container::Zip)
}

/// Extract `source` (a growing download) of format `fmt` into `dest`, entry by entry as bytes
/// arrive. `fmt` comes from the caller (typically sniffed from the download URL/name). Errors if
/// `fmt` isn't stream-extractable, the caller should await completion and use the normal engine.
pub fn extract_stream(
    source: Arc<dyn ByteSource>,
    fmt: Format,
    dest: &Path,
    opts: ExtractOptions,
    sink: &dyn ProgressSink,
) -> Result<Report> {
    if !is_streamable(fmt) {
        return Err(ArchiveError::StreamUnsupported);
    }
    fs::create_dir_all(dest)?;
    match fmt.container {
        Container::Zip => extract_zip_stream(source, dest, opts, sink),
        _ => extract_tar_stream(source, fmt, dest, opts, sink), // tar-family
    }
}

/// Tar-family streaming: whole-stream codec decode → `tar` parser → sequential write loop.
fn extract_tar_stream(
    source: Arc<dyn ByteSource>,
    fmt: Format,
    dest: &Path,
    opts: ExtractOptions,
    sink: &dyn ProgressSink,
) -> Result<Report> {
    // Blocking reader over the growing prefix → the whole-stream decoder → tar.
    let decoded = decode_stream(fmt.codec, Box::new(SourceReader::new(source)))?;
    let mut archive = tar::Archive::new(decoded);
    let mut report = Report::default();

    for item in archive
        .entries()
        .map_err(|e| ArchiveError::Backend(format!("tar: {e}")))?
    {
        sink.wait_if_paused();
        if sink.is_cancelled() {
            report.cancelled = true;
            break;
        }
        let mut te = item.map_err(|e| ArchiveError::Backend(format!("tar: {e}")))?;
        let et = te.header().entry_type();
        // Same rule as the tar backend: links/devices/FIFOs/sparse members have no extractable
        // byte stream, writing them as plain files would materialize empty/garbage stand-ins.
        if !et.is_dir() && !matches!(et, tar::EntryType::Regular | tar::EntryType::Continuous) {
            continue;
        }
        let raw = match te.path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        let is_dir = et.is_dir();
        let Some(safe) = EntryPath::from_raw(&raw) else {
            report.dropped_unsafe += 1; // zip-slip name → drop, but say how many
            continue;
        };
        let entry = Entry {
            index: 0,
            path: safe,
            kind: if is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
            size: te.size(),
            compressed_size: None,
            modified: None,
            unix_mode: None,
            crc32: None,
            encrypted: false,
        };

        let outpath = entry.path.join_under(dest);
        if is_dir {
            let _ = fs::create_dir_all(&outpath);
            continue;
        }

        // skip-already-correct: a stream can't seek past the entry, so we still read its bytes off
        // the wire, but skip the decode-to-disk write. (tar carries no CRC, so this only fires for a
        // source that supplies one.)
        if opts.skip_existing && skip::dest_already_correct(&outpath, &entry) {
            let _ = io::copy(&mut te, &mut io::sink()); // drain so the parser advances
            sink.on_bytes(entry.size);
            sink.on_file_done(&entry);
            report.skipped += 1;
            continue;
        }

        sink.on_entry_start(&entry);
        // Parent-dir creation and file open are per-entry-fatal-free (matches the sequential path).
        if let Some(parent) = outpath.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                let _ = io::copy(&mut te, &mut io::sink());
                report.push_failure(entry.name(), e);
                continue;
            }
        }
        let file = match File::create(&outpath) {
            Ok(f) => f,
            Err(e) => {
                let _ = io::copy(&mut te, &mut io::sink());
                report.push_failure(entry.name(), e);
                continue;
            }
        };
        let mut writer = ProgressWriter::new(BufWriter::with_capacity(WRITE_BUF, file), sink);
        match io::copy(&mut te, &mut writer).and_then(|n| {
            writer.flush()?;
            Ok(n)
        }) {
            // tar headers carry an authoritative size. A body that ends short means the archive was
            // truncated, on a growing download that is the common case, and counting it as
            // `extracted` would report a partial tree as a complete one.
            Ok(n) if n != entry.size => {
                drop(writer);
                let _ = fs::remove_file(&outpath);
                report.push_failure(
                    entry.name(),
                    io::Error::other(format!(
                        "truncated: decoded {n} bytes but the tar header declared {}",
                        entry.size
                    )),
                );
            }
            Ok(n) => {
                report.extracted += 1;
                report.bytes += n;
                sink.on_file_done(&entry);
            }
            Err(e) => {
                drop(writer);
                let _ = fs::remove_file(&outpath);
                if sink.is_cancelled() {
                    report.cancelled = true;
                    break;
                }
                report.push_failure(entry.name(), e);
            }
        }
    }
    Ok(report)
}

/// Zip streaming: read consecutive *local file header + data* records straight off the growing prefix
/// with `zip::read::read_zipfile_from_stream`, no central directory required. Per-entry compression
/// (STORE/DEFLATE) and zip64 sizes are handled by the `zip` crate.
///
/// If the very first record can't be parsed for streaming, a data-descriptor zip (sizes in a trailing
/// record, not the local header) or an encrypted zip; nothing is written and we return
/// [`ArchiveError::StreamUnsupported`] so the caller extracts the completed file normally. The
/// data-descriptor / encryption flags are archive-wide in practice, so this is decided on entry #1
/// (before any wasted work), not midway.
fn extract_zip_stream(
    source: Arc<dyn ByteSource>,
    dest: &Path,
    opts: ExtractOptions,
    sink: &dyn ProgressSink,
) -> Result<Report> {
    let mut reader = SourceReader::new(source);
    let mut report = Report::default();
    let mut saw_entry = false;

    loop {
        sink.wait_if_paused();
        if sink.is_cancelled() {
            report.cancelled = true;
            break;
        }
        // Next local record. `Ok(None)` = we reached the central directory (clean end of the file
        // section) → fully streamed. `Err` before any entry = this zip isn't stream-shaped → defer.
        let mut zf = match zip::read::read_zipfile_from_stream(&mut reader) {
            Ok(Some(zf)) => zf,
            Ok(None) => break,
            Err(_) if !saw_entry => return Err(ArchiveError::StreamUnsupported),
            Err(e) => {
                // A structural error after we'd already streamed entries (or a mid-stream abort): the
                // post-download extractor is authoritative, so hand off to it rather than half-trusting
                // a partially-parsed stream.
                let _ = e;
                return Err(ArchiveError::StreamUnsupported);
            }
        };
        saw_entry = true;

        let raw = zf.name().to_string();
        let is_dir = zf.is_dir();
        let Some(safe) = EntryPath::from_raw(&raw) else {
            // zip-slip name → skip (Drop drains the entry so the parser stays aligned), counted so
            // the caller can say the archive carried entries it refused.
            report.dropped_unsafe += 1;
            continue;
        };
        let entry = Entry {
            index: 0,
            path: safe,
            kind: if is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
            size: zf.size(),
            compressed_size: None,
            modified: None,
            unix_mode: None,
            crc32: None,
            encrypted: false,
        };

        let outpath = entry.path.join_under(dest);
        if is_dir {
            let _ = fs::create_dir_all(&outpath);
            continue;
        }

        // skip-already-correct: still read the entry's bytes off the wire (a stream can't seek past
        // it), but skip the write. (zip carries a CRC, so this can fire.)
        if opts.skip_existing && skip::dest_already_correct(&outpath, &entry) {
            let _ = io::copy(&mut zf, &mut io::sink());
            sink.on_bytes(entry.size);
            sink.on_file_done(&entry);
            report.skipped += 1;
            continue;
        }

        sink.on_entry_start(&entry);
        if let Some(parent) = outpath.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                let _ = io::copy(&mut zf, &mut io::sink());
                report.push_failure(entry.name(), e);
                continue;
            }
        }
        let file = match File::create(&outpath) {
            Ok(f) => f,
            Err(e) => {
                let _ = io::copy(&mut zf, &mut io::sink());
                report.push_failure(entry.name(), e);
                continue;
            }
        };
        let mut writer = ProgressWriter::new(BufWriter::with_capacity(WRITE_BUF, file), sink);
        match io::copy(&mut zf, &mut writer).and_then(|n| {
            writer.flush()?;
            Ok(n)
        }) {
            Ok(n) => {
                report.extracted += 1;
                report.bytes += n;
                sink.on_file_done(&entry);
            }
            Err(e) => {
                drop(writer);
                let _ = fs::remove_file(&outpath);
                if sink.is_cancelled() {
                    report.cancelled = true;
                    break;
                }
                report.push_failure(entry.name(), e);
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Codec;
    use crate::progress::NullSink;
    use crate::source::SourceStatus;
    use std::sync::{Condvar, Mutex};
    use std::thread;

    /// A ByteSource whose prefix a test thread reveals incrementally, a stand-in for a download.
    struct GrowingSource {
        data: Vec<u8>,
        state: Mutex<(u64, bool)>, // (revealed, finished)
        cond: Condvar,
    }

    impl GrowingSource {
        fn new(data: Vec<u8>) -> Arc<Self> {
            Arc::new(Self {
                data,
                state: Mutex::new((0, false)),
                cond: Condvar::new(),
            })
        }
        fn reveal(&self, extra: u64) {
            let mut s = self.state.lock().unwrap();
            s.0 = (s.0 + extra).min(self.data.len() as u64);
            self.cond.notify_all();
        }
        fn finish(&self) {
            let mut s = self.state.lock().unwrap();
            s.0 = self.data.len() as u64;
            s.1 = true;
            self.cond.notify_all();
        }
    }

    impl ByteSource for GrowingSource {
        fn total(&self) -> Option<u64> {
            Some(self.data.len() as u64)
        }
        fn available(&self) -> u64 {
            self.state.lock().unwrap().0
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let revealed = self.state.lock().unwrap().0 as usize;
            let start = (offset as usize).min(revealed);
            let n = (revealed - start).min(buf.len());
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            Ok(n)
        }
        fn wait_until(&self, want: u64) -> SourceStatus {
            let mut s = self.state.lock().unwrap();
            loop {
                if s.0 >= want {
                    return SourceStatus::Available(s.0);
                }
                if s.1 {
                    return SourceStatus::Finished(self.data.len() as u64);
                }
                s = self.cond.wait(s).unwrap();
            }
        }
    }

    /// Build an in-memory `.zip` with its sizes in the local headers (STORE + DEFLATE entries),
    /// i.e. a stream-shaped zip, the way 7-Zip/WinRAR write one to a file. Writing to a seekable
    /// `Cursor` is what makes the writer backpatch sizes into the local headers (no data descriptors).
    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        use zip::write::{SimpleFileOptions, ZipWriter};
        use zip::CompressionMethod;
        let mut w = ZipWriter::new(io::Cursor::new(Vec::new()));
        for (i, (name, content)) in files.iter().enumerate() {
            let method = if i % 2 == 0 {
                CompressionMethod::Stored
            } else {
                CompressionMethod::Deflated
            };
            let opts = SimpleFileOptions::default().compression_method(method);
            w.start_file(*name, opts).unwrap();
            w.write_all(content).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn extract_while_download_zip() {
        // A zip with STORE + DEFLATE entries and a nested dir, revealed incrementally like a download.
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("readme.txt", b"hello zip ".repeat(400)),
            ("data/big.bin", (0..50_000u32).map(|i| i as u8).collect()),
            ("data/sub/note.txt", b"nested ".repeat(250)),
        ];
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, c)| (*n, c.as_slice())).collect();
        let zip = make_zip(&refs);

        let source = GrowingSource::new(zip.clone());
        let feeder = {
            let source = source.clone();
            let len = zip.len() as u64;
            thread::spawn(move || {
                let step = (len / 40).max(1);
                let mut revealed = 0;
                while revealed < len {
                    source.reveal(step);
                    revealed += step;
                    thread::yield_now();
                }
                source.finish();
            })
        };

        let dest = std::env::temp_dir().join(format!("cram-stream-zip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        let src: Arc<dyn ByteSource> = source;
        let report = extract_stream(
            src,
            Format::zip(),
            &dest,
            ExtractOptions::default(),
            &NullSink,
        )
        .expect("stream extract zip");
        feeder.join().unwrap();

        assert_eq!(report.extracted, 3, "all three files streamed");
        assert!(report.failed.is_empty(), "failures: {:?}", report.failed);
        for (name, content) in &files {
            let got = fs::read(dest.join(name)).unwrap_or_else(|e| panic!("missing {name}: {e}"));
            assert_eq!(&got, content, "mismatch for {name}");
        }
        let _ = fs::remove_dir_all(&dest);
    }

    /// Build an in-memory `.tar.gz` of a few files.
    fn make_tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        let mut builder = tar::Builder::new(gz);
        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *content).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extract_while_download_tar_gz() {
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("dir/a.txt", b"alpha ".repeat(500)),
            ("dir/b.bin", (0..20_000u32).map(|i| i as u8).collect()),
            ("dir/sub/c.txt", b"charlie ".repeat(300)),
        ];
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, c)| (*n, c.as_slice())).collect();
        let targz = make_tar_gz(&refs);

        let source = GrowingSource::new(targz.clone());
        // Feeder thread: reveal the archive in small steps, exercising the frontier-blocking path.
        let feeder = {
            let source = source.clone();
            let len = targz.len() as u64;
            thread::spawn(move || {
                let step = (len / 40).max(1);
                let mut revealed = 0;
                while revealed < len {
                    source.reveal(step);
                    revealed += step;
                    thread::yield_now();
                }
                source.finish();
            })
        };

        let dest = std::env::temp_dir().join(format!("cram-stream-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        let src: Arc<dyn ByteSource> = source;
        let report = extract_stream(
            src,
            Format::tar(Codec::Gzip),
            &dest,
            ExtractOptions::default(),
            &NullSink,
        )
        .expect("stream extract");
        feeder.join().unwrap();

        assert_eq!(report.extracted, 3);
        assert!(report.failed.is_empty(), "failures: {:?}", report.failed);
        for (name, content) in &files {
            let got = fs::read(dest.join(name)).unwrap_or_else(|e| panic!("missing {name}: {e}"));
            assert_eq!(&got, content, "mismatch for {name}");
        }
        let _ = fs::remove_dir_all(&dest);
    }
}
