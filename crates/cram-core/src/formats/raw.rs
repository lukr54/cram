//! Raw single-stream backend — a bare compressed file (`foo.gz`, `foo.xz`, …) whose decoded stream
//! *is* the one and only entry. No container: we hand the engine one [`EntryStream`] over the
//! decoder, streaming (no buffering). The entry's uncompressed size isn't known up front (the codec
//! doesn't cheaply report it), so `size = 0` and `meta_final = false`.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use crate::codec::decode_stream;
use crate::error::Result;
use crate::format::Format;
use crate::model::{Entry, EntryKind, EntryPath};
use crate::reader::{ArchiveReader, EntryStream};
use crate::secret::PasswordProvider;

/// A `foo.gz` / `foo.xz` opened as a one-entry stream.
pub struct RawReader {
    fmt: Format,
    entries: Vec<Entry>,
    /// The decoded stream, taken on the first `next_entry`.
    body: Option<Box<dyn Read + Send>>,
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
        let file: Box<dyn Read + Send> = Box::new(File::open(path)?);
        let body = decode_stream(fmt.codec, file)?;
        Ok(Self {
            fmt,
            entries: vec![entry],
            body: Some(body),
        })
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
        match self.body.take() {
            Some(body) => Ok(Some(EntryStream {
                entry: self.entries[0].clone(),
                body,
                meta_final: false,
            })),
            None => Ok(None),
        }
    }
}
