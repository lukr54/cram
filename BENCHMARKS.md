# Benchmarks

Measured 5 August 2026 against 7-Zip 26.01, RAR 7.12, xz 5.4.5 and zstd 1.5.5 on one Linux
machine. Every archive in every table was extracted and compared byte-for-byte against the
source; **98 of 98 verified, no failures**. Commands are given in full so the numbers can be
checked.

Nothing here is estimated. Where cram loses, the row is in the same table.

## Summary

**At the settings people actually use, cram is 6–11× faster than 7-Zip's default**, and writes
a 9–19% larger archive. That is a different point on the speed/size frontier, not a dominance.

**cram holds the smallest archive on Silesia.** `--small` writes 48,012,170 bytes against
7-Zip's tuned maximum at 48,287,980, and beats `xz -9e` with a 256 MiB dictionary too.

**cram does not hold it anywhere else.** 7-Zip `-mx=9` is 1.84% smaller on the kernel tree and
5.09% smaller on enwik9. On the kernel tree cram `--small` is *strictly dominated*: bigger and
slower than 7-Zip `-mx=9`.

**Against RAR on a realistic tree, cram wins outright**: default against default, 5.2× faster
and 15% smaller.

**Extraction of many small files is cram's largest margin**: 2.21 s against 7-Zip's 5.96 s and
RAR's 11.14 s on 94,778 files.

**One corpus exposes a real weakness.** enwik9 is a single 1 GB file, and cram extracts it in
10.28 s against 7-Zip's 2.77 s. Parallel extraction is per-entry, and there is one entry.

## Machine

| | |
|---|---|
| CPU | AMD Ryzen 9 5900X, 24 threads |
| RAM | 23 GiB |
| OS | Ubuntu 24.04.4, kernel 6.8 |
| Storage | dedicated ext4 volume, 124 GB free |

cram 1.0.0 built with the shipping feature set (`download,zstd-c,phash`). A default-feature
build writes XZ where the shipped one writes zstd and is not the tool users get.

## Method

- Silesia is the median of 3 runs; enwik9 and the kernel tree the median of 2. Each repetition
  rotates the tool order, so no tool is permanently first or last.
- The corpus is read into the page cache immediately before every timed run, so no tool pays to
  warm the disk for the next one.
- **Every tool is given all 24 threads explicitly** (`-mmt=24`, `-mt24`, `-T0`) rather than left
  to its default.
- **Every tool is run at both its own default and its documented maximum.** Comparing one tool's
  maximum against another's default is the standard way these comparisons mislead.
- Archives are deleted between runs; 7-Zip and RAR append to an existing archive otherwise.
- Peak RSS via `/usr/bin/time -f '%M'`.
- Every archive is extracted and `diff -rq`'d against the source. A ratio from an archive nobody
  opened is a rumour.
- Timings stop when the process exits. None of these tools calls `fsync`, so on a 23 GiB machine
  part of the output can still be in the page cache at that moment. This is what every published
  archiver comparison measures, and it flatters all four tools equally.

**All four tools archive exactly the same bytes.** The kernel tree ships 99 symbolic links,
twelve of them pointing at directories. cram skips symlinks and reports each one; 7-Zip and RAR
dereference them, which on this tree silently duplicates 8,011 files. Rather than caveat that
asymmetry, the symlinks were removed from the corpus, so every tool sees an identical file set.

`-s` on RAR is not optional for a fair comparison: 7-Zip is solid by default and RAR is not.

## Corpora

| corpus | files | bytes | source |
|---|---|---|---|
| Silesia | 12 | 211,938,580 | `silesia.zip`, sha256 `0626e25f45c0ffb5dc801f13b7c82a3b75743ba07e3a71835a41e3d9f63c77af` |
| enwik9 | 1 | 1,000,000,000 | mattmahoney.net/dc/enwik9.zip |
| Linux kernel tree | 94,778 | 1,920,837,858 | `.git` and symlinks excluded |

Silesia and enwik9 are the corpora the compression field publishes against, so anyone can
re-run these. The kernel tree stands in for what people actually archive: many small files,
mixed text and binary.

## Silesia, 211,938,580 bytes

| tool | setting | create | bytes | ratio | peak RSS | extract |
|---|---|---:|---:|---:|---:|---:|
| **cram** | **`--small`** | 74.56 s | **48,012,170** | **0.2265** | 2227 MB | 0.86 s |
| 7-Zip | `-mx=9` tuned | 40.07 s | 48,287,980 | 0.2278 | 2088 MB | 1.38 s |
| xz | `-9e` | 76.51 s | 48,624,588 | 0.2294 | 1071 MB | 2.04 s |
| 7-Zip | `-mx=9` | 34.21 s | 48,688,243 | 0.2297 | 2088 MB | 1.37 s |
| xz | `-6` (default) | 11.23 s | 49,586,256 | 0.2340 | 1086 MB | 2.02 s |
| 7-Zip | `-mx=5` (default) | 17.25 s | 49,597,414 | 0.2340 | 907 MB | 0.92 s |
| zstd | `-19 --long` | 29.52 s | 52,778,162 | 0.2490 | 589 MB | 0.25 s |
| RAR | `-m5 -s` | 5.25 s | 53,120,775 | 0.2506 | 580 MB | 0.57 s |
| RAR | `-m3` (default) | 3.14 s | 54,218,452 | 0.2558 | 310 MB | 0.59 s |
| **cram** | **`--auto`** (default) | **1.50 s** | 58,280,168 | 0.2750 | 764 MB | 0.28 s |
| zstd | `-3` (default) | 0.21 s | 66,625,332 | 0.3144 | 249 MB | 0.20 s |
| cram | `--fast` | 0.20 s | 69,474,237 | 0.3278 | 133 MB | 0.28 s |

cram holds both ends here: the smallest archive of any tool, and the two fastest points.
7-Zip owns the middle.

## enwik9, 1,000,000,000 bytes

| tool | setting | create | bytes | ratio | peak RSS | extract |
|---|---|---:|---:|---:|---:|---:|
| **7-Zip** | **`-mx=9` tuned** | 155.78 s | **208,851,242** | **0.2089** | 9833 MB | 2.77 s |
| 7-Zip | `-mx=9` | 155.47 s | 210,604,362 | 0.2106 | 9835 MB | 2.86 s |
| xz | `-9e` | 249.50 s | 214,153,080 | 0.2142 | 3666 MB | 8.15 s |
| cram | `--small` | 179.24 s | 219,486,008 | 0.2195 | 8210 MB | 10.28 s |
| RAR | `-m5 -s -md512m` | 43.40 s | 219,984,649 | 0.2200 | 3910 MB | 3.35 s |
| 7-Zip | `-mx=5` (default) | 67.38 s | 224,618,387 | 0.2246 | 3775 MB | 1.88 s |
| zstd | `-19 --long` | 66.34 s | 230,822,761 | 0.2308 | 2402 MB | 1.21 s |
| xz | `-6` (default) | 39.08 s | 233,402,304 | 0.2334 | 3032 MB | 7.94 s |
| RAR | `-m5 -s` | 32.08 s | 237,654,873 | 0.2377 | 564 MB | 2.88 s |
| RAR | `-m3` (default) | 22.23 s | 249,222,506 | 0.2492 | 294 MB | 2.84 s |
| **cram** | **`--auto`** (default) | **11.32 s** | 267,632,141 | 0.2676 | 1937 MB | 3.91 s |
| cram | `--fast` | 0.85 s | 328,890,853 | 0.3289 | 101 MB | 3.81 s |

**This is cram's worst corpus and the reason is structural.** A `.cram` compresses each pack
independently, so its match window is one pack — 64 MiB at `--small` — against LZMA's whole-file
solid block. On one 1 GB file that costs 5.09%. RAR at `-md512m` reaches cram's `--small` ratio
in a quarter of the time.

## Linux kernel tree, 1,920,837,858 bytes, 94,778 files

| tool | setting | create | bytes | ratio | peak RSS | extract |
|---|---|---:|---:|---:|---:|---:|
| **7-Zip** | **`-mx=9`** | 97.59 s | **442,486,736** | **0.2304** | 18905 MB | 6.37 s |
| xz | `-9e` | 241.03 s | 445,092,440 | 0.2317 | 3842 MB | 9.02 s |
| 7-Zip | `-mx=9` tuned | 102.84 s | 448,355,868 | 0.2334 | 18912 MB | 5.88 s |
| cram | `--small` | 184.95 s | 450,635,350 | 0.2346 | 7550 MB | 5.35 s |
| 7-Zip | `-mx=5` (default) | 49.62 s | 452,190,211 | 0.2354 | 5963 MB | 5.96 s |
| zstd | `-19 --long` | 73.29 s | 454,490,514 | 0.2366 | 4104 MB | 3.93 s |
| xz | `-6` (default) | 35.53 s | 458,409,724 | 0.2387 | 3185 MB | 9.19 s |
| RAR | `-m5 -s -md512m` | 87.20 s | 474,538,515 | 0.2470 | 3962 MB | 9.64 s |
| RAR | `-m5 -s` | 74.25 s | 482,801,949 | 0.2513 | 614 MB | 9.32 s |
| **cram** | **`--auto`** (default) | **8.36 s** | 493,816,077 | 0.2571 | 1867 MB | **2.21 s** |
| zstd | `-3` (default) | 1.90 s | 540,088,970 | 0.2812 | 283 MB | 4.07 s |
| cram | `--fast` | 2.19 s | 557,998,873 | 0.2905 | 177 MB | 3.81 s |
| RAR | `-m3` (default) | 43.47 s | 581,071,715 | 0.3025 | 345 MB | 11.14 s |

Default against default, cram is **5.9× faster than 7-Zip** and **5.2× faster than RAR while
also being 15% smaller**. It extracts in 2.21 s where 7-Zip takes 5.96 s and RAR 11.14 s.

7-Zip `-mx=9` reaches a ratio cram does not, and does it in half `--small`'s time. It pays
**18.9 GB of RAM** to get there, against cram's 7.5 GB.

## Where cram loses

- **Maximum ratio on large corpora.** 7-Zip `-mx=9` is 1.84% smaller on the kernel tree and
  5.09% smaller on enwik9. On the kernel tree cram `--small` is dominated outright: bigger and
  slower. Use `--auto` there and take the speed.
- **Single-file archives, on extraction.** enwik9 extracts in 10.28 s against 7-Zip's 2.77 s.
  The parallel extract path is per-entry, so one entry means one thread.
- **Small archives, on creation.** cram does not parallelise *within* a pack, so a corpus
  smaller than a few packs compresses single-threaded while 7-Zip splits it across every core.
  On a 20 MB input, cram at maximum took 15.89 s against 7-Zip `-mx=9`'s 4.55 s.
- **Memory at maximum**, on the small corpora: 2227 MB on Silesia against 7-Zip's 2088 MB. On
  the kernel tree the position reverses sharply, 7.5 GB against 18.9 GB.

## What these corpora do not measure

**None of the three contains duplicate content**, and cross-file deduplication is what `.cram`
is built around. These are pure compression tests, so they measure cram's compressor with its
main structural advantage switched off. On a corpus that does repeat itself — a set of game
repacks, versioned build outputs, successive backups — 7-Zip and RAR cannot collapse a
duplicate at any setting, and cram stores it once by construction.

That is not an excuse for the tables above. It is a statement about what is missing from them,
and the measurement has not been done yet.

## Determinism

An unencrypted `.cram` is byte-for-byte reproducible: the same inputs give the same archive on
any machine, at any thread count, with any amount of RAM. Pinned by
`crates/cram-core/tests/reproducible.rs`, `batch_invariance.rs` and `chunk_lanes.rs`, the last
of which builds the same tree at 1, 2, 4 and 16 chunk workers and compares the bytes.

## Reproducing

```sh
# corpora
curl -LO https://sun.aei.polsl.pl//~sdeor/corpus/silesia.zip
curl -LO https://mattmahoney.net/dc/enwik9.zip

# one configuration, as run
cram a OUT.cram <inputs> --auto -y
7zz  a -mmt=24 -mx=9 OUT.7z <inputs>
rar  a -mt24 -m5 -s -r -y OUT.rar <inputs>
tar -cf - <inputs> | xz -9e -T0 > OUT.tar.xz
tar -cf - <inputs> | zstd -19 --long=27 -T0 -o OUT.tar.zst
```

Peak RSS comes from `/usr/bin/time -f '%M'` around each command.
