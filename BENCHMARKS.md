# Benchmarks

Measured 4 August 2026 against 7-Zip 26.01 and WinRAR 7.12/7.13. Every figure is the minimum
of repeated runs on a warm page cache. Commands are given in full so the numbers can be
checked.

Nothing in this file is estimated. Where a number is missing, it says so.

## Summary

On a 94,753-file Linux kernel source tree, cram's default setting writes a 197 MB archive in
7.93 s. For 7-Zip to write a smaller archive it needs `-mx=5`, which takes 5.4x as long and
4.2x the memory. For 7-Zip to finish faster it needs `-mx=1`, which is 12% larger.

At maximum effort cram writes a **smaller** archive than 7-Zip's default level on both
corpora: 164,607,029 bytes against 165,227,785 on the kernel tree, and 49,228,133 against
49,597,414 on Silesia, using 12% less memory in the first case. It takes 24% longer to get
there.

Extraction is where the margin is largest: 1.88 s against 7-Zip's 5.55 s on the kernel tree.
**Read that number with the caveat in Method below** — cram and 7-Zip did not extract the
same number of files, and roughly 8% of the difference is accounting rather than speed.

WinRAR is not on the speed/size frontier at any level on the kernel tree.

cram does not win everywhere. 7-Zip `-mx=9` reaches a ratio cram cannot match at any setting,
and cram's `--best` gives up its former speed advantage to reach the size it now reaches.
Both are quantified below.

## Machines

| | Windows | Linux |
|---|---|---|
| CPU | AMD Ryzen 7 3700X, 8C/16T | AMD Ryzen 9 5900X, 24 vCPU |
| RAM | 15.9 GiB | 23 GiB |
| OS | Windows 11 Pro 26200 | Ubuntu 24.04.4 |
| Storage | NVMe, NTFS | virtio disk, ext4 `noatime` |
| Notes | Defender active | KVM guest; the guest does not address the host NVMe directly |

Tool versions: cram 1.0.0 (`zstd-c, download, phash`) · 7-Zip **26.01 on both machines** ·
RAR 7.13 (Windows) and 7.12 (Linux; 7.13 has been withdrawn by RARLab).

## Method

- Best of 2 runs, page cache warmed immediately before each run.
- All tools receive the same explicit input list, so none silently includes or skips a file.
- All tools are given the machine's full thread count (16 on Windows, 24 on Linux).
- Archives are deleted between runs. 7-Zip and RAR append to an existing archive otherwise.
- Peak RSS on Linux via `/usr/bin/time -f '%M'`. Not captured on Windows.
- Output is written to a file handle, never a pipe.
- **Two timings are reported for every operation.** None of these tools calls `fsync`, so on a
  23 GiB machine a 1.6 GB archive can still be entirely in the page cache when the process
  exits. The plain figure stops when the tool returns, which is what every published archiver
  comparison measures; the `+sync` figure includes a full flush, which is what it costs to
  have the bytes actually on disk. They differ by more than 3x for extraction.
- cram is built with the shipping feature set (`download,zstd-c,phash`). A default-feature
  build silently runs XZ at the default level instead of zstd and is not the shipping tool.

**The three tools do not archive the same file set, and it favours cram.** The kernel tree
holds 99 symbolic links, twelve of which point at directories. 7-Zip and WinRAR dereference
them, so they archive and later extract **102,851 files** where cram archives **94,753** —
the extra 8,098 being a duplicated copy of the subtrees behind those twelve directory links.
cram archives no symlinks at all and reports each one it skipped (see `docs/CRAM_FORMAT.md`,
"Symbolic links"). So the competitors are doing about 8% more work in the extract column, and
cram's lead there is smaller than the raw numbers show. The size columns are barely affected,
since duplicated content compresses away for all three.

```
cram a OUT.cram <inputs> [--fast|--best]
7zz  a -t7z -mx=N -mmt=<threads> -bso0 -bsp0 OUT.7z <inputs>
rar  a -mN -s -md64m -mt<threads> -r -y -idq OUT.rar <inputs>
```

`-s` on RAR is required for a fair comparison. 7-Zip is solid by default and RAR is not.
Without it RAR produced 286,287,804 bytes on the kernel tree instead of 186,024,063, a 35%
penalty caused entirely by the missing switch. `-md64m` matches 7-Zip's 64 MB dictionary.

## Corpora

| corpus | files | bytes | source |
|---|---|---|---|
| Linux kernel tree | 94,753 | 1,615,437,663 | commit `2d2338c93da79b3bfe4b6099a931d9468d539952` (v7.2-rc5), `.git` excluded |
| Silesia | 12 | 211,938,580 | `silesia.zip` sha256 `0626e25f45c0ffb5dc801f13b7c82a3b75743ba07e3a71835a41e3d9f63c77af` |
| enwik8 | 1 | 100,000,000 | mattmahoney.net |
| enwik9 | 1 | 1,000,000,000 | mattmahoney.net |
| Canterbury | 11 | 2,810,784 | corpus.canterbury.ac.nz |
| Large Canterbury | 3 | 11,159,482 | corpus.canterbury.ac.nz |
| Calgary | 18 | 3,251,493 | corpus.canterbury.ac.nz |

The Silesia archive hashes identically on both machines, so it is the only corpus valid for
cross-machine comparison. The Windows kernel checkout was converted to CRLF by
`core.autocrlf=true` and measures 1,658,608,550 bytes, 2.67% larger than the Linux checkout
of the same commit. Kernel figures are therefore comparable between tools on one machine,
not between machines.

## Linux kernel tree, 1,615,437,663 bytes

Ryzen 9 5900X, 24 threads. Sorted by create time. `+sync` includes a full flush to disk.

| tool | level | size | ratio | create | +sync | MiB/s | verify | extract | +sync | peak RSS |
|---|---|---|---|---|---|---|---|---|---|---|
| cram | `--fast` | 259,779,988 | 6.219x | **3.12 s** | 3.69 s | 493.8 | 1.02 s | 2.17 s | 7.19 s | 332 MB |
| 7-Zip | `-mx=1` | 221,197,968 | 7.303x | 6.15 s | 6.60 s | 250.5 | 0.74 s | 5.67 s | 10.70 s | 183 MB |
| cram | default | 197,601,525 | 8.175x | 7.93 s | 8.35 s | 194.3 | 1.13 s | **1.88 s** | **6.70 s** | 1.37 GB |
| WinRAR | `-m1` | 262,814,946 | 6.147x | 22.20 s | 22.85 s | 69.4 | 4.45 s | 9.22 s | 14.34 s | 534 MB |
| 7-Zip | `-mx=5` | 165,227,785 | 9.777x | 42.64 s | 42.90 s | 36.1 | 1.09 s | 5.55 s | 10.58 s | 5.69 GB |
| cram | `--best` | **164,607,029** | **9.814x** | 52.98 s | 53.15 s | 29.1 | 1.34 s | 3.07 s | 7.79 s | 5.00 GB |
| WinRAR | `-m3` | 193,418,163 | 8.352x | 56.96 s | 57.10 s | 27.0 | 3.78 s | 8.54 s | 13.48 s | 550 MB |
| WinRAR | `-m5` | 186,509,619 | 8.661x | 77.07 s | 77.13 s | 20.0 | 3.75 s | 8.57 s | 13.69 s | 1.07 GB |
| 7-Zip | `-mx=9` | 154,370,692 | 10.465x | 84.38 s | 84.62 s | 18.3 | 1.58 s | 6.04 s | 11.12 s | 15.86 GB |

A configuration is on the speed/size frontier if no other configuration is both smaller and
faster. Here that is cram `--fast`, 7-Zip `-mx=1`, cram default, 7-Zip `-mx=5`, cram `--best`
and 7-Zip `-mx=9`. **All three WinRAR levels are dominated**: `-m1` by cram `--fast`, which is
both 1.2% smaller and 7.1x faster, and `-m3` and `-m5` by 7-Zip `-mx=5`.

cram `--best` is 0.38% smaller than 7-Zip `-mx=5` on 12% less memory, and 24% slower. Neither
dominates the other.

7-Zip `-mx=9` reaches a ratio no cram setting matches, and needs 15.86 GB to do it — more than
the total RAM of many machines this software is expected to run on.

## Silesia, 211,938,580 bytes

Ryzen 9 5900X, 24 threads. Sorted by create time.

| tool | level | size | ratio | create | +sync | MiB/s | verify | extract | +sync | peak RSS |
|---|---|---|---|---|---|---|---|---|---|---|
| cram | `--fast` | 69,474,237 | 3.051x | **0.32 s** | 0.48 s | 631.7 | 0.25 s | 0.28 s | 0.78 s | 171 MB |
| 7-Zip | `-mx=1` | 59,126,576 | 3.585x | 0.55 s | 0.68 s | 367.5 | 0.14 s | 0.25 s | 0.75 s | 102 MB |
| WinRAR | `-m1` | 66,683,749 | 3.178x | 1.46 s | 1.60 s | 138.4 | 0.43 s | 0.56 s | 1.01 s | 510 MB |
| cram | default | 58,280,168 | 3.637x | 1.63 s | 1.77 s | 124.0 | 0.25 s | 0.29 s | 0.75 s | 756 MB |
| WinRAR | `-m3` | 54,231,315 | 3.908x | 3.52 s | 3.65 s | 57.4 | 0.48 s | 0.59 s | 1.10 s | 533 MB |
| WinRAR | `-m5` | 53,150,010 | 3.988x | 5.71 s | 5.84 s | 35.4 | 0.47 s | 0.56 s | 1.04 s | 1.05 GB |
| 7-Zip | `-mx=5` | 49,597,414 | 4.273x | 18.42 s | 18.55 s | 11.0 | 0.83 s | 0.93 s | 1.41 s | 907 MB |
| cram | `--best` | **49,228,133** | **4.305x** | 18.77 s | 18.91 s | 10.8 | 0.49 s | 0.68 s | 1.15 s | 1.91 GB |
| 7-Zip | `-mx=9` | 48,688,243 | 4.353x | 37.14 s | 37.21 s | 5.4 | 1.31 s | 1.41 s | 1.85 s | 2.09 GB |

Only WinRAR `-m1` is dominated here. On twelve large files WinRAR is competitive and its
`-m3` and `-m5` are both on the frontier; the kernel-tree picture is different because WinRAR
reaches only about 610% of 2400% available CPU on many small files.

cram `--best` is 0.75% smaller than 7-Zip `-mx=5` at 1.9% more time, but carries twice the
memory (1.91 GB against 907 MB). The memory relationship is corpus-dependent and does not
generalise from the kernel tree.

## Cross-machine check

**Measured against the previous revision (3 August 2026) and not re-run since.** The pack
size at `--best` has changed from 8 MiB to 32 MiB, so the cram figures below no longer
correspond to the `--best` in the tables above; they are kept because what they demonstrate
is cross-machine byte-identity, not speed.

| | Windows, 16T | Linux, 24T |
|---|---|---|
| cram `--best` | 50,195,004 in 15.16 s | 50,195,004 in 9.12 s |
| 7-Zip `-mx=9` | 48,688,236 in 50.41 s | 48,688,243 in 30.78 s |
| WinRAR `-m5` | 53,147,219 in 6.08 s | 53,150,010 in 5.36 s |

Ordering was identical on both machines. cram produced **exactly the same 50,195,004 bytes**
on Windows and Linux from independently compiled binaries. 7-Zip differed by 7 bytes,
WinRAR by 2,791.

That cram property is expected to survive the pack-size change, since pack size is now chosen
by effort level rather than by anything about the machine, and a test pins the one input that
does vary by hardware (`tests/batch_invariance.rs`, which asserts that the number of packs
compressed concurrently cannot change the archive). **It has not been re-verified across two
machines since, and should be before this claim is repeated anywhere public.**

## Determinism

Output was byte-identical across every repeated run of all seven corpora, on a cold cache
and a warm one, across separate sessions, and across Windows and Linux. cram writes no
timestamps and does not let thread scheduling reach the output.

## Standard corpora

cram `--best`, Windows. Ratios are deterministic and hardware-independent, so published
figures from the Large Text Compression Benchmark and lzbench are cited directly.

| corpus | input | cram `--best` | ratio |
|---|---|---|---|
| Calgary | 3,251,493 | 854,052 | 3.807x |
| Canterbury | 2,810,784 | 485,666 | 5.787x |
| Large Canterbury | 11,159,482 | 2,571,006 | 4.341x |
| enwik8 | 100,000,000 | 27,457,101 | 3.642x |
| Silesia | 211,938,580 | 50,195,004 | 4.222x |
| enwik9 | 1,000,000,000 | 240,181,012 | 4.164x |

Against published references:

| | cram | reference | delta |
|---|---|---|---|
| Silesia | 50,195,004 | zstd -22, 52,333,880 | cram 4.1% smaller |
| Silesia | 50,195,004 | brotli -11, 50,407,795 | cram 0.4% smaller |
| Silesia | 50,195,004 | xz -9, 48,745,306 | cram 3.0% larger |
| enwik8 | 27,457,101 | zstd -22, 25,405,601 | cram 8.1% larger |
| enwik9 | 240,181,012 | zstd -22, 215,674,670 | cram 11.4% larger |

Deduplication contributed **zero bytes** on all six of these corpora, so they measure the
entropy stage alone. On the kernel tree it contributed 45.5 MiB, 2.88% of input.

## Where cram loses

**7-Zip `-mx=9` is out of reach.** 154,370,692 bytes against cram `--best`'s 164,607,029, a
6.2% gap on the kernel tree that no cram setting closes. `.cram` compresses each pack
independently, so its match window is one pack — 32 MiB at `--best` — where LZMA's solid block
spans the whole archive. That is structural, not a tuning gap, and the frozen format caps a
pack at 64 MiB. It costs 7-Zip 15.86 GB of RAM to get there.

**`--best` is no longer the fast option it was.** It bought its current size by moving from
8 MiB to 32 MiB packs, and that cost create time: 47.85 s previously against 52.98 s now on
the kernel tree, and 9.12 s against 18.77 s on Silesia. It buys 4.5% and 1.9% of size
respectively. On Silesia in particular that is a poor trade if size is not the goal.

**Ratio falls behind on large homogeneous text.** enwik9 becomes many independent packs, so
the penalty grows with input size and homogeneity. Silesia is within 3% of `xz -9` and enwik9
is 11% behind `zstd -22`. Larger packs narrow this but do not close it.

**Memory at `--best` is high.** 5.00 GB on the kernel tree, against WinRAR's 1.07 GB for a
comparable-ratio setting. It is below 7-Zip `-mx=5`'s 5.69 GB and far below `-mx=9`'s
15.86 GB, but it is not a small number, and it scales with pack size. The create path sizes
its pack batch from available RAM, so a smaller machine uses less and takes longer rather than
failing; that behaviour has not been measured on a genuinely small machine.

**Symbolic links are not archived at all.** No format cram writes can record a link target.
Each one is reported by name at create time rather than dropped in silence, but an archive of
a tree containing symlinks is not a complete copy of it. See `docs/CRAM_FORMAT.md`.

### Fixed since the previous measurement

Three entries in this section are gone because the defects behind them were fixed, not because
the benchmark changed:

- **Extraction of many small files.** Previously 39 files/s, aborted after 666 s having written
  24,452 of 94,753 files, and described here as roughly two orders of magnitude off. Now 1.88 s
  at the default level, faster than either competitor. The cause was the extraction scheduler
  fanning out by entry across a shared pack cache; see `docs/PERFORMANCE_FINDINGS.md` §12-13.
- **Verification.** Previously 17.64 s at `--best` against 7-Zip's 1.59 s. Now 1.34 s, faster
  than `-mx=9`. It had been running on a single thread on a 24-thread machine.
- **`--best` being dominated on the kernel tree.** 7-Zip `-mx=5` was both 4.1% smaller and
  1.18x faster. It is now 0.38% larger than cram `--best`.

## Limitations

- One machine per operating system. No aggregation across hardware.
- Peak memory was not captured on Windows.
- The Linux machine is a KVM guest. Disk behaviour passes through the hypervisor.
- Windows Defender was active throughout the Windows runs.
- Source and archive shared one physical device on both machines.
- RAR versions differ by one point release between the two machines.
- **The extract column is not a like-for-like comparison.** 7-Zip and WinRAR wrote 102,851
  files where cram wrote 94,753, because they dereference the tree's twelve directory
  symlinks and cram archives no symlinks at all. About 8% of cram's extract advantage is that
  accounting difference rather than throughput. Re-measuring on a symlink-free corpus would
  settle it and has not been done.
- Only the Linux machine was re-measured for this revision. The Windows figures from the
  previous revision have been removed rather than carried forward against new Linux numbers.
- The `--best` pack size changed between revisions (8 MiB to 32 MiB), so `--best` rows are not
  comparable to any earlier published table. Every other level changed too.
- cram's calibration profile (`hw.rs`) is consumed by the extraction planner, not by the
  create path, so create figures do not depend on it. The Linux machine's cached profile was
  inaccurate at the time of measurement and does not affect any create number here.

## Reproducing

```bash
# corpora
curl -O https://sun.aei.polsl.pl//~sdeor/corpus/silesia.zip     # sha256 0626e25f...
git init linux && cd linux
git remote add origin https://github.com/torvalds/linux.git
git fetch --depth 1 origin 2d2338c93da79b3bfe4b6099a931d9468d539952
git checkout FETCH_HEAD

# one configuration, as run
cram a out.cram <inputs>
7zz  a -t7z -mx=5 -mmt=$(nproc) -bso0 -bsp0 out.7z <inputs>
rar  a -m5 -s -md64m -mt$(nproc) -r -y -idq out.rar <inputs>
```

Pass the same explicit input list to each tool, delete the archive between runs, warm the
cache first, and take the minimum of at least two runs.
