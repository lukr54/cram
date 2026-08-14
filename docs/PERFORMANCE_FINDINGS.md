# Performance findings, 2–13 August 2026

Open issues found while benchmarking against 7-Zip 26.01 and WinRAR 7.12/7.13. Measurements
are in `BENCHMARKS.md`. Each item states what was observed, where the code is, and what is
hypothesis rather than fact.

Items 1–15 are from 2–3 August and concern `.cram`. Items 16–19 are from 13 August and concern
reading `.7z`, including the scheduler and calibration changes that came out of it.

Machines: Ryzen 7 3700X / 16T / 15.9 GiB / Windows 11, and Ryzen 9 5900X / 24 vCPU /
23 GiB / Ubuntu 24.04 (KVM guest).

---

## 1. Extraction of many small files is pathologically slow

**Severity: highest. RESOLVED, confirmed by measurement 2026-08-14.**

Re-run on the same corpus and the same machine. `--best` is now spelled `--small`:

| kernel tree, 86,618 files, 1605 MiB, to tmpfs | wall | files/s | MiB/s | peak |
|---|---|---|---|---|
| `.cram --small` (XZ packs) | **2.92 s** | 29,664 | 482 | 1713 MB |
| `.cram` default (zstd packs) | **2.32 s** | 37,335 | 607 | 962 MB |
| 7-Zip, same tree | 3.59 s | 25,973 | — | 1615 MB |

**760× the finding's 39 files/s**, and faster than 7-Zip on the same content. Every one of the
86,618 files verified byte-identical to the source tree at both levels.

**The hypothesis in this item was right.** LPT ordering scheduled neighbouring workers onto
unrelated packs and the pack cache thrashed. The fix is `locality_key` plus `coalesce_locality` in
`engine/parallel.rs`: entries sharing a pack become one work item, scheduled together, so a pack is
decoded about once instead of once per entry that touches it. Items 12 and 13 are the same defect
seen from the verify side.

The original text follows, since the reasoning is what made it findable.

---

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

**Severity: high. RESOLVED, confirmed by measurement 2026-08-14.**

Same corpus, same machine, two rounds each:

| `cram t`, kernel tree | was | now | 7-Zip |
|---|---|---|---|
| `--small` (XZ packs) | 17.64 s | **1.10 s** | 1.08 s |
| default (zstd packs) | 3.05 s | **0.90 s** | — |

16× at `--small`, and level with 7-Zip rather than eleven times behind it. **The claim below that
this is "the one clearly published weakness in `BENCHMARKS.md`" is no longer true** and that document
no longer says so.

Fixed by items 12 and 13 — `.cram` reported one decode unit, and verify's unit of work was the entry
rather than the pack, so every pack was decompressed 2.31 times. Verify now takes the same
`copy_unit` path extraction does.

The original text follows.

---

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

## 7. `File::open` dominates create on Windows, and is nearly free on Linux

**Severity: high on Windows only. Confirmed 2026-08-03. Partially fixed.**

> **Corrected 2026-08-03, later the same day.** This section originally read "`File::open` is the
> largest single cost in create" without qualification. Measured on the idle Linux box it is
> **4.1 us per file and 4.8% of create**, against **177.6 us and 52.4%** on Windows — a factor of 43,
> and it is Defender in the open path, not the code. The Windows figures below stand; the headline
> did not. Anything measured on that Windows machine describes Defender as much as it describes cram.

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

---

## 9. Raising the pack-batch clamp makes create SLOWER. Measured, not assumed

**Tried and rejected 2026-08-03. This contradicts issue 4, which is corrected below.**

`formats/cram.rs` sets `batch: rayon::current_num_threads().clamp(1, 16)`, so a 24-thread machine
compresses at most 16 packs at once. Issue 4 called that a defect and proposed wiring `derive_plan`
in, which returns `workers: hw.logical` for create. Both were wrong, and the measurement is
unambiguous. On the idle 24-thread box, kernel tree, default level:

| build | wall | peak RSS | chunk | flush | blocked |
|---|---|---|---|---|---|
| clamp 16 (shipped) | **8.36 s** | 1.22 GB | 3975 ms | 1862 ms | 2033 ms |
| clamp 64, batch 24 | 8.96 s | 1.62 GB | 5079 ms | 1328 ms | 1346 ms |

Compression did get faster: blocked fell 2033 -> 1346 ms, exactly as predicted. But chunking rose
3975 -> 5079 ms, and the loss is larger than the gain. Peak memory rose 400 MB as well, because
`batch x PACK_TARGET` is buffered twice under the overlap.

The chunker is a single thread on the critical path, and every extra compression worker takes cores
away from it. The same effect was measured independently on 16-thread Windows, where 11 rayon threads
beat 16 by 13%.

> **Superseded in part by finding 15.** Chunking moved onto a pool in `e7eb57b`, so it is no longer
> one thread. The conclusion below still stands for the compressing levels — `--auto` gains nothing
> from more chunk lanes and the pack compressor remains the constraint — but "single thread on the
> critical path" now describes only the commit stage.

**So the optimum compression concurrency is BELOW the core count, not equal to it.** `derive_plan`'s
`workers: hw.logical` for `Op::Create` is the wrong answer, and wiring it in unchanged would make
create slower on every machine with more cores than the current clamp. Whatever replaces the clamp
has to reserve headroom for the producer, and the right reserve is a thing to measure per machine
rather than guess — which is an argument for calibration, just not the one issue 4 made.

Output was byte-identical across clamp 16, clamp 24, and both earlier revisions, so pack batching
does not affect the archive.

---

## 10. Per-class pack buffers are worth nothing measurable. Built, measured, reverted

**Tried and rejected 2026-08-03.**

Section 8 concluded that grouping by compressibility belongs at the pack-assignment layer rather than
in the entry list. That was built: two class buffers, ids reserved on first append so an unused class
leaves no hole, `packs` addressed by id because the two fill out of order, and `add_file` finally
consuming the `WriteHint` it had been discarding. 209 tests green, clippy clean.

Then measured on three corpora:

| corpus | delta vs single buffer |
|---|---|
| kernel tree, 94,778 files | **+0.038%** (worse) |
| Silesia | 0.000%, byte-identical |
| 194 MiB, ~60% already-compressed blobs | **-0.016%** (better) |

Noise. Even a corpus that is mostly pre-compressed data gains 30 KB on 185 MB.

Two things explain it. On the first two corpora the probe classified essentially **nothing** as store
-- the kernel tree went from 222 packs to 223, Silesia stayed at 26 and produced an identical archive
-- so there was nothing to separate and the kernel tree's +0.038% is pack-boundary reshuffling, not
fragmentation. And where separation did happen, the gain was still negligible, because a `.cram` pack
is only 8 MiB. Cross-file grouping pays for 7-Zip because its solid blocks are far larger; at 8 MiB
the entropy coder adapts within a pack anyway. That is the same ceiling issue 3 measured from the
other direction, where the whole dictionary-size question was worth 0.92%.

Reverted. It added Option-indexed packs, per-class id reservation and a fallible `finish` to the core
write path for no measured benefit. Revisit only if `PACK_TARGET` grows a lot, and note issue 9 before
assuming more packs in flight is free.

---

## 11. The pack-batch cap: measured on one machine, not yet a rule

Following issue 9, the cap was swept on the idle 24-thread box, kernel tree, warm cache. Archive size
was **identical at every setting**, so batching never reaches the output.

| batch cap | 4 | 8 | **12** | 16 (shipped) | 20 | 24 |
|---|---|---|---|---|---|---|
| wall | 12.96 s | 8.75 s | **8.32 s** | 8.40 s | 8.75 s | 9.10 s |

The optimum is 12, exactly half the logical cores, and the basin is broad: 8 through 16 all sit within
5%. The shipped `clamp(1, 16)` is 1% off optimum here.

It is **not** 1% off everywhere. On 16-thread Windows, 11 beat 16 by 13%. So the same constant is
near-perfect on one machine and materially wrong on another, which is the argument for deriving it.

**No change shipped, deliberately.** Half-the-cores fits both data points loosely, but that is one
clean sweep and one noisy one, and fitting a cross-machine constant to a single machine is the exact
mistake sections 4, 7 and 8 each record a version of. What is needed is the same sweep on a second
machine, and then a rule -- or a calibration step that sweeps it once per host and stores the answer,
which is what `hw.rs` exists for.

---

## 12. `cram t` verified on one thread, because `.cram` reported one decode unit

`cram t` on the 1.6 GB kernel archive took 18.10 s at **96% CPU** on a 24-thread machine, while
7-Zip verified a comparable archive in 1.55 s at 354%. The verify loop was not the problem. The
number it planned against was.

`codec::plan::block_count` had no rule for `Container::Cram` and fell through to `1`. That count is
what `hw::derive_plan` fans out over, and its CPU-bound branch is

```rust
blocks.max(1).min(hw.logical).max(hw.physical.min(blocks.max(1)))
```

which for `blocks = 1` is one worker, on any hardware, forever.

**Extraction hid it.** An extract plan is write-bound, takes a different branch, and clamps to
`((physical * 3) / 4).clamp(4, 8)` — those are the eight workers seen during the extraction work.
Verify writes nothing, so it is the only verb that ever asked for the CPU-bound answer, and so the
only one that got the truthful `1`.

The fix is to let the backend report what it already knows: `RandomAccessReader::decode_units()`,
which `.cram` answers with its pack count. Verify then fans out over the same locality-ordered
groups extraction uses, since entries sharing a pack still have to be visited together.

Measured on the 24-thread box against the archives from the same sweep:

| | before | after | |
|---|---|---|---|
| `cram t --best`, linux | 18.10 s @ 96% | **7.24 s @ 1112%** | 2.50x |
| `cram t` auto, linux | 3.07 s @ 99% | **1.88 s @ 1594%** | 1.63x |
| `cram t --best`, silesia | 3.05 s | **0.99 s @ 477%** | 3.08x |
| `cram x` auto, linux | 2.26 s | 2.00 s | within noise |
| `cram x` auto, durable | 7.17 s | 6.92 s | within noise |

Extraction is unchanged because its plan already reached the write-bound branch. Making `blocks`
truthful removed a coin flip rather than changing a number: with `blocks = 1` the classifier compared
a projected `1 x decode_rate` against the measured write wall, so on a drive faster than one core's
decode rate it would have returned CPU-bound and planned **one extraction worker too**. Which branch
an extract took depended on how fast the destination drive measured.

This left `--best` verify 5.9x slower than 7-Zip `-mx=5` and burning 80 core-seconds where the
single-threaded pass burned 17. The shared `Mutex<PackCache>` looked like the suspect. It was not.
See §13.

## 13. Verify decompressed every pack 2.31 times. The unit of work was wrong

The lock was never the problem: `get_pack` already decodes *outside* it. The work was simply being
done more than once, and the cache is what hid that. Two workers that miss on the same pack both
decompress it, and `PackCache::insert` discards the loser **after** the CPU has been spent, so the
waste never appeared as a miss.

Counting it (`CRAM_PROFILE=1`, the new pack profile) on the 186-pack kernel archive:

| | decodes | per pack | decompressed |
|---|---|---|---|
| entry per task | 430 | 2.31 | 3446 MiB |
| pack per task + single-flight | 274 | **1.47** | 2203 MiB |

To verify 1615 MiB, the old path decompressed 3446 MiB.

Two things were wrong and both had to change.

**The unit of work was the entry, not the pack.** Ordering same-pack entries adjacently (the fix in
*Extract a pack once, not sixteen times*) is not the same as keeping them on one worker: rayon still
splits a cluster across workers and they then miss together. Grouping entries by locality key, so
one task owns one pack, means one decode and no race to lose.

**Nothing served a second comer.** `get_pack` now holds that pack's own lock across the decode, so a
worker wanting a pack already in flight waits for those bytes instead of producing its own copy.
Per-pack locks, at most one held at a time, so unrelated packs never serialise and they cannot
deadlock.

| | §12 | now | total |
|---|---|---|---|
| `cram t --best`, linux | 7.24 s @ 1112% | **3.65 s @ 1551%** | 4.96x from 18.10 s |
| `cram t` auto, linux | 1.88 s @ 1594% | **1.12 s @ 1822%** | 2.74x from 3.07 s |
| `cram t --best`, silesia | 0.99 s | **0.91 s** | 3.35x from 3.05 s |
| `cram x` auto | 2.00 s | 2.13 s | within noise |

Extraction does not move, which is consistent: it is write-bound on eight workers, so it races for a
pack far less often and the decode is not its bottleneck anyway.

Where verify now stands against the incumbents, same corpus and box:

| | archive | verify |
|---|---|---|
| `cram t` fast | 260 MB | 1.03 s |
| `7zz t -mx=5` | 165 MB | 1.08 s |
| `cram t` auto | 199 MB | 1.12 s |
| `7zz t -mx=9` | 154 MB | 1.58 s |
| **`cram t --best`** | **172 MB** | **3.65 s** |
| `rar t -m5` | 186 MB | 3.69 s |

`--best` now edges past WinRAR while producing an archive 7.6% smaller, and remains 3.4x behind
7-Zip `-mx=5`. That residue is mostly XZ against LZMA2 on the decode side, not scheduling.

**What is left.** 1.47 decodes per pack, not 1.00. Silesia reaches exactly 1.00 (26 packs, 12
files), so the residue is specific to a many-small-file tree: an entry straddling a pack boundary is
grouped by its *first* pack, so the worker owning pack N pulls in pack N+1, whose own owner may have
to decode it again if the cache evicted it meanwhile. 186 packs against roughly 104 extra decodes is
close to one per boundary. Closing it needs either a larger `PACK_CACHE_CAP` (peak RSS is already
2.2 GB at `--best`, which is the XZ dictionary per worker rather than the cache) or grouping a
straddling entry by every pack it touches. Worth perhaps 20-30% of the remaining decode CPU.

## 14. Pack size is the match window. 32 MiB is the knee, and the spec caps it anyway

`.cram` compresses each pack independently, so the pack **is** the archive's match window. 8 MiB of
context against LZMA's whole-archive solid block is why `7z -mx=5` beat `--best` on the kernel tree
on both axes at once, the only cell in `BENCHMARKS.md` where a competitor dominated outright.

Swept with `CRAM_PACK_TARGET`, linux tree, `--best`, 24 threads:

| pack | output | create | create RSS | verify | extract | packs | decodes/pack |
|---|---|---|---|---|---|---|---|
| 8 MiB | 172.37 MB | 47.28 s | 2431 MB | 4.41 s | 7.49 s | 186 | 1.47 |
| 16 MiB | 167.90 MB | 49.33 s | 3550 MB | 2.55 s | 4.38 s | 94 | 1.33 |
| **32 MiB** | **164.61 MB** | 53.83 s | 4870 MB | **1.30 s** | **2.94 s** | 47 | **1.00** |
| 56 MiB | 162.09 MB | 63.48 s | 7715 MB | 1.31 s | 3.45 s | 27 | 1.00 |
| *7z -mx=5* | *165.23 MB* | *40.93 s* | *5828 MB* | *1.08 s* | — | — | — |

**32 MiB is the knee on every axis simultaneously**, which was not the expected shape. Ratio gains
fall to 1.5% past it, create costs 18% more time and 58% more memory, verify stops improving
(1.31 s against 1.30 s), and extract gets *worse* (3.45 s against 2.94 s) because 27 packs across 24
workers balances less evenly than 47 do. The read-side was expected to keep improving with fewer,
larger packs and it does not.

At 32 MiB `--best` produces a **smaller archive than `7z -mx=5`** (164.61 MB against 165.23 MB) using
16% less memory, for 31% more create time. Neither dominates the other any more, and cram takes two
of the four frontier points at the high-ratio end where it previously held none.

It also drives decodes-per-pack to exactly 1.00, closing the straddling-entry residue from §13 as a
side effect: eight times fewer pack boundaries.

**No format change is justified.** The spec caps a pack's decompressed size at 64 MiB (§9 check 5,
enforced independently by `cram-extract`), so 56 MiB is the largest legal target once the one-chunk
overshoot is allowed for. Since the curve has already flattened at 32 MiB, that ceiling is not
binding and a v2 bump buys nothing. The question is closed rather than deferred.

**Not shipped as a default.** The knob exists; `PACK_TARGET` is still 8 MiB. Doubling create memory
to ~4.9 GB is a real cost on a 16 GB machine and that trade belongs to the product, not to this
document.

**The `--auto` arm of this sweep was invalid** and is not reported above: the dev-box build lacked
the `zstd-c` feature, so "auto" ran XZ preset 6 rather than the zstd path that ships. The `--best`
rows are unaffected, and were confirmed byte-identical across two independent runs. Auto still needs
measuring with the feature enabled, and it matters more than `--best` does, being what anyone gets
without passing a flag.

## 15. The chunker is no longer one thread, and the win is mostly pipelining, not parallelism

Finding 9 above concluded that "the chunker is a single thread on the critical path, and every extra
compression worker takes cores away from it". That was true when it was written and is no longer:
`e7eb57b` moved chunking onto a pool of lanes. This records what the pool is actually worth, because
the first number measured for it was wrong and got as far as a written claim before the repeat run
caught it.

Round-robin across configurations rather than all repeats of one config back to back, on the public
corpus (2.8 GB, 42,151 files, 15% duplicate). Medians; `store` n=5, `fast` n=3.

| lanes | `--store` | `--fast` |
|---|---|---|
| pre-pool (`8909a1e`) | 6.95 s | 6.20 s |
| 1 | 5.86 s | 5.65 s |
| 2 | 4.70 s | 4.56 s |
| 24 | 4.08 s | 3.96 s |

**`--store` gains 1.70x, `--fast` 1.57x, `--auto` gains nothing** — 10.73 to 10.97 s across every
lane count from 1 to 24, which is noise. At `--auto` the pack compressor is the whole cost and the
chunker was never in the way. Twenty-four lanes there cost 70% more CPU and a further gigabyte of
RSS for no wall-clock at all, which is why `prepare_lanes` caps the compressing case at 3.

**Most of the first jump is pipelining, not parallelism.** Serial `--store` spends 7.79 core-seconds;
one lane spends 7.99 — the same work — but finishes in 5.86 s instead of 6.95 because the lane
overlaps the read with the hash. The serial build sat at 55% CPU on a warm cache, under one core.
Everything past two lanes is worth about 15% more.

Two traps, both paid for:

- **A single sample is worthless here.** The first sweep measured `pre --store` once at 14.16 s and
  `w=1` once at 10.95 s, and both were stalls; a 1.8x disagreement between two runs of the same
  binary on the same bytes is what prompted the round-robin. A curve drawn through a stall is not a
  curve. The published 3.46x that came out of it was wrong and was retracted.
- **Ordering, not sample count, was the fix.** Running every repeat of a config back to back puts any
  slow patch of the machine entirely on one config, where it reads as that config's number.

Output was byte-identical across every lane count and across the pre/post commit boundary — 24 runs
per level, three levels — which is the reproducibility guarantee holding on real files rather than on
a fixture. `crates/cram-core/tests/chunk_lanes.rs` pins it at 1, 2, 4 and 16.

Shipped lane counts: `--store` gets 12 (`packs_are_cheap` is `matches!(policy, PackPolicy::Store)`),
`--fast` and `--auto` get 3. **`--fast` at 12 lanes measures about 7% faster for 380 MB more RSS**,
which is a trade rather than a free win and has not been taken.

---

## 16. 7z extraction ran on one thread, and four separate things kept it there

**Confirmed and fixed, 13 August 2026.** Measured on the 24-thread Linux box, Cram corpus 1.0,
destination `/dev/shm`, three rounds with the tool order rotated, every extraction checked against
the corpus `MANIFEST.sha256`.

Each of these was individually sufficient to keep extraction at ~1.1 effective cores, so fixing any
one of them alone would have shown nothing. That is the reason to record them separately.

1. **`sevenz.rs` never implemented `as_random_access`**, so no 7z ever reached the parallel path.
2. **Clustering was not enough.** With the unit merely *adjacent* in the schedule, rayon steals
   across the list, a worker hops units, and a backend caching "the unit I am on" evicts on every
   hop: 110 CPU-seconds against 11 sequential, ten re-decodes of every block, for almost no wall
   gain. `coalesce_locality` fuses a unit into one work item — 34 items instead of 41,305.
3. **The memory gate was all-or-nothing on the largest block.** A block holding a single entry needs
   no cache at all, and the corpus has one: a 263.3 MiB stored video, 5.5% over a 249.6 MiB
   per-worker budget, which vetoed the other 48 blocks. Extraction stayed at 1.30 cores on the very
   corpus the project publishes against, and only a benchmark noticed — the gate is invisible when
   wrong, because extraction still succeeds.
4. **`copy_entry` is the wrong shape for a solid format.** Serving a block through a per-entry
   interface means decoding the block and *holding* every entry's bytes: 1.8 GB peak RSS against
   7-Zip's 176 MB, to be 10% faster than it. `copy_unit` hands each entry over as it decodes, and is
   both smaller and faster — 175 MB and 38% less CPU than the sequential path it replaced, because
   the buffering was most of the cost.

| `cram.7z`, 49 blocks | wall | cpu | cores | peak |
|---|---|---|---|---|
| sequential | 8.62 s | 11.03 s | 1.28 | 138 MB |
| cached blocks | 2.96 s | 12.06 s | 4.07 | 1.0–1.8 GB |
| streaming | **1.35 s** | 11.68 s | 8.65 | 303 MB |
| 7-Zip 26.01 | 3.25 s | 4.74 s | 1.46 | 173 MB |

---

## 17. One solid folder is divisible after all, and the upstream support for it does nothing

**Confirmed and fixed, 13 August 2026.**

7-Zip's default writes an entire archive as ONE solid block (`Method = LZMA2:25`, `Blocks = 1`), so
§16 bought nothing there: 24.99 s against 7-Zip's 3.68 s, 6.8× behind on the archive shape a user is
most likely to be handed.

Its multi-threaded encoder resets the LZMA2 dictionary at each thread-block boundary, and a chunk
with a dictionary reset decodes cold. Walking the corpus archive's framing: **47,011 chunks, 21
dictionary resets**, segments of 110.9–128.0 MiB. The walk is self-validating — it consumed the pack
stream exactly and totalled 2,800,604,582 unpacked bytes, the corpus size to the byte.

**`lzma-rust2` already finds these seams and cannot use them.** `Lzma2ReaderMt` splits on exactly the
same condition (`control >= 0xE0 || control == 0x01`), `sevenz-rust2` selects it whenever
`threads >= 2`, and `ArchiveReader::open` already defaults `thread_count` to
`available_parallelism()`. None of it is feature-gated. Measured:

| threads | wall | cores | peak |
|---|---|---|---|
| 1 | 15.43 s | 1.01 | 49 MB |
| 4 | 19.68 s | 1.01 | 577 MB |
| 24 | 19.71 s | 1.01 | 577 MB |

28% slower and 528 MB heavier for no parallelism, because it refills its work queue only when the
queue is *empty* and spawns a worker only when the queue is non-empty — a depth-1 prefetch pipeline.
Its work units are also whole `Vec`s of input and output, so even fixed it would buffer ~128 MiB per
unit in flight. Hence `formats/lzma2seg.rs` cutting the seams in cram and feeding them to the
scheduler that already exists.

Two things the format forces. A segment ends where the next one's control byte begins, so read alone
it has no end marker and the decoder runs off the end looking for one more chunk header
(`UnexpectedEof`, all 21, first attempt). Giving it the rest of the pack stream and dropping it early
is simpler than synthesising a terminator, and it makes an entry straddling a boundary fall out for
free. And the dictionary window is sized from the segment rather than from the archive's declared
dictionary, because `sevenz-rust2` keeps `Coder::properties` private — a segment opens on a reset, so
nothing inside it reaches back past its start. **That over-allocates about 4× on 7-Zip's output**
(128 MiB windows where 32 MiB would do) and is the bulk of the 2795 MB below. Recoverable if a
`properties()` accessor lands upstream.

| `sevenz.7z`, 1 block / 21 segments | wall | cpu | cores | peak |
|---|---|---|---|---|
| before | 25.01 s | 27.12 s | 1.08 | 609 MB |
| after | **3.66 s** | 29.42 s | 8.04 | 2795 MB |
| 7-Zip 26.01 | 3.68 s | 16.77 s | 4.56 | 4876 MB |

**This is a property of the archive, not of the format.** An archive written `-mmt=1`, or smaller
than one thread-block, has one segment and gains nothing. No survey has been done of how common each
case is, so no claim is made about `.7z` files in general.

---

## 18. A write-bound plan asked for a wall it then declined to reach

**Confirmed and fixed, 13 August 2026. Then largely undone on 14 August, and that is the right
outcome** — the wall it was scaling by turned out not to be a ceiling, so most of the workers it
added were bought with a number that was measuring a cache. Read to the end before acting on the
middle of this item.

`classify` computed `min(units, physical) × decode_rate = 21 × 143.7 = 3018 MiB/s` against a measured
2689 MiB/s wall, called the extraction write-bound, and the write-bound branch then ran
`((physical * 3) / 4).clamp(4, 8)` = **8** workers. Actual throughput was 620 MiB/s, 23% of the wall
it had just declared itself bound by. Being write-bound is a claim about the ratio of two rates, so
the worker count now comes from those rates: `wall / decode_rate`, nineteen here. 4.16 s → 3.66 s.
The old value is kept as a floor, so this can only add workers.

**It is not free, and the cost lands where the win does not.** On an archive whose wall time is
already write-bound the extra workers buy nothing and still cost their CPU: `cram.7z` stays at 1.35 s
and goes from 6.4 to 11.7 CPU-seconds. `decode_rate` is a static per-codec estimate and cannot know
that half that corpus is stored data decoding at memcpy speed. A per-archive decode estimate, or a
governor that sheds workers that are not moving bytes, would fix it.

**RESOLVED 14 August: a write figure now says whether it is a ceiling.** The probe an extraction runs
writes 512 MiB and reported 331.9 MiB/s for the dev box's `/scratch`; `calibrate --write-probe`
writes 4 GiB and reported 84 MiB/s for the same volume. Both are measurements — the short one never
leaves the drive's cache. A wall 4× high asked for 4× the workers, and on tmpfs (2841 MiB/s, which is
memory bandwidth) it asked for twenty.

`hw::Wall` carries `sustained: bool`. Only a probe that watched throughput step down and sampled past
the step has seen a ceiling; anything else classifies the bottleneck but never sizes the pool, and
the distinction survives a reload (profile schema 4).

**A worker sweep, using the `CRAM_WORKERS` override added for it, put the knee at 8**: 1.33 s and
6.33 CPU-seconds, against 20 workers' 1.24 s and 11.27. The planner now picks 8. That is **40% less
CPU and 41% less memory for 11% more wall time** — a trade, and recorded as one rather than as a win.

That also settles the paragraph above: the extra workers were bought by the inflated wall, so
correcting the wall was the fix and no per-archive decode estimate was needed. The `Governor` in
`hw.rs` remains implemented, unit-tested, and called from nowhere.

**Worth knowing:** with an honest wall the scaling term is nearly always inert. On any destination
slower than the codec, `needed` is under 1 and the floor decides; it only fires for a RAM disk. If
that survives further measurement, the term is worth deleting rather than keeping as machinery that
never runs.

---

## 19. The write wall was cached per machine, and it is a property of the volume

**Confirmed and fixed, 13 August 2026.** Profile schema 3.

`write_wall_mibs` was one number for the computer. Whichever destination happened to be extracted to
first set it for every destination afterwards. Live on the dev box: a profile carrying
`write_wall_mibs = 2689.6`, measured against `/dev/shm`, planning extractions onto an ext4 disk.

Codec rates stay per machine — they belong to the CPU. Walls are now keyed by volume (`st_dev` on
unix, volume root on Windows) and a destination never measured gets no wall rather than another
volume's. Verified on one machine, two volumes: `/dev/shm` → 2717.1 MiB/s → 19 workers; `/scratch`
→ 331.9 MiB/s → 8 workers.

Schema 4 (14 August) adds the other half: a wall records whether it is a **ceiling** (`wall.<volume>`)
or a **burst** that may be nothing but cache (`burst.<volume>`). Keying by volume stopped one
destination's figure being offered for another; this stops a figure being scaled by when it was never
a measurement of the thing it is being used for. Both entries above were burst figures, which is why
neither now fields nineteen workers.

**Benchmark trap this exposed.** Two cram builds on one machine share
`~/.config/cram/profile.toml` and disagree about its schema, so each invalidated the other's *every
run*; every timing then silently included a full recalibration plus a 512 MiB write probe. It showed
as cram swinging 1.33–8.07 s while 7-Zip sat still at 3.28 s, and read exactly like a regression. An
A/B of two builds must give each its own `XDG_CONFIG_HOME` and a warm-up pass.

---

## 20. `.tar.gz` create used one core, and cutting the stream up costs almost nothing

**Fixed and measured, 14 August 2026.**

gzip is a single stream with no boundaries a writer has to respect, so cram wrote it through one
`GzEncoder` and used one core. It was marginally *slower* than stock `tar czf` on Silesia, 6.26 s
against 5.72 s.

pigz's trick applies: cut the tar stream into fixed chunks, deflate each with its own compressor,
end each with a **sync flush** so it stops on a byte boundary, and concatenate. Each compressor
starts empty, so no back-reference points outside its own chunk, which is what makes the chunks
separable. The result is one ordinary gzip member and `gzip -t` accepts it.

Dev box, 24 threads, default level, destination tmpfs, warm-up discarded, median of 3 rotated:

| corpus | before | after | stock `tar czf` |
|---|---|---|---|
| Silesia 203 MiB | 6.26 s | **0.72 s** | 5.72 s |
| enwik9 954 MiB | 29.82 s | **3.07 s** | 29.43 s |
| kernel tree 2.1 GB | 37.43 s | **5.43 s** | 32.29 s |

**Only create is parallel.** A standard `.gz` cannot be extracted in parallel by anyone, this
included: a decoder cannot find the block boundaries without inflating everything before them.

**The seam cost is 0.19–0.34%**, a quarter of what pigz reports for independent chunks, because the
chunk is 1 MiB rather than pigz's 128 KiB and a 32 KiB dictionary loss against a megabyte is
proportionally eight times smaller. On the kernel tree the archive is 1.40% *smaller* than gzip's
own output even after paying it.

**The trade is CPU and memory:** 30–39% more CPU, and peak RSS from 17 to 177 MB on Silesia. The
memory is the window — one core's worth of chunk each, plus output buffers — and nothing else.

**The CPU rise is not the seams, and one core does not regress.** That was worth checking rather
than assuming, since a single-core machine would pay the CPU and get no parallelism back. With
`RAYON_NUM_THREADS=1` on Silesia the chunked writer is 6.12 s / 6.07 CPU-s against the streaming
writer's 6.28 s / 6.23 — fractionally *cheaper*, twice. So cutting the stream up is free, and the
extra CPU appears only when the chunks run concurrently. Which of barrier spin or memory-bandwidth
contention it is has not been established and is not claimed here.

**Output is byte-identical at 1 thread and at 24.** Chunk boundaries come from byte offsets in the
tar stream, not from scheduling, so the pool width cannot change the archive.

## 21. A ranged 7z read decoded from the block, when it could start at a segment

**Fixed and measured, 14 August 2026.**

`read_range` is the mount primitive. On a block too large to cache it fell back to streaming from the
block's first byte, so reading sixteen bytes out of a 2.3 GB solid archive decoded however much of
that archive lay in front of them. A mount built on it would have been unusable, and the ROADMAP
said as much.

Finding 17 already cuts such a block into independently-decodable LZMA2 segments for extraction.
The ranged path now uses the same seams: decode starts at the segment holding the range. Reading on
past a segment boundary is ordinary — it is what the sequential decoder does — so a range crossing
one costs only its tail.

Measured against the 2.3 GB 7-Zip-written corpus archive, subject a 13,964,658-byte entry:

| | |
|---|---|
| decode from the block start, i.e. the old cost | 24.80 s |
| range at the entry's start | 0.535 s |
| range at the entry's end | 0.546 s |

**45× on the worst of them, and flat.** The flatness is the result: cost is now the segment holding
the range rather than the distance into the archive. Every range matched the same slice of the entry
decoded whole, which is the check that matters — a segment chosen one too far along raises no error,
it serves the wrong bytes.

**Nothing is held.** Only the LZMA2 dictionary window stays resident, so a segment far larger than
memory costs its window rather than its length. That settles the memory form of the ROADMAP's open
question about large segments; what is left is a mount-side latency decision, because nothing is
kept between calls — 32 single-byte reads at the tail cost 0.536 s each.

**A block that fits the cache is deliberately unchanged.** There the first read pays one decode and
every later one is free, and a segment decode per read would be worse.

---

## Fixed since this document was written

- **The create barrier.** `flush_batch` compressed synchronously, so the chunker stopped dead for
  44.7% of create while sixteen threads worked, then fifteen threads idled while one chunked.
  Batches now compress on a background thread, and output is unchanged byte for byte.

  **Measured on the idle 24-thread Linux box** — the Windows numbers taken the same day are
  worthless, that machine varying 19 s to 202 s for identical work:

  | | wall | peak RSS |
  |---|---|---|
  | `2f90515` before | 10.92 s | 1.03 GB |
  | `ebf73d5` overlap | 9.32 s | 1.20 GB |
  | `a7b88c3` single-open | **8.36 s** | 1.22 GB |

  **1.31x for the pair**, 1.17x from the overlap and a further 1.11x from opening each file once.
  Peak memory rises ~190 MB, which is the double buffering and is bounded.

  Note these runs archived `/scratch/bench/corpora/linux` **including its `.git`**, which the
  published methodology prunes: 2.1 GB against 1.6 GB, compressing to 472 MiB against 199 MiB
  because a packfile is already compressed. It is the same checkout, so the figures are valid as an
  A/B on one machine and are **not** comparable to the published table.
- **A duplicated chunk loop.** `add_file` carried its own copy of `chunk_stream`, identical apart
  from accumulating `size`, while `chunk_stream`'s doc comment claimed both paths shared it. The copy
  was the one every ordinary file took.

## Reproducing

Corpora, hashes, exact invocations and the full result tables are in `BENCHMARKS.md`.
Per-phase create timings: `CRAM_PROFILE=1 cram a out.cram <inputs>`.
