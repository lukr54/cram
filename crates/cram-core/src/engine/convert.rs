//! Convert one archive into another format: read the source front-to-back and stream each entry into
//! a destination `ArchiveWriter`. This is the **interop escape hatch** — a `.cram` (or any format we
//! can read) can be re-exported to a portable classic container, so adopting `.cram` is never a
//! one-way door. It reuses the exact reader/writer spine every backend already implements, so every
//! readable source × every writable destination composes for free.

use std::io::{self, Cursor, Read};
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;

use crate::error::{ArchiveError, Result};
use crate::format::Format;
use crate::formats;
use crate::model::Entry;
use crate::progress::{CountingReader, ProgressSink};
use crate::reader::{ArchiveReader, EntryStream};
use crate::secret::PasswordProvider;
use crate::writer::{ArchiveWriter, CreateOptions, CreateReport, WriteHint};

/// A single unknown-size (raw single-stream) source is buffered to learn its real length before it can
/// be written to a size-trusting destination. Above this, we refuse rather than hold gigabytes in RAM.
const MAX_BUFFERED: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Bytes per chunk handed to the writer thread. Large enough that the channel hop is negligible next
/// to the I/O, small enough that a multi-GB entry never materializes in RAM.
const PIPE_CHUNK: usize = 4 * 1024 * 1024;

/// Chunks in flight. Bounded → backpressure, so a fast reader can't outrun a slow writer and blow up
/// memory; `PIPE_DEPTH * PIPE_CHUNK` (~32 MiB) is the ceiling on buffered body bytes.
const PIPE_DEPTH: usize = 8;

/// One item streamed from the reading thread to the writing thread. A file is `FileStart`, then N ×
/// `Chunk`, then `FileEnd`, so a body never materializes whole; a directory is a lone `Dir`.
enum ConvMsg {
    Dir(Entry),
    FileStart(Entry),
    Chunk(Vec<u8>),
    FileEnd,
}

/// The writer side's view of one entry's body: a `Read` that pulls chunks off the channel.
struct ConvBody<'a> {
    rx: &'a Receiver<ConvMsg>,
    cur: Cursor<Vec<u8>>,
    done: bool,
}

impl ConvBody<'_> {
    /// Consume this entry's remaining chunks so the channel is left on a message boundary. Needed
    /// when a writer backend stops reading early — otherwise the next `recv` sees a stray `Chunk`
    /// and the whole stream desyncs.
    fn drain(&mut self) -> Result<()> {
        while !self.done {
            match self.rx.recv() {
                Ok(ConvMsg::Chunk(_)) => {}
                Ok(ConvMsg::FileEnd) => self.done = true,
                Ok(_) => return Err(ArchiveError::Backend("convert stream desync".into())),
                Err(_) => return Err(ArchiveError::Backend("source ended mid-entry".into())),
            }
        }
        Ok(())
    }
}

impl Read for ConvBody<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.cur.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(ConvMsg::Chunk(b)) => self.cur = Cursor::new(b),
                Ok(ConvMsg::FileEnd) => {
                    self.done = true;
                    return Ok(0);
                }
                Ok(_) => {
                    self.done = true;
                    return Err(io::Error::other("convert stream desync"));
                }
                // A channel closed MID-ENTRY means the reader died. This must be an error, never a
                // clean EOF — otherwise the destination silently records a truncated entry as good.
                Err(_) => {
                    self.done = true;
                    return Err(io::Error::other("source ended mid-entry"));
                }
            }
        }
    }
}

/// Reading half: walk the source and stream every entry into the channel. Runs on the CALLING thread
/// because [`ArchiveReader`] is deliberately not `Send` (RAR's archive handle can't move threads).
fn read_side(
    reader: &mut Box<dyn ArchiveReader>,
    tx: &SyncSender<ConvMsg>,
    sink: &dyn ProgressSink,
) -> Result<()> {
    while let Some(es) = reader.next_entry()? {
        sink.wait_if_paused();
        if sink.is_cancelled() {
            break;
        }
        let EntryStream {
            mut entry,
            mut body,
            meta_final,
        } = es;
        sink.on_entry_start(&entry);

        if entry.is_dir() {
            if tx.send(ConvMsg::Dir(entry)).is_err() {
                break;
            }
            continue;
        }

        if !meta_final {
            // Unknown size — a raw single-stream source (.gz/.xz/.bz2/.zst/.lz4/.br) reports
            // `size = 0` and defers the real length (reader.rs contract). A size-trusting
            // destination would truncate (tar's fixed header) or abort past 4 GiB (ZIP64), so
            // buffer to learn the true length, then write exactly that.
            let mut buf = Vec::new();
            CountingReader::new(body.by_ref(), sink)
                .take(MAX_BUFFERED + 1)
                .read_to_end(&mut buf)?;
            if buf.len() as u64 > MAX_BUFFERED {
                return Err(ArchiveError::Backend(format!(
                    "source stream exceeds {} MiB with no declared size — extract it first, then archive",
                    MAX_BUFFERED / (1024 * 1024)
                )));
            }
            entry.size = buf.len() as u64;
            drop(body);
            if tx.send(ConvMsg::FileStart(entry)).is_err()
                || tx.send(ConvMsg::Chunk(buf)).is_err()
                || tx.send(ConvMsg::FileEnd).is_err()
            {
                break;
            }
            continue;
        }

        if tx.send(ConvMsg::FileStart(entry)).is_err() {
            break;
        }
        let mut writer_gone = false;
        loop {
            let mut buf = vec![0u8; PIPE_CHUNK];
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            buf.truncate(n);
            sink.on_bytes(n as u64);
            if tx.send(ConvMsg::Chunk(buf)).is_err() {
                writer_gone = true;
                break;
            }
            if sink.is_cancelled() {
                // Cancelled MID-BODY. Returning here deliberately skips the `FileEnd` below: sending
                // it would tell the writer this entry ended normally, and the backends believe it —
                // tar zero-pads to the declared header size, zip records the short length — so a
                // half-streamed entry would be sealed as complete and the job reported Ok. Bailing
                // without the terminator makes `ConvBody` see a mid-entry disconnect and fail.
                return Err(ArchiveError::Cancelled);
            }
        }
        drop(body); // release the &mut borrow of `reader` before looping
        if writer_gone || tx.send(ConvMsg::FileEnd).is_err() {
            break;
        }
    }
    // A cancel BETWEEN entries leaves a valid prefix, but finishing it would still hand back a
    // success report for an archive silently missing every remaining file.
    if sink.is_cancelled() {
        return Err(ArchiveError::Cancelled);
    }
    Ok(())
}

/// Writing half: drain the channel into the destination. Runs on the spawned thread — legal because
/// [`ArchiveWriter`] is `Send`.
fn write_side(
    mut writer: Box<dyn ArchiveWriter>,
    rx: Receiver<ConvMsg>,
    sink: &dyn ProgressSink,
) -> Result<CreateReport> {
    while let Ok(msg) = rx.recv() {
        match msg {
            ConvMsg::Dir(e) => writer.add_dir(&e)?,
            ConvMsg::FileStart(entry) => {
                // Reuse the source's recorded compressed_size as the store hint: if it barely
                // shrank, it's already-compressed (there is no disk file to probe here).
                let hint = WriteHint {
                    store: entry
                        .compressed_size
                        .is_some_and(|c| c as u128 * 100 >= entry.size as u128 * 95),
                };
                let mut body = ConvBody {
                    rx: &rx,
                    cur: Cursor::new(Vec::new()),
                    done: false,
                };
                // On a write error, bail immediately: returning drops `rx`, which unblocks a reader
                // parked on `send`. Draining first would just delay the failure.
                writer.add_file(&entry, &mut body, hint)?;
                body.drain()?;
                sink.on_file_done(&entry);
            }
            ConvMsg::Chunk(_) | ConvMsg::FileEnd => {
                return Err(ArchiveError::Backend("convert stream desync".into()));
            }
        }
    }
    writer.finish()
}

/// Read `src` (format `src_fmt`, `src_pw` for an encrypted source) and re-create it at `dst` in
/// `dst_fmt` under `opts`. Directories and files are forwarded in source order; the source's own
/// per-entry compression signal seeds the store-vs-compress hint (there is no disk file to probe).
///
/// **Pipelined:** reading/decoding and writing/compressing run on two threads joined by a bounded
/// channel. Done serially they alternate — the drive idles while we decode and the CPU idles while we
/// write — so on a same-disk convert the two stages overlap instead of summing.
pub fn convert(
    src: &Path,
    src_fmt: Format,
    dst: &Path,
    dst_fmt: Format,
    opts: &CreateOptions,
    src_pw: Arc<dyn PasswordProvider>,
    sink: &dyn ProgressSink,
) -> Result<CreateReport> {
    let mut reader = formats::open(src, src_fmt, src_pw)?;
    // Stage + rename (same scheme as engine::create): converting straight onto `dst` would truncate
    // a pre-existing archive there before the first entry has even been read, so `dst` is left alone
    // until the rename at the end.
    let staging = super::staging_path(dst);
    let writer = formats::create(&staging, dst_fmt, opts)?;

    std::thread::scope(|s| {
        let (tx, rx) = sync_channel::<ConvMsg>(PIPE_DEPTH);
        let wh = s.spawn(move || write_side(writer, rx, sink));
        let read_res = read_side(&mut reader, &tx, sink);
        // Close the stream so the writer's `recv` loop ends and it can `finish()`.
        drop(tx);
        let write_res = wh
            .join()
            .map_err(|_| ArchiveError::Backend("convert writer thread panicked".into()))?;
        // Reader errors first: if the source failed, the writer's "source ended mid-entry" is only
        // the symptom. When the WRITER is at fault the reader's send fails and it returns Ok, so
        // this order surfaces the root cause either way.
        if let Err(e) = read_res {
            // The staging file is a partial archive that would otherwise sit on disk looking valid.
            drop(write_res);
            let _ = std::fs::remove_file(&staging);
            return Err(e);
        }
        match write_res {
            Ok(report) => {
                if let Err(e) = std::fs::rename(&staging, dst) {
                    let _ = std::fs::remove_file(&staging);
                    return Err(e.into());
                }
                Ok(report)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&staging);
                Err(e)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Codec;
    use crate::progress::NullSink;
    use crate::secret::NoPassword;
    use crate::sniff;
    use std::io::Write;

    fn all_files(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut v = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    v.extend(all_files(&p));
                } else {
                    v.push(p);
                }
            }
        }
        v
    }

    /// Guards against: losing the body when a raw single-stream source (a bare `.gz`, which reports
    /// size 0 / meta_final=false) is converted to a size-trusting destination (tar, zip). Those
    /// destinations write what the declared size says, so an entry whose real length is never
    /// learned lands as a silent 0-byte file. The full content must survive the round trip.
    #[test]
    fn convert_raw_gzip_source_preserves_content_to_tar_and_zip() {
        let dir = std::env::temp_dir().join(format!("cram-conv-raw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let content = b"raw single-stream content that must survive the convert ".repeat(500);
        let gz = dir.join("payload.gz");
        {
            let f = std::fs::File::create(&gz).unwrap();
            let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            enc.write_all(&content).unwrap();
            enc.finish().unwrap();
        }
        let src_fmt = sniff::sniff_path(&gz).unwrap();

        for (name, dst_fmt) in [
            ("out.tar", Format::tar(Codec::None)),
            ("out.zip", Format::zip()),
        ] {
            let dst = dir.join(name);
            convert(
                &gz,
                src_fmt,
                &dst,
                dst_fmt,
                &CreateOptions::default(),
                Arc::new(NoPassword),
                &NullSink,
            )
            .unwrap_or_else(|e| panic!("convert to {name}: {e}"));

            let out = dir.join(format!("x_{}", name.replace('.', "_")));
            crate::engine::extract(
                &dst,
                &out,
                Arc::new(NoPassword),
                Default::default(),
                &NullSink,
            )
            .unwrap_or_else(|e| panic!("extract {name}: {e}"));

            let files = all_files(&out);
            assert_eq!(files.len(), 1, "{name}: exactly one extracted file");
            let got = std::fs::read(&files[0]).unwrap();
            assert_eq!(
                got, content,
                "{name}: raw source content preserved, not truncated"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Guards against: a mid-body cancel sealing a truncated entry as complete. Breaking out of the
    /// chunk loop and still sending `FileEnd` tells the writer the entry ended normally, and the
    /// backends believe it — tar zero-pads to the declared size, zip records the short length — so
    /// `convert` would return Ok for a partial archive. A cancelled convert must be an error, not a
    /// silent partial.
    #[test]
    fn cancel_mid_body_does_not_report_success() {
        use crate::progress::Progress;
        use std::sync::atomic::{AtomicU64, Ordering};

        /// Cancels once enough bytes have flowed to be sure we are INSIDE an entry body.
        struct CancelMidBody {
            inner: Progress,
            seen: AtomicU64,
        }
        impl ProgressSink for CancelMidBody {
            fn on_bytes(&self, n: u64) {
                self.inner.on_bytes(n);
                // PIPE_CHUNK is 4 MiB; the entry below is far larger, so this lands mid-body.
                if self.seen.fetch_add(n, Ordering::Relaxed) + n >= PIPE_CHUNK as u64 {
                    self.inner.request_cancel();
                }
            }
            fn on_file_done(&self, e: &Entry) {
                self.inner.on_file_done(e)
            }
            fn is_cancelled(&self) -> bool {
                self.inner.is_cancelled()
            }
        }

        let dir = std::env::temp_dir().join(format!("cram-conv-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // One entry comfortably larger than PIPE_CHUNK so cancellation lands inside its body.
        let big = vec![b'x'; 16 * 1024 * 1024];
        let src = dir.join("src.zip");
        {
            use zip::write::{SimpleFileOptions, ZipWriter};
            let f = std::fs::File::create(&src).unwrap();
            let mut zw = ZipWriter::new(f);
            zw.start_file(
                "big.bin",
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zw.write_all(&big).unwrap();
            zw.finish().unwrap();
        }
        let src_fmt = sniff::sniff_path(&src).unwrap();

        for (name, dst_fmt) in [
            ("out.tar", Format::tar(Codec::None)),
            ("out.zip", Format::zip()),
        ] {
            let dst = dir.join(name);
            let sink = CancelMidBody {
                inner: Progress::new(big.len() as u64, 1),
                seen: AtomicU64::new(0),
            };
            let r = convert(
                &src,
                src_fmt,
                &dst,
                dst_fmt,
                &CreateOptions::default(),
                Arc::new(NoPassword),
                &sink,
            );
            assert!(
                matches!(r, Err(ArchiveError::Cancelled)),
                "{name}: a mid-body cancel must surface as Cancelled, got {r:?}"
            );
            assert!(
                !dst.exists(),
                "{name}: the partial archive must not be left behind looking valid"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Guards against: convert reporting no streamed bytes to its sink. The entry-level callbacks
    /// (`on_entry_start`/`on_file_done`) carry no byte counts on their own, so without `on_bytes` a
    /// GUI progress bar sits at 0% for the entire job while the destination file visibly grows on
    /// disk.
    #[test]
    fn convert_reports_byte_progress_to_its_sink() {
        use crate::progress::Progress;

        let dir = std::env::temp_dir().join(format!("cram-conv-prog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A source with several entries of known total size.
        let bodies: Vec<Vec<u8>> = (0..4)
            .map(|i| format!("entry {i} body ").repeat(2000).into_bytes())
            .collect();
        let expected: u64 = bodies.iter().map(|b| b.len() as u64).sum();

        let src = dir.join("src.zip");
        {
            use zip::write::{SimpleFileOptions, ZipWriter};
            let f = std::fs::File::create(&src).unwrap();
            let mut zw = ZipWriter::new(f);
            for (i, b) in bodies.iter().enumerate() {
                zw.start_file(format!("f{i}.txt"), SimpleFileOptions::default())
                    .unwrap();
                zw.write_all(b).unwrap();
            }
            zw.finish().unwrap();
        }
        let src_fmt = sniff::sniff_path(&src).unwrap();

        // Both writer shapes: tar declares each size up front in a fixed header, zip records it as
        // it goes, and the byte accounting has to hold for either.
        for (name, dst_fmt) in [
            ("out.tar", Format::tar(Codec::None)),
            ("out.zip", Format::zip()),
        ] {
            let prog = Progress::new(expected, bodies.len() as u64);
            convert(
                &src,
                src_fmt,
                &dir.join(name),
                dst_fmt,
                &CreateOptions::default(),
                Arc::new(NoPassword),
                &prog,
            )
            .unwrap_or_else(|e| panic!("convert to {name}: {e}"));

            assert_eq!(
                prog.done_bytes(),
                expected,
                "{name}: every streamed byte must reach the sink, or the UI shows 0% throughout"
            );
            assert_eq!(prog.done_files(), bodies.len() as u64, "{name}: file count");
            assert!(
                (prog.fraction() - 1.0).abs() < f32::EPSILON,
                "{name}: bar should read 100% at completion, got {}",
                prog.fraction()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
