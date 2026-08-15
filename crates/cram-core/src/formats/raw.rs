//! Raw single-stream backend, a bare compressed file (`foo.gz`, `foo.xz`, …) whose decoded stream
//! *is* the one and only entry. No container: we hand the engine one [`EntryStream`] over the
//! decoder, streaming (no buffering). The entry's uncompressed size isn't known up front (the codec
//! doesn't cheaply report it), so `size = 0` and `meta_final = false`.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::codec::{decode_stream, multi};
use crate::error::Result;
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream};
use crate::secret::PasswordProvider;

/// A `foo.gz` / `foo.xz` opened as a one-entry stream.
pub struct RawReader {
    path: PathBuf,
    fmt: Format,
    entries: Vec<Entry>,
    /// Whether the one entry has been handed out yet. The decoder is built on demand rather than at
    /// open: a multi-stream file is scanned for its seams so it can decode on a pool
    /// ([`multi`]), and that costs a read of the file — which `cram l` should not pay to list a
    /// single entry it already knows the name of.
    taken: bool,
}

impl RawReader {
    pub fn open(path: &Path, fmt: Format, _pw: Arc<dyn PasswordProvider>) -> Result<Self> {
        // Entry name = the file name minus the codec extension (`foo.gz` → `foo`).
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("data");
        let safe = EntryPath::from_raw(name)
            .unwrap_or_else(|| EntryPath::from_raw("data").expect("literal is safe"));
        let entry = Entry {
            index: 0,
            path: safe,
            kind: EntryKind::File,
            size: 0, // uncompressed size not known from the codec header
            compressed_size: None,
            modified: None,
            unix_mode: None,
            crc32: None,
            encrypted: false,
        };
        // Opened and dropped, so an unreadable file is still reported here rather than at the first
        // read, which is where callers expect it.
        File::open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            fmt,
            entries: vec![entry],
            taken: false,
        })
    }

    /// The decoded stream, on a pool if the file is a run of independent streams — `pbzip2` output,
    /// a Wikipedia multistream dump, `cat a.xz b.xz`, or anything we wrote ourselves.
    fn body(&self) -> Result<Box<dyn Read + Send>> {
        if let Some(plan) = multi::plan(&self.path, self.fmt.codec) {
            return Ok(multi::open(&plan));
        }
        let file: Box<dyn Read + Send> = Box::new(File::open(&self.path)?);
        decode_stream(self.fmt.codec, file)
    }
}

impl ArchiveReader for RawReader {
    fn format(&self) -> Format {
        self.fmt
    }

    fn entries(&self) -> Result<&[Entry]> {
        Ok(&self.entries)
    }

    fn next_entry(&mut self) -> Result<Option<EntryStream<'_>>> {
        if self.taken {
            return Ok(None);
        }
        self.taken = true;
        Ok(Some(EntryStream {
            entry: self.entries[0].clone(),
            body: self.body()?,
            meta_final: false,
        }))
    }
}
