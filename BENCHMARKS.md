# Benchmarks

Measured 2 and 3 August 2026 against 7-Zip 26.01 and WinRAR 7.12/7.13, on two machines and
two operating systems. Every figure here is the minimum of repeated runs on a warm page
cache. Commands are given in full so the numbers can be checked.

Nothing in this file is estimated. Where a number is missing, it says so.

## Summary

On a 94,753-file Linux kernel source tree, cram at its default setting writes a 199 MB
archive in 10.25 s using 900 MB of RAM. For 7-Zip to write a smaller archive it needs
`-mx=5`, which takes 3.95x as long and 6.3x the memory. For 7-Zip to finish faster it needs
`-mx=1`, which is 11% larger.

cram does not win everywhere. `--best` is beaten on both size and speed by 7-Zip's default
level on that corpus, `t` verification is slower than both competitors, and extraction of
many small files is slow enough to count as a defect. All three are quantified below.

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

Ryzen 9 5900X, 24 threads.

| tool | level | size | ratio | create | MiB/s | verify | peak RSS |
|---|---|---|---|---|---|---|---|
| cram | `--fast` | 259,779,988 | 6.219x | 3.34 s | 461.3 | 3.57 s | 275 MB |
| 7-Zip | `-mx=1` | 221,197,968 | 7.303x | 6.02 s | 255.9 | 0.74 s | 183 MB |
| cram | default | 199,188,776 | 8.110x | 10.25 s | 150.3 | 3.05 s | 900 MB |
| WinRAR | `-m1` | 262,814,946 | 6.147x | 21.49 s | 71.7 | 4.35 s | 535 MB |
| 7-Zip | `-mx=5` | 165,227,785 | 9.777x | 40.46 s | 38.1 | 1.06 s | 5.56 GB |
| cram | `--best` | 172,366,964 | 9.372x | 47.85 s | 32.2 | 17.64 s | 2.25 GB |
| WinRAR | `-m3` | 193,418,163 | 8.352x | 53.33 s | 28.9 | 3.58 s | 551 MB |
| WinRAR | `-m5` | 186,509,619 | 8.661x | 72.65 s | 21.2 | 3.56 s | 1.04 GB |
| 7-Zip | `-mx=9` | 154,370,692 | 10.465x | 91.38 s | 16.9 | 1.59 s | 15.86 GB |

A configuration is on the speed/size frontier if no other configuration is both smaller and
faster. Here that is cram `--fast`, 7-Zip `-mx=1`, cram default, 7-Zip `-mx=5` and 7-Zip
`-mx=9`. cram `--best` and all three WinRAR levels are dominated.

7-Zip `-mx=9` peaks at 15.86 GB, which exceeds the total RAM of the Windows machine. On that
machine one 7-Zip run took 709 s against 127 s for three others; the memory figure explains
it.

## Silesia, 211,938,580 bytes

Ryzen 9 5900X, 24 threads.

| tool | level | size | ratio | create | MiB/s | verify | peak RSS |
|---|---|---|---|---|---|---|---|
| cram | `--fast` | 69,474,237 | 3.051x | 0.33 s | 612.5 | 0.71 s | 156 MB |
| 7-Zip | `-mx=1` | 59,126,576 | 3.585x | 0.52 s | 388.7 | 0.14 s | 104 MB |
| WinRAR | `-m1` | 66,683,749 | 3.178x | 1.37 s | 147.5 | 0.42 s | 510 MB |
| cram | default | 58,580,101 | 3.618x | 1.65 s | 122.5 | 0.66 s | 790 MB |
| WinRAR | `-m3` | 54,231,315 | 3.908x | 3.34 s | 60.5 | 0.45 s | 533 MB |
| WinRAR | `-m5` | 53,150,010 | 3.988x | 5.36 s | 37.7 | 0.44 s | 1.02 GB |
| cram | `--best` | 50,195,004 | 4.222x | 9.12 s | 22.2 | 3.00 s | 1.88 GB |
| 7-Zip | `-mx=5` | 49,597,414 | 4.273x | 15.99 s | 12.6 | 0.79 s | 908 MB |
| 7-Zip | `-mx=9` | 48,688,243 | 4.353x | 30.78 s | 6.6 | 1.24 s | 2.09 GB |

Only WinRAR `-m1` is dominated. On twelve large files WinRAR is competitive and its `-m3`
and `-m5` are both on the frontier. The picture on the kernel tree is different because
WinRAR reaches only about 610% of 2400% available CPU on many small files.

Memory here is close between cram `--best` (1.88 GB) and 7-Zip `-mx=9` (2.09 GB). The large
memory gap on the kernel tree does not generalise to every corpus.

## Cross-machine check

Silesia is byte-identical on both machines.

| | Windows, 16T | Linux, 24T |
|---|---|---|
| cram `--best` | 50,195,004 in 15.16 s | 50,195,004 in 9.12 s |
| 7-Zip `-mx=9` | 48,688,236 in 50.41 s | 48,688,243 in 30.78 s |
| WinRAR `-m5` | 53,147,219 in 6.08 s | 53,150,010 in 5.36 s |

Ordering is identical on both machines. cram produced **exactly the same 50,195,004 bytes**
on Windows and Linux from independently compiled binaries. 7-Zip differed by 7 bytes,
WinRAR by 2,791.

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

**`--best` is off the frontier on the kernel tree.** 7-Zip's default `-mx=5` is 4.1% smaller
and 1.18x faster. Comparing cram `--best` against 7-Zip `-mx=9` would flatter cram by
matching its maximum against a setting few people run.

**Verification is slower.** 17.64 s at `--best` against 7-Zip's 1.59 s, an 11x gap. At the
default level it is 3.05 s against 7-Zip `-mx=5`'s 1.06 s, 2.9x. The penalty belongs to XZ
packs: `--fast` and the default write zstd packs, `--best` writes XZ.

**Ratio falls behind on large homogeneous text.** cram groups chunks into solid packs of
8 MiB and compresses each as one stream, so its match window is 8 MiB where `xz -9` carries
64 MiB. enwik9 becomes roughly 119 independent packs. The penalty grows with input size and
homogeneity, which is why Silesia is within 3% of `xz -9` and enwik9 is 11% behind zstd -22.

**Extraction of many small files is slow enough to be a defect.** Extracting the kernel tree
from a `--best` archive sustained **39 files/s, about 1.3 MiB/s**, while occupying 7.7 cores.
After 666 s it had written 24,452 of 94,753 files and the run was aborted. This is not a
tuning gap; it is roughly two orders of magnitude off where it should be, and it is under
investigation. See `docs/PERFORMANCE_FINDINGS.md` §1.

Because that run was aborted, **no complete extract-to-disk timing appears in the tables
above.** The tables report create and `t` (decode plus verify, no output written). Read the
absence of an extract column as an open defect rather than as a measurement nobody got to.

## Limitations

- One machine per operating system. No aggregation across hardware.
- Peak memory was not captured on Windows.
- The Linux machine is a KVM guest. Disk behaviour passes through the hypervisor.
- Windows Defender was active throughout the Windows runs.
- Source and archive shared one physical device on both machines.
- RAR versions differ by one point release between the two machines.
- Extraction to disk has no completed timing. A rate probe was run and aborted; see above
  and `docs/PERFORMANCE_FINDINGS.md` §1. `t` decodes and verifies without writing output.
- The extraction rate probe ran under a cached calibration profile later found to be
  inaccurate. Whether a corrected profile changes it is untested.
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
