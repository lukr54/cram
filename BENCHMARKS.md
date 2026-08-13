# Benchmarks

Measured 5–6 August 2026 against 7-Zip 26.01, RAR 7.12, xz 5.4.5 and zstd 1.5.5 on one Linux
machine. Every archive in every table was extracted and compared byte-for-byte against the
source; **98 of 98 verified, no failures**, and on the Cram corpus every extraction was counted
by file and by byte as well. Commands are given in full so the numbers can be checked.

Nothing here is estimated. Where cram loses, the row is in the same table.

**Four corpora, and they say different things.** Silesia, enwik9 and the kernel tree contain no
duplicate content at all, so they measure cram's compressor with its main structural advantage
switched off. The Cram corpus is 15% duplicate by construction and is the one that measures
deduplication. Read the summary below with that in mind: the size claims move a long way between
them, and which is relevant depends entirely on whether your data repeats itself.

## Summary

**On data that repeats itself, cram wins on both axes at once.** On the Cram corpus, default
against default: **9.4× faster than 7-Zip and 13.4% smaller**, and **12.1× faster than RAR at a
ratio too close to call** (0.7104 against 0.7100 — RAR ahead by 0.06%).

**On data that does not repeat, cram is faster and larger.** On Silesia, enwik9 and the kernel
tree, cram is 6–11× faster than 7-Zip's default and writes a 9–19% larger archive. That is a
different point on the speed/size frontier, not a dominance.

**`--store` is the cleanest demonstration of what `.cram` is for.** With every compressor
switched off on both sides, cram writes **15.3% less than 7-Zip `-mx=0`** and 15.4% less than
RAR `-s -m0`. Nothing is being compressed by anybody; the difference is deduplication, and there
is no flag on either competitor that recovers it.

**cram holds the smallest archive on Silesia**, at 48,012,170 bytes against 7-Zip's tuned
maximum at 48,287,980, and beats `xz -9e` with a 256 MiB dictionary too. **It does not hold it
on the pure-compression corpora**: 7-Zip `-mx=9` is 1.84% smaller on the kernel tree and 5.09%
smaller on enwik9, and on the kernel tree cram `--small` is *strictly dominated* — bigger and
slower than 7-Zip `-mx=9`.

**Extraction is a decoder win and a wash on disk.** With the write wall removed, cram decodes the
Cram corpus in 2.58 s against 7-Zip's 3.64 s and RAR's 7.25 s. Writing to a real disk all three
land between 15 and 19 s, because extraction is write-bound and the disk does not care which
decoder fed it.

**One corpus exposes a real weakness.** enwik9 is a single 1 GB file, and cram extracts it in
10.28 s against 7-Zip's 2.77 s. Parallel extraction of a `.cram` is per-entry, and there is one entry.

**Opening somebody else's `.7z` is now level with 7-Zip on time and well under it on memory**, at
3.26 s against 3.68 s on the Cram corpus, in **867 MB against 7-Zip's 4876 MB**. That is a later
build than the rest of this document and is measured separately below; the number depends on the
archive having been written by a multi-threaded encoder, which is a property of the file rather than
of the format.

**Memory is where cram is unambiguously cheaper.** Default against default on the Cram corpus,
2.4 GB against 7-Zip's 7.1 GB. At maximum, 7-Zip needs **17.4 GB** and does not fit on a 16 GB
machine; given all 24 threads it exceeds 20 GB and is killed.

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
- **Extraction timings include `sync`.** None of these tools calls `fsync`, so a timer stopped at
  process exit stops with gigabytes still in the page cache, and the kernel flushes them afterwards
  on whatever runs next. That is what every published archiver comparison measures, and the claim
  made here previously — that it flatters all four tools equally — is false. It does not distribute
  evenly; it lands on whichever command runs next. In one run of the mixed corpus it had 7-Zip
  "extracting" an archive in 10.94 s that it needed 52.74 s to *verify*, which is impossible for the
  same bytes: verification decodes the same data and never writes the 5.4 GB. The extraction's
  writeback had been billed to the verify. Putting `sync` inside the timed region charges each tool
  for its own writes and the anomaly disappears.
- **Every extraction is counted, not trusted.** File count and total bytes are compared against the
  corpus afterwards. A fast extraction that dropped half the files is not fast, and an exit code
  does not catch it.
- Create timings still stop at process exit, which is the conventional measurement and is stated
  here rather than hidden. Creation writes far less than it reads, so the effect is much smaller
  than on the read side.

**All four tools archive exactly the same bytes.** The kernel tree ships 99 symbolic links,
twelve of them pointing at directories. cram skips symlinks and reports each one; 7-Zip and RAR
dereference them, which on this tree silently duplicates 8,011 files. Rather than caveat that
asymmetry, the symlinks were removed from the corpus, so every tool sees an identical file set.

`-s` on RAR is not optional for a fair comparison: 7-Zip is solid by default and RAR is not.

## Corpora

| corpus | files | bytes | duplicate | source |
|---|---|---|---|---|
| Silesia | 12 | 211,938,580 | none | `silesia.zip`, sha256 `0626e25f45c0ffb5dc801f13b7c82a3b75743ba07e3a71835a41e3d9f63c77af` |
| enwik9 | 1 | 1,000,000,000 | none | mattmahoney.net/dc/enwik9.zip |
| Linux kernel tree | 94,778 | 1,920,837,858 | none | `.git` and symlinks excluded |
| **Cram corpus 1.0** | 42,151 | 2,800,604,582 | **15.0%** | [`tools/corpus`](tools/corpus), id `deb5f932d27a913ad6da2b994be7e66bffd03d6bf8546abd3de8ca7344efe599` |

Silesia and enwik9 are the corpora the compression field publishes against, so anyone can
re-run these. The kernel tree stands in for what people actually archive: many small files,
mixed text and binary.

**The Cram corpus is the one that measures deduplication**, which the other three cannot: none of
them repeats itself, so on all three `.cram` is a plain compressor with its main structural
advantage switched off. It is built by a script in this repository from the Linux kernel, Big Buck
Bunny and 202 Wikimedia Commons photographs — all redistributable — and every download is
checksum-pinned, so two people building it get byte-identical trees. `CORPUS.id` above is a digest
over every file in it; if yours matches, you have the same corpus.

Its duplicate content is **15.0%**, sitting in one top-level directory called `dup/`. That figure is
an assumption about what a working drive looks like, and it is the assumption every dedup number
below depends on, so it is deliberately deletable: `rm -rf dup/` and re-run to see the corpus
without it. If you think 15% is generous, measure both and say so.

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

## Cram corpus 1.0, 2,800,604,582 bytes, 42,151 files

Medians of 3, tool order rotated per round, corpus re-read into page cache before every create.
15.0% of this corpus is duplicate content; see [Corpora](#corpora) for what that means and how to
delete it.

### Default settings

| tool | mode | create | archive | ratio | peak RSS |
|---|---|---|---|---|---|
| **cram** | `--auto` | **6.93 s** | 1,989,536,373 | 0.7104 | 2,463 MB |
| 7-Zip | `-mx=5` | 65.46 s | 2,297,090,458 | 0.8202 | 7,079 MB |
| RAR | `-s -m3` | 84.09 s | 1,988,397,501 | 0.7100 | 325 MB |

**9.4× faster than 7-Zip and 13.4% smaller.** Against RAR, **12.1× faster at the same ratio** —
RAR is 1,138,872 bytes smaller out of 1.99 GB, which is 0.06% and is a tie, not a win for either.

Spreads were tight: cram 6.87–6.95, 7-Zip 65.22–65.48, RAR 83.97–84.20.

### As small as each one goes

| tool | mode | create | archive | ratio | peak RSS |
|---|---|---|---|---|---|
| **cram** | `--small` | 354.79 s | **1,660,841,040** | **0.5930** | 6,766 MB |
| 7-Zip | `-mx=9`, own threading | 87.04 s | 2,293,950,670 | 0.8191 | **17,401 MB** |
| 7-Zip | `-mx=9 -mmt=24` | *killed at 20 GB* | — | — | >20,480 MB |
| RAR | `-s -m5` | 86.15 s | 1,986,335,604 | 0.7093 | 595 MB |

cram `--small` is **27.6% smaller than 7-Zip at its maximum** and 16.4% smaller than RAR at its
maximum. It costs 4.1× 7-Zip's time.

Two things in that table need saying plainly. **7-Zip's maximum is barely smaller than its
default here** — 2.294 GB against 2.297 GB, 0.14%, for 33% more time and 2.5× the memory; on a
corpus that is 51% incompressible media there is very little left for a bigger window to find.
And **`-mx=9` with every thread exceeds 20 GB and is killed**. That is a real result under this
document's stated method, which gives every tool all 24 threads — but it is not a fair
characterisation of 7-Zip on its own terms, because its own heuristic throttles to fit, so both
rows are given.

### With every compressor switched off

| tool | mode | create | archive | ratio |
|---|---|---|---|---|
| **cram** | `--store` | 2.55 s | **2,372,984,126** | **0.8473** |
| 7-Zip | `-mx=0` | 3.93 s | 2,801,024,600 | 1.0001 |
| RAR | `-s -m0` | 3.18 s | 2,805,233,456 | 1.0017 |

**15.3% smaller than 7-Zip, 15.4% smaller than RAR**, with nothing compressed on any side. The
whole difference is deduplication. It tracks the corpus's 15.0% duplicate content almost exactly,
which is the point: on a corpus that does not repeat itself this column would read 1.0000 like the
others.

### `--fast`

| tool | mode | create | archive | ratio | peak RSS |
|---|---|---|---|---|---|
| **cram** | `--fast` | **2.81 s** | 2,013,900,705 | 0.7191 | 184 MB |
| 7-Zip | `-mx=5` | 65.46 s | 2,297,090,458 | 0.8202 | 7,079 MB |

**23.3× faster than 7-Zip's default and still 12.3% smaller**, in 184 MB of RAM against 7.1 GB.

### Extraction

| tool | to disk, `sync` included | to tmpfs |
|---|---|---|
| **cram** | 15.36 s | **2.58 s** |
| 7-Zip | 16.00 s | 3.64 s |
| RAR | 18.85 s | 7.25 s |

All nine disk extractions and all nine tmpfs extractions produced 42,151 files and
2,800,604,582 bytes; nothing was short.

The tmpfs column is the decoder with the write wall removed, and it repeats to within 3%: cram
2.52–2.59, 7-Zip 3.63–3.65, RAR 7.24–7.43. **cram decodes 1.4× faster than 7-Zip and 2.8× faster
than RAR.**

The disk column does not repeat well — cram 8.69–18.54, 7-Zip 15.47–19.88, RAR 15.70–18.86 — and
those spreads overlap completely. **On disk these three are indistinguishable**, and anyone
quoting a winner from that column is quoting noise. Extraction is write-bound: the same work takes
2.58 s when the disk is not in the way and 15 s when it is.

## Reading archives other tools wrote

Everything above measures each tool on its *own* format. This measures the other thing people
actually do: open a `.7z` somebody else made.

**Measured 2026-08-13, and not part of the run above.** Same machine, but two later builds than the
1.0.0 used everywhere else in this document: released 1.1.0 as the baseline, and an unreleased build
for the segment work described below. The corpus is on a different volume of the same box. Do not compare these timings against
the tables above; compare them against the 7-Zip column beside them, which was re-run at the same
time. Three rounds with the tool order rotated, `/dev/shm` as the destination, every extraction
counted by file and byte and checked against the corpus `MANIFEST.sha256`: 42,151 files each, all
matching.

Two archives of the Cram corpus, because how an archive was *written* decides what can be done with
it:

```
cram a corpus.7z .                     2,339,551,070 bytes, 49 solid blocks
7zz a corpus.7z . -mmt=24 -mx=5        2,297,090,458 bytes, ONE solid block
```

| extracting | cram 1.1.0 | cram, this build | 7-Zip 26.01 |
|---|---|---|---|
| the 7-Zip-written archive | 25.01 s, 609 MB | **3.26 s** [3.13–3.26], **867 MB** | 3.68 s [3.67–3.71], 4876 MB |
| the cram-written archive | 8.62 s, 138 MB | **1.35 s** [1.32–1.38], 303 MB | 3.25 s [3.24–3.27], 173 MB |

**On 7-Zip's own archive the two are level on time** — 3.26 against 3.68 is close enough that
reading a winner into it is reading noise. What is not noise is the memory: 7-Zip needs 4876 MB to
reach that time and cram needs 867 MB. On a cram-written `.7z` cram is 2.4× faster, at 303 MB
against 173 MB.

**Most of that memory gap was closed in one line.** Until the declared LZMA2 dictionary size became
readable, the window had to be bounded by the segment's own length — always safe, since a segment
opens on a dictionary reset, and about four times too large, since 7-Zip writes 32 MiB dictionaries
into 128 MiB thread blocks. Asking the archive instead took peak RSS from 2809 MB to 867 MB on this
run, and 15% off the CPU with it, from the allocation that stopped happening. The declared value
comes from the archive, so it is used only to shrink the window and never to grow it.

**Why a single-folder archive is divisible at all.** 7-Zip's `-mx=5` default puts the whole 2.8 GB
in one solid block, which cram used to decode on one thread. But its multi-threaded encoder resets
the LZMA2 dictionary at each thread-block boundary, and a chunk with a dictionary reset can be
decoded cold. Walking the framing of that archive: 47,011 chunks, **21 dictionary resets**, segments
of 110.9–128.0 MiB. Twenty-one places a decoder can start.

**This is a property of the archive, not of the format, and it is worth stating plainly rather than
generalising.** The resets exist because a multi-threaded encoder put them there. A `.7z` written
with `-mmt=1`, or one smaller than a single thread-block, has exactly one segment and gains nothing
— it decodes at the 1.0.0 speed. No survey has been done of how common each case is in the wild, so
no claim is made about "most `.7z` files".

Extraction to a real disk is not reported here. On this machine it is write-bound and the four
columns do not separate: repeated runs of the same command ranged 9.85–26.68 CPU-seconds and
16.33–30.32 s wall, which supports no claim in either direction.

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
- **`--small` is a bad trade on a media-heavy corpus.** On the Cram corpus it is 16.5% smaller
  than `--auto` for **51× the time**, and it holds 6.8 GB while doing it. Most of that corpus is
  already-compressed photographs and video, where a wider window and a longer match search have
  almost nothing to find. Reach for it on text and source, not on a photo library.

## What these corpora do not measure

**Nothing here measures a corpus larger than memory.** Every table above fits in this machine's
23 GiB page cache, so no result is bounded by re-reading source data from disk. A 200 GB backup
is a different measurement and it has not been done.

**Nothing here measures a spinning disk**, and extraction is write-bound: the Cram corpus decodes
in 2.58 s and takes 15 s to write to NVMe. On a 120 MB/s HDD every tool in this document would be
pinned to the disk and the decoder column would stop mattering at all.

**Whether extraction is write-bound at all is a property of your drive, not of any archiver here.**
Measured 2026-08-07 with `calibrate --recalibrate --write-probe` on two machines: the benchmark
box decodes DEFLATE at 948 MiB/s on one core against an 84 MiB/s sustained write wall, a ratio of
11.3, so a single worker already outruns the disk by an order of magnitude. A desktop NVMe in the
same room decodes at 674 MiB/s against a 757 MiB/s wall, a ratio of 0.89, where one decoding thread
does not saturate the drive and a second has real work to do. The same tool is write-bound on one
and roughly balanced on the other, which is why the engine measures rather than assuming. Run that
command to find out which regime you are in; do not assume this table's answer is yours.

**Both drives stepped down at ~2 GiB written**, from 349 to 84 MiB/s and from 1218 to 757 MiB/s, as
the SLC cache filled. Any extraction benchmark whose output fits under that is measuring cache
rather than disk. The extraction runs above go to tmpfs and avoid the question entirely; the create
runs write ~2 GB and land on the knee.

**Nothing here measures Windows**, which is the platform Cram is built for first. These are Linux
numbers on one machine, and the Windows file-open path is measurably different — see
[`docs/PERFORMANCE_FINDINGS.md`](docs/PERFORMANCE_FINDINGS.md) §7, where `File::open` dominates
create on Windows and is nearly free on Linux.

**The dedup figures depend entirely on one assumption**, which is that 15% of a working drive
repeats itself. That number is stated, is confined to a deletable directory, and is the single
input every deduplication claim here rests on. On the three corpora that repeat nothing, cram's
size advantage disappears completely — which is exactly what the Silesia, enwik9 and kernel tables
show.

## Determinism

An unencrypted `.cram` is byte-for-byte reproducible: the same inputs give the same archive on
any machine, at any thread count, with any amount of RAM. Pinned by
`crates/cram-core/tests/reproducible.rs`, `batch_invariance.rs` and `chunk_lanes.rs`, the last
of which builds the same tree at 1, 2, 4 and 16 chunk workers and compares the bytes.

Held on real data too: every Cram corpus figure above was produced at a lane count the machine
chose for itself, and a sweep from 1 to 24 chunk lanes over three effort levels produced the same
archive bytes every time — 1,989,536,373 at `--auto`, 2,372,984,126 at `--store` — across 24 runs
per level and across the commit that introduced the lane pool.

## Reproducing

The Cram corpus is [a 2.22 GiB download](https://drive.proton.me/urls/FYRM6FM454#zf8BLhcKK4ew),
sha256 `5be1b545ec9535834904a6436e6abf27a0fd607190851e314624e8a2db53faa7`.

It also builds itself from public sources, checked against pinned digests so a different upstream
file stops the build rather than silently changing the corpus:

```sh
python3 tools/corpus/make-corpus.py --out ./cram-corpus-1.0
cat cram-corpus-1.0/CORPUS.id
# deb5f932d27a913ad6da2b994be7e66bffd03d6bf8546abd3de8ca7344efe599
```

Both routes give the same corpus. `CORPUS.id` is a digest over `MANIFEST.sha256`, which lists every
file, so downloading and building are equally checkable and neither requires trusting the other.

Then the whole table, with the method above already encoded in it:

```sh
tools/corpus/bench-corpus.sh ./cram-corpus-1.0 /tmp/bench 3
```

The other three corpora, and one configuration of each tool as run:

```sh
curl -LO https://sun.aei.polsl.pl//~sdeor/corpus/silesia.zip
curl -LO https://mattmahoney.net/dc/enwik9.zip

cram a OUT.cram <inputs> --auto -y
7zz  a -mmt=24 -mx=9 OUT.7z <inputs>
rar  a -mt24 -m5 -s -r -y OUT.rar <inputs>
tar -cf - <inputs> | xz -9e -T0 > OUT.tar.xz
tar -cf - <inputs> | zstd -19 --long=27 -T0 -o OUT.tar.zst
```

Peak RSS comes from `/usr/bin/time -f '%M'` around each command.
