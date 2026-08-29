//! Reading a ZIP whose entries use Zstandard, method 93.
//!
//! Repackers have largely moved to zstd-in-zip because it is far faster than DEFLATE at a similar
//! ratio, and until 1.3.1 Cram could not read one: every zstd entry failed with "compression method
//! not supported: 93" while the rest of the archive extracted around it, so a real archive came out
//! missing most of its files with no single obvious error. `zip/zstd` rides on the `zstd-c` feature
//! because it needs the same crate at the same version, which the shipping build already links.
//!
//! The fixture is written here rather than committed, so this also covers the write side of the same
//! feature and there is no binary blob in the tree.

#![cfg(feature = "zstd-c")]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use cram_core::format::Format;
use cram_core::secret::NoPassword;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-zip-zstd-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Compressible enough that the method actually engages instead of falling back to stored.
fn body() -> String {
    (0..4000)
        .map(|i| format!("line {i} of a very repetitive test file\n"))
        .collect()
}

const ZSTD_ENTRIES: [&str; 3] = ["alpha.txt", "nested/beta.txt", "nested/deep/gamma.txt"];

/// A ZIP with three zstd entries and one stored entry, because a real archive mixes methods and the
/// bug only ever hit the zstd ones.
fn write_fixture(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("zstd.zip");
    let mut z = ZipWriter::new(fs::File::create(&path).unwrap());
    let zstd = SimpleFileOptions::default().compression_method(CompressionMethod::Zstd);
    let text = body();
    for name in ZSTD_ENTRIES {
        z.start_file(name, zstd).unwrap();
        z.write_all(text.as_bytes()).unwrap();
    }
    z.start_file(
        "plain.txt",
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
    )
    .unwrap();
    z.write_all(b"stored entry\n").unwrap();
    z.finish().unwrap();
    path
}

#[test]
fn a_zip_using_method_93_reads_back_byte_identical() {
    let dir = scratch("read");
    let archive = write_fixture(&dir);

    let reader = cram_core::formats::open(&archive, Format::zip(), Arc::new(NoPassword)).unwrap();
    let ra = reader.as_random_access().expect("zip offers random access");

    let want = body();
    let mut seen = 0;
    for (i, e) in ra.entries().iter().enumerate() {
        let name = e.path.raw().to_string();
        let mut got = Vec::new();
        ra.copy_entry(i, &mut got)
            .unwrap_or_else(|err| panic!("{name} failed to decode: {err}"));
        if ZSTD_ENTRIES.contains(&name.as_str()) {
            assert_eq!(
                got,
                want.as_bytes(),
                "{name} decoded to the wrong bytes rather than failing outright"
            );
            seen += 1;
        }
    }
    assert_eq!(
        seen,
        ZSTD_ENTRIES.len(),
        "not every zstd entry was present in the archive"
    );

    let _ = fs::remove_dir_all(&dir);
}
