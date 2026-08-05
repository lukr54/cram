//! **Everything the create table offers must actually be creatable.**
//!
//! [`formats::CREATE_TARGETS`] is the one list of what Cram can write, and it is read by the CLI's
//! extension match, by its error text, and by the Studio GUI's format picker. Before it existed the
//! match lived in two places that had drifted four formats apart, so the GUI silently offered less
//! than the engine could do and nothing anywhere failed.
//!
//! A single table fixes the drift and introduces a new way to be wrong: an entry can name an
//! extension for something that cannot actually be written, and now the GUI would offer it. So each
//! offered target is built here, extracted, and compared against the source.
//!
//! The other thing worth pinning is that matching is longest-first. `out.tar.gz` has to be a gzipped
//! tar and not a bare tar that happens to end in something; ordering the table differently must not
//! be able to change that.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cram_core::format::{Codec, Container};
use cram_core::formats::{self, CREATE_TARGETS};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cram-targets-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(
        src.join("a.txt"),
        "the same line over and over\n".repeat(200),
    )
    .unwrap();
    fs::write(
        src.join("sub/b.bin"),
        (0u8..=255).cycle().take(9_000).collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(src.join("empty.txt"), b"").unwrap();
    src
}

#[test]
fn every_offered_target_round_trips() {
    let dir = scratch("rt");
    let src = tree(&dir);
    let offered: Vec<_> = CREATE_TARGETS.iter().filter(|t| t.offer).collect();
    assert!(
        offered.len() >= 10,
        "expected the full set of writable targets, got {}",
        offered.len()
    );

    for target in offered {
        let arc = dir.join(format!("out{}", target.ext));
        let fmt = formats::format_for_new(&arc).unwrap_or_else(|e| {
            panic!(
                "{}: the table offers it but cannot match it: {e}",
                target.ext
            )
        });
        assert_eq!(
            (fmt.container, fmt.codec),
            (target.container, target.codec),
            "{}: matched the wrong format",
            target.ext
        );

        cram_core::engine::create::create(
            &arc,
            fmt,
            std::slice::from_ref(&src),
            CreateOptions::default(),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("{} is offered but failed to create: {e}", target.ext));

        let out = dir.join(format!("x{}", target.ext.replace('.', "_")));
        fs::create_dir_all(&out).unwrap();
        cram_core::engine::extract(
            &arc,
            &out,
            Arc::new(NoPassword),
            Default::default(),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("{} created but would not extract: {e}", target.ext));

        for (name, want) in [
            ("a.txt", fs::read(src.join("a.txt")).unwrap()),
            ("sub/b.bin", fs::read(src.join("sub/b.bin")).unwrap()),
        ] {
            let got = out.join("src").join(name);
            assert!(
                got.is_file(),
                "{}: {name} missing after extract",
                target.ext
            );
            assert_eq!(
                fs::read(&got).unwrap(),
                want,
                "{}: {name} came back different",
                target.ext
            );
        }
        let _ = fs::remove_file(&arc);
        let _ = fs::remove_dir_all(&out);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn longest_extension_wins() {
    // The trap: `.tar` also matches the end of `.tar.gz` if you take the first hit rather than the
    // longest, and then every compressed tar is silently written uncompressed.
    for (name, container, codec) in [
        ("out.tar", Container::Tar, Codec::None),
        ("out.tar.gz", Container::Tar, Codec::Gzip),
        ("out.tar.xz", Container::Tar, Codec::Xz),
        ("out.tar.zst", Container::Tar, Codec::Zstd),
        ("out.tar.bz2", Container::Tar, Codec::Bzip2),
        ("out.tar.lz4", Container::Tar, Codec::Lz4),
        ("out.tar.br", Container::Tar, Codec::Brotli),
        ("out.tgz", Container::Tar, Codec::Gzip),
        ("out.zip", Container::Zip, Codec::None),
        ("out.7z", Container::SevenZ, Codec::None),
        ("out.cram", Container::Cram, Codec::None),
        // Case is not a signal: someone typing OUT.TAR.GZ means the same thing.
        ("OUT.TAR.GZ", Container::Tar, Codec::Gzip),
    ] {
        let fmt =
            formats::format_for_new(Path::new(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((fmt.container, fmt.codec), (container, codec), "{name}");
    }

    // Anything unrecognised has to be refused rather than guessed at, and the message has to say
    // what is on offer, because it is the only place a CLI user finds out.
    let err = formats::format_for_new(Path::new("out.rar")).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(".cram") && msg.contains(".tar.zst"),
        "unhelpful: {msg}"
    );
}
