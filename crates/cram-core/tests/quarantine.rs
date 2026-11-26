//! macOS: an archive's download quarantine reaches everything extracted from it, in every format and
//! on both engine paths, and an archive without one leaves nothing behind. See `engine::quarantine`.
#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;
use cram_core::ExtractOptions;

const Q: &str = "0083;6745f2a0;Safari;3F2504E0-4F89-11D3-9A0C-0305E82C3301";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-quar-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The archive's value at `p`, or `None`. Through `xattr(1)` so the test does not share the code
/// it is checking.
fn mark_of(p: &Path) -> Option<String> {
    let o = Command::new("xattr")
        .args(["-p", "com.apple.quarantine"])
        .arg(p)
        .output()
        .unwrap();
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

fn set_mark(p: &Path) {
    let s = Command::new("xattr")
        .args(["-w", "com.apple.quarantine", Q])
        .arg(p)
        .status()
        .unwrap();
    assert!(s.success());
    assert_eq!(
        mark_of(p).as_deref(),
        Some(Q),
        "the fixture must carry the mark"
    );
}

/// A small file (batched on the tar path), a 5 MiB one (over tar's 4 MiB batch limit, so streamed),
/// and a directory two levels down with a script in it.
fn make_archive(dir: &Path, fmt: Format, name: &str) -> PathBuf {
    let data = dir.join("data");
    fs::create_dir_all(data.join("app.app/Contents/MacOS")).unwrap();
    fs::write(data.join("a.txt"), b"alpha ".repeat(100)).unwrap();
    fs::write(
        data.join("big.bin"),
        (0..5u32 << 20)
            .map(|i| (i * 7 + i / 251) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(
        data.join("app.app/Contents/MacOS/run"),
        b"#!/bin/sh\necho x\n",
    )
    .unwrap();
    let archive = dir.join(name);
    engine::create::create(&archive, fmt, &[data], CreateOptions::default(), &NullSink)
        .expect("create");
    archive
}

/// Every path under `root`, `root` itself excluded.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out.sort();
    out
}

fn formats() -> [(Format, &'static str); 4] {
    [
        (Format::zip(), "q.zip"),
        (Format::sevenz(), "q.7z"),
        (Format::cram(Codec::None), "q.cram"),
        (Format::tar(Codec::None), "q.tar"),
    ]
}

#[test]
fn a_marked_archive_marks_every_file_and_directory() {
    for (fmt, name) in formats() {
        let dir = scratch(&format!("all-{name}"));
        let archive = make_archive(&dir, fmt, name);
        set_mark(&archive);
        let out = dir.join("out");
        let r = engine::extract(
            &archive,
            &out,
            Arc::new(NoPassword),
            ExtractOptions::default(),
            &NullSink,
        )
        .unwrap();
        assert_eq!(r.extracted, 3, "{name}: three files");
        let paths = walk(&out);
        // data, data/app.app, .../Contents, .../MacOS, and three files.
        assert_eq!(paths.len(), 7, "{name}: {paths:?}");
        for p in &paths {
            assert_eq!(mark_of(p).as_deref(), Some(Q), "{name}: {}", p.display());
        }
    }
}

#[test]
fn an_unmarked_archive_marks_nothing() {
    for (fmt, name) in formats() {
        let dir = scratch(&format!("none-{name}"));
        let archive = make_archive(&dir, fmt, name);
        assert_eq!(mark_of(&archive), None);
        let out = dir.join("out");
        engine::extract(
            &archive,
            &out,
            Arc::new(NoPassword),
            ExtractOptions::default(),
            &NullSink,
        )
        .unwrap();
        let paths = walk(&out);
        assert_eq!(paths.len(), 7, "{name}: {paths:?}");
        for p in &paths {
            assert_eq!(mark_of(p), None, "{name}: {}", p.display());
        }
    }
}

/// A file the user already had gets the archive's content and so its mark; the directory they
/// already had is theirs and is left as it was.
#[test]
fn an_overwritten_file_is_marked_and_a_pre_existing_directory_is_not() {
    for (fmt, name) in formats() {
        let dir = scratch(&format!("over-{name}"));
        let archive = make_archive(&dir, fmt, name);
        set_mark(&archive);
        let out = dir.join("out");
        fs::create_dir_all(out.join("data")).unwrap();
        fs::write(out.join("data/a.txt"), b"the user's").unwrap();
        engine::extract(
            &archive,
            &out,
            Arc::new(NoPassword),
            ExtractOptions::default(),
            &NullSink,
        )
        .unwrap();
        assert_eq!(
            fs::read(out.join("data/a.txt")).unwrap(),
            b"alpha ".repeat(100)
        );
        assert_eq!(
            mark_of(&out.join("data/a.txt")).as_deref(),
            Some(Q),
            "{name}"
        );
        assert_eq!(mark_of(&out.join("data")), None, "{name}: theirs");
        assert_eq!(
            mark_of(&out.join("data/app.app")).as_deref(),
            Some(Q),
            "{name}"
        );
    }
}
