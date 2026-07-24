//! Regression: extraction must restore each entry's recorded modification time, not stamp
//! the extracted files with "now". Covered for the formats that actually carry a per-entry mtime,
//! ZIP (parallel/random-access path) and tar (sequential path), so both engine paths are exercised.
//!
//! `.cram` v1 deliberately stores no per-entry mtime (the format is frozen), so its reader reports
//! `modified: None` and there is nothing to restore, that's correct, not a regression, and is why
//! `.cram` is not asserted here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

fn only_file(dir: &Path) -> PathBuf {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, &mut v);
    assert_eq!(v.len(), 1, "expected exactly one extracted file in {dir:?}");
    v.into_iter().next().unwrap()
}

fn secs_since_epoch(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[test]
fn extract_restores_entry_mtime_for_zip_and_tar() {
    // A fixed, distinctive timestamp well in the past on an even second (ZIP's DOS time has a 2-second
    // granularity). If restoration were a no-op, the extracted file's mtime would be ~now instead.
    const FIXED_UNIX: i64 = 1_577_934_246; // 2020-01-02T03:04:06Z
    let want = FIXED_UNIX;

    let root = std::env::temp_dir().join(format!("cram-mtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(src.join("d")).unwrap();
    let src_file = src.join("d/keep.txt");
    std::fs::write(
        &src_file,
        b"content whose timestamp must survive the round-trip",
    )
    .unwrap();
    // Stamp the source so create() records this mtime into the entry.
    filetime::set_file_mtime(&src_file, filetime::FileTime::from_unix_time(FIXED_UNIX, 0)).unwrap();

    for (name, fmt) in [
        ("out.zip", Format::zip()),
        ("out.tar", Format::tar(Codec::None)),
    ] {
        let arc = root.join(name);
        engine::create::create(
            &arc,
            fmt,
            &[src.join("d")],
            CreateOptions::default(),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("create {name}: {e}"));

        let out = root.join(format!("x_{}", name.replace('.', "_")));
        engine::extract(
            &arc,
            &out,
            Arc::new(NoPassword),
            Default::default(),
            &NullSink,
        )
        .unwrap_or_else(|e| panic!("extract {name}: {e}"));

        let got = std::fs::metadata(only_file(&out))
            .and_then(|m| m.modified())
            .unwrap_or_else(|e| panic!("stat extracted {name}: {e}"));
        let got_s = secs_since_epoch(got);
        let now_s = secs_since_epoch(SystemTime::now());

        assert!(
            (got_s - want).abs() <= 2,
            "{name}: extracted mtime {got_s} should match archived {want} (±2s), not now ({now_s})"
        );
        // Extra guard: prove it isn't just "now" (which a no-op restore would leave).
        assert!(
            (now_s - got_s) > 60,
            "{name}: extracted mtime {got_s} looks like now ({now_s}), restore did not take effect"
        );
    }

    // Sanity: the fixed timestamp really is far from now, so the ±2s assert above is meaningful.
    assert!(secs_since_epoch(SystemTime::now()) - want > 60);
    let _ = std::fs::remove_dir_all(&root);
}
