//! Regression: a crafted 7z must not be able to make a read never return.
//!
//! `tests/data/hostile-7z-read-after-error.7z` is 2,208 bytes and was produced by the smoke-fuzz
//! harness on 2026-08-13 (a valid archive with 47 bytes appended). Its header is intact, so it
//! lists correctly, and its LZMA2 stream is not: the first read of entry 0 reports corrupt input
//! and **the second read of the same stream never returns**. Before the fix that hung `cram t` and
//! `cram x` forever at 100% of a core, having allocated nothing.
//!
//! Every assertion here is about termination and about the failure being *reported*. The exact
//! error text is upstream's and is not pinned.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use cram_core::format::Format;
use cram_core::secret::NoPassword;

/// Generous: the whole archive is 2 KB, so anything correct finishes in milliseconds. This is a
/// liveness bound, not a performance one.
const BUDGET: Duration = Duration::from_secs(60);

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hostile-7z-read-after-error.7z")
}

/// Run `f` on its own thread and fail if it does not finish. A hung thread cannot be joined, so a
/// timeout has to be reported from here rather than waited on; the harness exits and takes the
/// thread with it.
fn within_budget<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(BUDGET) {
        Ok(v) => v,
        Err(_) => {
            panic!("{what} did not return within {BUDGET:?} -- the read-after-error hang is back")
        }
    }
}

#[test]
fn a_crafted_7z_cannot_hang_the_random_access_path() {
    let outcome = within_budget("the random-access path", || {
        let reader =
            cram_core::formats::open(&fixture(), Format::sevenz(), Arc::new(NoPassword)).unwrap();

        // The header survives the mutation, so listing must still work. If this ever stops being
        // true the fixture has stopped testing what it was built to test.
        let listed = reader.entries().map(<[_]>::len).ok();

        let ra = reader.as_random_access().expect("7z offers random access");
        let n = ra.entries().len();

        // The shape the fuzz harness drives, and the shape `cram t` drives: hand every entry to a
        // visitor that reads the body and keeps going after a failure.
        let all: Vec<usize> = (0..n).collect();
        let mut served = 0usize;
        let unit = ra.copy_unit(&all, &mut |_i, body| {
            let _ = std::io::copy(&mut body.take(1 << 20), &mut std::io::sink());
            served += 1;
            true
        });

        // And the per-entry calls, which reach the same decoder by another route.
        let per_entry: Vec<_> = (0..n)
            .map(|i| {
                let mut sink = std::io::sink();
                (
                    ra.copy_entry(i, &mut sink).is_err(),
                    ra.read_range(i, 0, 4096).is_err(),
                )
            })
            .collect();

        (listed, unit.is_err(), served, per_entry)
    });

    let (listed, unit_failed, served, per_entry) = outcome;
    assert_eq!(
        listed,
        Some(3),
        "the fixture should still list three entries"
    );

    // The block faulted, so the unit fails. That is what makes the engine report every entry the
    // unit never reached instead of dropping them from a successful-looking extraction.
    assert!(unit_failed, "a faulted solid block must fail its unit");
    assert!(
        served < 3,
        "the visitor cannot have been handed every entry of a faulted block"
    );

    // The two content entries are unreadable; the directory entry has no bytes and is fine.
    assert!(
        per_entry[0].0 && per_entry[0].1,
        "entry 0 must report an error"
    );
    assert!(
        per_entry[1].0 && per_entry[1].1,
        "entry 1 must report an error"
    );
}

/// The same file through the front door: extraction reports failure rather than hanging or claiming
/// success. Nothing is asserted about what lands on disk beyond it not being a silent pass.
#[test]
fn extracting_a_crafted_7z_fails_rather_than_hanging() {
    let dir = std::env::temp_dir().join(format!("cram-hostile-7z-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let out = dir.clone();
    let res = within_budget("extraction", move || {
        cram_core::engine::extract(
            &fixture(),
            &out,
            Arc::new(NoPassword),
            Default::default(),
            &cram_core::progress::NullSink,
        )
        .map(|r| r.failed.len())
    });

    let _ = std::fs::remove_dir_all(&dir);
    // A typed error is equally acceptable: it did not hang, and it did not claim success.
    if let Ok(failed) = res {
        assert!(
            failed > 0,
            "a corrupt solid block must be reported, not silently skipped"
        );
    }
}
