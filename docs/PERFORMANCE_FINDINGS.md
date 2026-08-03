# Performance findings, 2–3 August 2026

Open issues found while benchmarking against 7-Zip 26.01 and WinRAR 7.12/7.13. Measurements
are in `BENCHMARKS.md`. Each item states what was observed, where the code is, and what is
hypothesis rather than fact.

Machines: Ryzen 7 3700X / 16T / 15.9 GiB / Windows 11, and Ryzen 9 5900X / 24 vCPU /
23 GiB / Ubuntu 24.04 (KVM guest).

---

## 1. Extraction of many small files is pathologically slow

**Severity: highest. Unresolved.**

Extracting the 94,753-file kernel tree from a `--best` `.cram` archive ran at a sustained
**39 files/s / ~1.3 MiB/s** while occupying 7.7 cores. After 666 s it had written 24,452
files (855 MB of 1,615 MB). The run was aborted; a full extract would have taken over an
hour. For scale, `7zz t` decodes the same content in 1.59 s.

Observations:

- Files extracted first averaged 35 KB against the tree's 17 KB mean, confirming
  largest-first scheduling was in effect.
- CPU was high (7.7 cores) while output was ~1.3 MiB/s, so the cores were doing work that
  did not reach the disk.

**Hypothesis, not confirmed.** `engine/parallel.rs` sorts destination groups
longest-processing-time-first (`groups.sort_by_key(|g| Reverse(...))`). That is correct for
formats where entries are independent, which is what the code was written for. In `.cram`,
entries' chunks live in shared ~8 MiB packs, so LPT ordering schedules neighbouring workers
onto unrelated packs. `PACK_CACHE_CAP` is 256 MiB, i.e. 32 resident packs at
`PACK_TARGET = 8 MiB`, against 16–24 concurrent workers each pulling a different pack. If
the cache thrashes, every entry costs a whole pack decode. 39 files/s x 8 MiB is ~312 MiB/s
of decode, which is the right order of magnitude for that explanation.

**Next step, cheap and decisive.** Probe the extraction rate of the *default-level* archive
(zstd packs) against the `--best` archive (XZ packs) for 60 s each. zstd decodes several
times faster than LZMA. If both crawl at a similar rate the codec is not the cause and the
access pattern is. Also extract the 12-file Silesia archive as a control: if that is fast,
the problem is specific to many small entries. A script for this exists at
`extract-probe.sh` in the session scratch; it was written but not run.

If confirmed, the fix is to order `.cram` extraction by pack rather than by size, so that
workers sharing a pack run together.

---

## 2. `cram t` is single-threaded

**Severity: high. Cause confirmed.**

`engine/verify.rs` contains no `rayon`, no `par_iter`, no `ThreadPool` and no worker count.
It streams entries one at a time.

Measured on the 24-core Linux machine, kernel tree, `--best` archive: **17.64 s for
1,540.6 MiB = 87.3 MiB/s aggregate**. A calibration run on the same idle machine measured
single-core LZMA decode at **145.9 MiB/s**. Aggregate throughput below one core's rate is
the signature of serial execution.

7-Zip verifies the same content in 1.59 s and WinRAR in 3.56 s. This is the one clearly
published weakness in `BENCHMARKS.md`.

Packs are independent standalone streams and `PackCache` already exists, so the parallel
pattern in `engine/parallel.rs` should transfer. Sixteen-way pack decode would put verify in
the 2–3 s range, at parity with 7-Zip.

Note the penalty is codec-specific: verify is 17.64 s at `--best` (XZ packs) and 3.05 s at
the default level (zstd packs).

---

## 3. `PACK_TARGET` caps the match window at 8 MiB

**Severity: medium. Cause confirmed, fix has a real trade-off.**

`formats/cram.rs` sets `PACK_TARGET = 8 * 1024 * 1024`. Unique chunks are grouped into packs
of that size and each is compressed as one standalone stream, so the LZMA match window can
never exceed 8 MiB. `xz -9` carries 64 MiB.

Confirmed by counting XZ stream magics (`fd 37 7a 58 5a 00`) in the output: 12 streams in
enwik8 (95.4 MiB), 26 in Silesia (202.1 MiB), matching `PACK_TARGET` to within a pack.

**Do not assume raising it recovers much.** lzbench's own table contains the controlled
experiment: xz `-6` and `-9` differ only in dictionary size (8 MiB vs 64 MiB), and on
`silesia.tar` they give 49,195,929 and 48,745,306 — a **0.92%** gain. cram at 8 MiB packs
produces 50,195,004, so roughly two thirds of its 3.0% deficit against `xz -9` is structural
(range-coder restarts, the per-chunk index, pack framing) and not the dictionary.

Blocked packs are also weaker than a sliding window of the same size, since data at the
start of a pack has no context. 64 MiB packs behave closer to `xz -8`.

Costs of raising it, on a 16 GiB machine:

| pack size | random-access latency | cache slots at 256 MiB | packs in 1.6 GB |
|---|---|---|---|
| 8 MiB | ~67 ms | 32 | ~200 |
| 32 MiB | ~267 ms | 8 | ~50 |
| 64 MiB | ~533 ms | 4 | ~25 |

`MAX_PACK_RAW` is already 64 MiB and is the *reader's* rejection bound, so larger packs are
legal in the frozen format with no reader change. If this is pursued, tie pack size to level
so `--best` gets larger packs and the default keeps `mount` and selective extract fast.
Note issue 1 first: larger packs would make a pack-locality problem worse, not better.

---

## 4. The calibration profile is not consumed by the create path

**Severity: medium (correctness of the abstraction, not of output).**

`hw.rs` measures per-core codec rates and persists them to `%APPDATA%\cram\profile.toml` or
`~/.config/cram/profile.toml`. `engine/mod.rs` loads the profile, calls `derive_plan`, and
passes `plan.workers` into `parallel::run` for **extraction**.

`engine/create.rs` contains no `hw::` reference. Pack parallelism comes from
`rayon::current_num_threads().clamp(1, 16)` in `formats/cram.rs`.

Consequences:

- Every create figure in `BENCHMARKS.md` is independent of calibration, which is why they
  are reported without a calibration caveat.
- The hardcoded `clamp(1, 16)` caps pack compression at 16 concurrent packs. On a 24-thread
  machine, 8 threads are unavailable to create by construction, while 7-Zip was given
  `-mmt=24`.
- Observed create CPU was ~930% of 2400%. `engine/create.rs` walks entries in a sequential
  loop doing read, FastCDC chunking and BLAKE3 hashing single-threaded, with parallelism only
  in the batched pack flush. That Amdahl ceiling sits above the thread clamp, so raising the
  clamp alone will not reach 24 cores.

---

## 5. Calibration can record badly wrong rates

**Severity: low, but it misleads whoever reads the profile.**

The Linux machine's cached profile, written 24 July, claimed `deflate_enc_mibs = 5.0`,
`deflate_dec_mibs = 136.8`, `lzma_dec_mibs = 27.3` — 3 to 5 times *slower* than the older
3700X, which is implausible for a 5900X. Re-running `calibrate --recalibrate` on an idle
machine gave 23.2, 953.3 and 145.9.

The micro-benchmark evidently has no guard against being run on a loaded machine, and the
resulting numbers are persisted permanently as this machine's "measured" rates.

Worth considering: sample several times and take the maximum, or record the load average
alongside the rates so an obviously contended measurement can be rejected on read.

Separately, `calibrate --recalibrate` without `--write-probe` discards a previously
*measured* write wall and replaces it with an estimate. On this machine that turned a
measured 1222.6 MiB/s into a 350 MiB/s estimate. The recalibration path should preserve a
measured wall it is not re-measuring.

---

## 6. `--best` is hard to justify in its current form

**Not a bug. A design question raised by the numbers.**

On the kernel tree, `--best` is off the speed/size frontier: 7-Zip `-mx=5` is 4.1% smaller
*and* 1.18x faster. Against the default level, `--best` costs 4.7x the time for 13% size,
and carries the verify penalty in issue 2 (17.64 s against 3.05 s).

The default level, by contrast, sits in a gap 7-Zip has no answer for: 199,188,776 bytes in
10.25 s where `-mx=1` is 11% larger and `-mx=5` takes 3.95x as long.

Fixing issues 1, 2 and 3 would change this picture. Until then, the default is the
configuration worth putting in front of users.

---

---

## 7. `File::open` is the largest single cost in create

**Severity: highest for create. Cause confirmed 2026-08-03, unfixed.**

Create was instrumented per phase (`CRAM_PROFILE=1`). On the 94,829-file kernel tree, 16-thread
Windows, default level:

```
open (serial)     16837.8 ms   52.4%   94,829 files, 177.6 us each
chunk (serial)     9895.5 ms   30.8%   read+cdc 7239, hash 722, other 1935
residual           5049.1 ms   15.7%
flush                321.2 ms    1.0%   blocked 1135 ms
index+trailer         12.0 ms    0.0%
walk 1531 ms, probe 2327 ms  (both before the writer exists)
```

Opening the source files costs more than chunking them and twenty times more than compressing them.
177 us per open against 5–20 us for a warm one, which is Defender in the path.

Three things follow.

- **Every file is opened twice.** `probe::classify_file` takes a path, so the store-vs-compress
  pre-pass opens and reads all 94,829 files, then the create loop opens all 94,829 again. Adding
  `walk`'s `fs::metadata`, the filesystem is touched three times per entry before a byte is
  compressed.
- **For `.cram` the probe is pure waste.** `CramArchiveWriter::add_file` takes `_hint: WriteHint`
  and ignores it, where `zip_write.rs:172` and `sevenz_write.rs:156` both consume it. The pass costs
  its 2.33 s and every one of those extra opens, and the `.cram` writer discards the verdict.
- **Hashing is not the bottleneck and never was.** BLAKE3 is 722 ms against read+CDC's 7239 ms,
  running at ~3.2 GiB/s. Parallelising it would buy almost nothing.

Absolute numbers move by nearly 2x with machine state — the same work measured 19 s and 36 s an hour
apart — so the proportions are the finding. Nothing here belongs in `BENCHMARKS.md`.

The fix is to prefetch opens on a small pool while the current file chunks, and to stop opening
twice. Neither touches the dedup table or pack ordering, so determinism should survive, but verify
by hash rather than assume.

---

## 8. Grouping the entry list by file type is 25x slower. Do not retry it as written

**Tried and rejected 2026-08-03. Negative result, recorded so it is not rediscovered.**

`.cram` compresses each ~8 MiB pack as one standalone stream, so what shares a pack decides how well
that pack compresses. Sorting the entry list by `(store, extension, path)` before the create loop —
the same reason 7-Zip sorts into its solid blocks — looked like a free ratio win, and determinism
survives it because the order stays a reproducible function of the input.

Measured: the kernel tree ran roughly **25x slower**. Killed after ten minutes having written 150 MB
of a 191 MB archive, against 20–36 s in tree order. The ratio benefit was never measured because no
run finished.

The cause is that the entry list is also the **read order**. In tree order a directory's metadata is
warm while its files are opened. Sorted by extension, every consecutive open lands in a different
directory, and issue 7 has already established that `File::open` is the dominant cost. Reordering
multiplies the one thing that was already worst.

The idea is still right; the layer was wrong. Read in tree order and route chunks to a per-class pack
buffer at assignment time, so locality and pack homogeneity are decided independently. That needs
pack ids reserved per buffer rather than a single `next_pack_id`, since two buffers fill at once and
`packs[id]` must stay addressable by id.

---

## Fixed since this document was written

- **The create barrier.** `flush_batch` compressed synchronously, so the chunker stopped dead for
  44.7% of create while sixteen threads worked, then fifteen threads idled while one chunked.
  Batches now compress on a background thread. Blocked time fell to ~1 s in 17, output is unchanged
  byte for byte, and the win was 19.51 s to 16.57 s — smaller than the phase split implied, because
  what it exposed is that ~95% of create is single-threaded.
- **A duplicated chunk loop.** `add_file` carried its own copy of `chunk_stream`, identical apart
  from accumulating `size`, while `chunk_stream`'s doc comment claimed both paths shared it. The copy
  was the one every ordinary file took.

## Reproducing

Corpora, hashes, exact invocations and the full result tables are in `BENCHMARKS.md`.
Per-phase create timings: `CRAM_PROFILE=1 cram a out.cram <inputs>`.
