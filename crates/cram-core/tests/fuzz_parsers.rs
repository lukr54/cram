//! Smoke-fuzz every archive parser: feed it random and mutated-from-valid bytes and assert it never
//! **panics**, a corrupt/hostile archive must always come back as a typed `Err`, never an out-of-bounds
//! index, integer-overflow panic, or `unwrap()` on `None`. This is the stable-Rust, mingw-friendly gate
//! that runs in normal `cargo test`; for coverage-guided fuzzing point `cargo fuzz` (nightly/LLVM, a CI
//! job) at the same `formats::open` entry point.
//!
//! Iterations scale with `CRAM_FUZZ_ITERS` (default 150 per parser); a nightly run can set it high.
//!
//! The pure-Rust parsers (ZIP, 7z, tar, ISO, `.cram`) are covered here. RAR is excluded:
//! it decodes through the UnRAR C++ library, where a bad input could fault the *process* rather than
//! raise a catchable Rust panic, not something a unit test can contain.
//!
//! Two of these parsers (7z, tar) decode the body on a **spawned worker thread**, so a panic there
//! would *not* unwind into `catch_unwind` on the test thread. To catch those too, a process-wide panic
//! **hook** bumps a counter on *every* panic on *any* thread; `feed` flags a failure if either the
//! caught result is an error (test-thread panic) or the counter advanced (worker-thread panic).
//!
//! **Not returning is a failure too**, and needs its own mechanism: every input runs under
//! [`PER_INPUT_LIMIT`], because a parser that spins forever produces no panic, no error and no
//! output, and is indistinguishable from slow work until someone reads the staging file's mtime.

use std::io::Read;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use cram_core::engine;
use cram_core::format::{Codec, Format};
use cram_core::progress::NullSink;
use cram_core::secret::NoPassword;
use cram_core::writer::CreateOptions;

/// Bumped by the panic hook on every panic on any thread (test or decode worker).
static PANIC_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Tiny deterministic PRNG (xorshift64*), reproducible so a failure prints a re-runnable seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
}

fn iters() -> usize {
    std::env::var("CRAM_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150)
}

/// Drive `formats::open` + `entries` + a bounded body drain, the whole read path a caller would take.
/// Any `Err` is fine; the point is that it must not panic. Bodies are capped so a crafted huge size
/// can't turn the fuzzer into a memory hog.
fn exercise(fmt: Format, path: &Path) {
    let Ok(mut reader) = cram_core::formats::open(path, fmt, Arc::new(NoPassword)) else {
        return;
    };
    let _ = reader.entries().map(<[_]>::len);

    // The random-access side reads structure the sequential side never looks at -- for 7z, the LZMA2
    // chunk framing inside a block, walked to decide where a decoder may start. That walk is driven
    // entirely by attacker-controlled bytes, so it belongs in here rather than being covered by
    // `next_entry` alone, which does not reach it at all.
    if let Some(ra) = reader.as_random_access() {
        let n = ra.entries().len().min(4);
        let all: Vec<usize> = (0..n).collect();
        let mut served = 0usize;
        let _ = ra.copy_unit(&all, &mut |_i, body| {
            let mut sink = std::io::sink();
            let _ = std::io::copy(&mut body.take(1 << 20), &mut sink);
            served += 1;
            served < 8
        });
        for i in 0..n {
            let mut sink = std::io::sink();
            let _ = ra.copy_entry(i, &mut sink);
            let _ = ra.read_range(i, 0, 4096);
        }
    }

    for _ in 0..8 {
        match reader.next_entry() {
            Ok(Some(mut es)) => {
                let mut sink = std::io::sink();
                let mut body = (&mut es.body).take(1 << 20);
                let _ = std::io::copy(&mut body, &mut sink);
            }
            _ => break,
        }
    }
}

/// How long one input gets. Every input here is at most 40 KB, so a correct parser needs
/// milliseconds; this is a liveness bound, not a speed one.
///
/// It exists because **a hang and a slow run are indistinguishable from outside**. On 2026-08-13 a
/// crafted 2 KB 7z spun for 7,470 CPU-seconds inside a single `read` call and the sweep looked
/// merely slow for two hours, having in fact stopped on its 21,000th input of 40,000.
const PER_INPUT_LIMIT: Duration = Duration::from_secs(60);

/// Feed `bytes` to the `fmt` parser (staged at the reused `path`). Returns `Some(message)` if it
/// panicked (on any thread) or never came back. Returning the message instead of asserting lets the
/// caller restore the real panic hook *before* failing, so the re-runnable seed is actually printed
/// rather than swallowed by the quiet hook.
///
/// The parse runs on its own thread so that not returning is a reportable outcome. A hung thread
/// cannot be reclaimed, so the harness reports and the process exits, taking the thread with it.
fn feed(fmt: Format, bytes: &[u8], seed: u64, path: &Path) -> Option<String> {
    if std::fs::write(path, bytes).is_err() {
        return None;
    }
    let len = bytes.len();
    let before = PANIC_COUNT.load(Ordering::SeqCst);

    let staged = path.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = panic::catch_unwind(AssertUnwindSafe(|| exercise(fmt, &staged)));
        let _ = tx.send(res.is_err());
    });

    let panicked = match rx.recv_timeout(PER_INPUT_LIMIT) {
        Ok(v) => v,
        Err(_) => {
            return Some(format!(
                "parser {:?} HUNG on fuzz input: nothing returned within {PER_INPUT_LIMIT:?} \
                 (seed={seed}, len={len})",
                fmt.container
            ))
        }
    };

    let after = PANIC_COUNT.load(Ordering::SeqCst);
    if panicked || after != before {
        Some(format!(
            "parser {:?} PANICKED on fuzz input (seed={seed}, len={len})",
            fmt.container
        ))
    } else {
        None
    }
}

/// Build a small, valid archive of `fmt` in-process (so mutation fuzzing starts from a real file that
/// reaches deep into the parser), or `None` for formats with no writer (ISO).
fn seed_archive(fmt: Format, dir: &Path) -> Option<Vec<u8>> {
    let src = dir.join("s");
    std::fs::create_dir_all(src.join("d")).ok()?;
    std::fs::write(src.join("d/a.txt"), b"fuzz seed alpha ".repeat(20)).ok()?;
    std::fs::write(
        src.join("d/b.bin"),
        (0..2000u32).map(|i| i as u8).collect::<Vec<_>>(),
    )
    .ok()?;
    let arc = dir.join("seed.arc");
    engine::create::create(
        &arc,
        fmt,
        &[src.join("d")],
        CreateOptions::default(),
        &NullSink,
    )
    .ok()?;
    std::fs::read(&arc).ok()
}

/// Randomly corrupt a copy of `base`: a mix of byte flips, range zeroing, truncation, and appends.
fn mutate(base: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut v = base.to_vec();
    let ops = 1 + rng.below(8);
    for _ in 0..ops {
        if v.is_empty() {
            break;
        }
        match rng.below(5) {
            0 => {
                let i = rng.below(v.len());
                v[i] ^= rng.byte();
            }
            1 => {
                // zero a short run
                let i = rng.below(v.len());
                let n = 1 + rng.below(16);
                for b in v.iter_mut().skip(i).take(n) {
                    *b = 0;
                }
            }
            2 => {
                // truncate
                let keep = rng.below(v.len());
                v.truncate(keep);
            }
            3 => {
                // append random bytes
                let n = rng.below(64);
                for _ in 0..n {
                    v.push(rng.byte());
                }
            }
            _ => {
                // set a byte to a random value (can hit length/count fields)
                let i = rng.below(v.len());
                v[i] = rng.byte();
            }
        }
    }
    v
}

fn random_buf(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(8192);
    (0..len).map(|_| rng.byte()).collect()
}

#[test]
fn parsers_survive_random_and_mutated_input() {
    let dir = std::env::temp_dir().join(format!("cram-fuzz-seed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Formats to fuzz. ISO has no writer → random-only (plus a CD001-seeded variant below).
    let writable = [
        Format::zip(),
        Format::sevenz(),
        Format::tar(Codec::None),
        Format::cram(Codec::None),
    ];

    let n = iters();
    let path = dir.join("input.bin"); // one staging file, overwritten per input
                                      // Quiet the panic hook for the duration so a *caught* panic doesn't spam stderr; restore after.
    let prev = panic::take_hook();
    panic::set_hook(Box::new(|_| {
        PANIC_COUNT.fetch_add(1, Ordering::SeqCst);
    }));

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut failure: Option<String> = None;

    'fuzz: {
        // 1) Pure-random buffers through every parser (incl. ISO).
        for fmt in writable.iter().copied().chain([Format::iso()]) {
            for _ in 0..n {
                let seed = rng.0;
                let buf = random_buf(&mut rng);
                if let Some(m) = feed(fmt, &buf, seed, &path) {
                    failure = Some(m);
                    break 'fuzz;
                }
            }
        }

        // 2) Mutation fuzzing from a valid seed archive (reaches deep parser paths).
        for fmt in writable {
            if let Some(base) = seed_archive(fmt, &dir) {
                for _ in 0..n {
                    let seed = rng.0;
                    let mutated = mutate(&base, &mut rng);
                    if let Some(m) = feed(fmt, &mutated, seed, &path) {
                        failure = Some(m);
                        break 'fuzz;
                    }
                }
            }
        }

        // 3) ISO with a valid `CD001` primary-descriptor magic so the fuzzer gets past the front gate
        //    into parse_vol / parse_dir (the interesting, recently-hardened code).
        let mut svd = vec![0u8; 20 * 2048];
        svd[16 * 2048] = 1; // primary VD type
        svd[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        for _ in 0..n {
            let seed = rng.0;
            let mut buf = svd.clone();
            for _ in 0..40 {
                let i = rng.below(buf.len());
                buf[i] = rng.byte();
            }
            buf[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001"); // keep past the gate
            if let Some(m) = feed(Format::iso(), &buf, seed, &path) {
                failure = Some(m);
                break 'fuzz;
            }
        }
    }

    // Restore the real hook BEFORE failing, so the seed message is printed (not swallowed).
    panic::set_hook(prev);
    let _ = std::fs::remove_dir_all(&dir);
    if let Some(msg) = failure {
        panic!("{msg}");
    }
}
