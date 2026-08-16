# Benchmarks

Measured 5–6 August 2026 against 7-Zip 26.01, RAR 7.12, xz 5.4.5 and zstd 1.5.5 on one Linux
machine. Every archive in every table was extracted and compared byte-for-byte against the source;
**98 of 98 verified, no failures**, and on the Cram corpus every extraction was counted by file and
by byte as well. Commands are given in full so the numbers can be checked.

Nothing here is estimated. Where cram loses, the row is in the same table.

**Re-run on 14 August against a build thirty commits newer.** The create figures reproduced to the
byte — every archive size, across four corpora and three effort levels. The extraction figures did
not reproduce at all, and [Decode](#decode) replaces them with a measurement that does: five tools,
four corpora, **80 of 80 extractions verified**, and 17 of its 20 cells repeating to within 8%
across three rounds (worst case 22%).

> **The `extract` column was removed from the four tables below on 14 August, and replaced by
> [Decode](#decode).** Its times implied write rates the destination cannot reach: extracting the
> 1,920,837,858-byte kernel tree in the 2.21 s it published needs 829 MiB/s, on a volume measuring
> **84 MiB/s sustained**. It affected every tool in the column equally, not cram alone.
>
> Two causes, both in the method below and both now fixed: `sync` flushes the *whole system* rather
> than what the tool just wrote, and the method never said where extractions were written.
>
> The replacement measures to a **RAM disk** and calls itself decode, because that is what it is.
> The disk result is kept in one sentence there, since it is the more useful fact for anyone
> choosing on extraction speed: on a real disk the tools converge to within 23% and it barely
> matters which you pick.
>
> The **create** column re-measured exactly — every archive size matches to the byte across four
> corpora and three levels, on a build thirty commits newer. That column stands unchanged.

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

**Decoding, cram beats every tool in its class and loses to zstd.** With the write wall removed it
is **1.4–4.1× faster than 7-Zip** and 2.1–4.1× faster than RAR across four corpora, in a third to a
quarter of 7-Zip's memory. zstd `-3` decodes faster on three of the four, which is what a much
weaker compressor buys — its archives are the largest in every table here. Full numbers, including
the three cram loses, in [Decode](#decode).

**On a real disk none of that shows.** Extraction is write-bound: the same four tools land within
23% of each other, and the choice barely matters if you are writing to a spinning or SATA disk.

**Writing a `.tar.gz` is faster than `pigz`, by 1.08×, in a 1.38% smaller archive.** pigz is the
right comparison and `tar czf` is not: `tar czf` pipes through one thread, so beating it says nothing
except that we use the machine. Numbers, and what the chunking costs, in
[Writing `.tar.gz`](#writing-targz).

**Reading one is now 1.25× faster than `gzip -dc | tar`** — 4.94 s against 6.17 on the kernel tree,
both on a single decode thread, because a standard `.gz` cannot be parallelised by anybody. Until
2026-08-15 this was 2.26× *slower*, and every cause was ours: a megabyte allocated and zeroed for
every one of 94,778 entries, a decoder that could never get more than one message ahead of its
consumer, an extraction path issuing 1.83 million syscalls where GNU tar issues 0.79 million, and
files written on one thread while another decoded. **All six tar codecs improved, `.tar.xz` overtook
`xz -dc | tar`, a plain `.tar` overtook GNU tar, and the four still behind are within 1.05–1.83×
rather than 2.2–5.6×**; see [Where cram loses](#where-cram-loses).

**One corpus exposed a real weakness, and it is mostly closed.** enwik9 is a single 1 GB file, and
extraction fanned out per entry — one entry, one thread, whatever the machine. Cutting the entry at
its pack boundaries took it from **9.05 s at 1.0 effective cores to 2.06 s at 5.4**, against 7-Zip's
1.64 s at 4.4, in 900 MB against 7-Zip's 1176 MB. Measured to tmpfs on 14 August, output compared
byte-for-byte against the original file; a later build than the tables below.

**Opening somebody else's `.7z` is now level with 7-Zip on time and well under it on memory**, at
3.26 s against 3.68 s on the Cram corpus, in **867 MB against 7-Zip's 4876 MB**. Measured to tmpfs
on a later build than the tables below, and stated with its conditions because they are load-bearing:

- It applies to a `.7z` holding **more than 128 MiB, written at stock settings**. That is where a
  multi-threaded encoder leaves dictionary resets a decoder can start from. The rule is exact —
  block size is four times the dictionary — and derived from measurement rather than assumed.
- Below that threshold, or with `-mmt=1`, there are no seams. cram gives time back and is **47%
  slower**, in a third of the memory.
- At `-mx=9` the segments are 256 MiB and cram is **26% slower**, in 1.75× less memory.

Every one of those is measured in that section, including the ones where cram loses.

**Memory is where cram is cheaper than 7-Zip**, and the qualifier is doing real work — it means
7-Zip and it does not generalise. Against the threaded tar tools cram is *dearer*, and writing a
`.tar.xz` it now peaks at 3960 MB against `xz -T0`'s 3196. Against the pipe-based readers it is
dearer by one to two orders of magnitude — extracting a `.tar.gz` costs 113 MB against `gzip -dc |
tar`'s 3.5 — because a pipe holds nothing and a pipeline holds its buffers. That is the price of the
parallel paths and it is not going away.

Extraction also holds a bounded batch of decoded entries for its writer pool, which is most of why a
plain `.tar` now peaks at 219 MB against 133 before. Where a `.tar.bz2` or `.tar.xz` decodes on a
pool as well the price is explicit and bounded: 594 MB and 439 MB against 94 and 118 sequential, held to a 256 MiB budget for the decoded bytes in flight plus
the decoders themselves. `CRAM_PARALLEL_DECODE=0` gives the memory back and takes the speed with it.
For comparison `lbzip2` peaks at 546 MB on the same archive, so on `bz2` this is not the expensive
option.
Creating the Cram corpus, default against default, is 2.4 GB against 7-Zip's 7.1 GB; at maximum
7-Zip needs **17.4 GB**, does not fit on a 16 GB machine, and given all 24 threads exceeds 20 GB and
is killed. Decoding that corpus is 1310 MB against 4877 MB. Against RAR and the pipe-based tools it
is the other way round: zstd and xz decode in 6–10 MB because a tar pipe holds nothing, and RAR
creates in 325 MB. The claim is about 7-Zip.

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
- **Decode timings are taken to a RAM disk** (`/dev/shm`), stated as the destination, with the
  destination emptied before every timed run. That last part matters for one tool only: tmpfs pages
  are RAM and cram's planner reads available RAM to size its worker count, so leaving the previous
  extraction in place would change cram's plan and no competitor's.

  This replaced an earlier approach on 14 August. **The `sync` described below is not enough**, and
  the paragraph is kept because the reasoning in it is right and the conclusion was wrong. `sync`
  flushes the whole system rather than the writes the tool just made, so on a machine doing anything
  else each run is charged for unrelated writeback — and the resulting figures still implied write
  rates the drive cannot reach. Per-file `fsync` fixes the accounting; it does not fix that this
  drive cannot measure anything under 2 GiB repeatably, which is why the RAM disk is used instead.

- **Extraction timings include `sync`** *(superseded, see above)*. None of these tools calls `fsync`,
  so a timer stopped at
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

| tool | setting | create | bytes | ratio | peak RSS |
|---|---|---:|---:|---:|---:|
| **cram** | **`--small`** | 74.56 s | **48,012,170** | **0.2265** | 2227 MB |
| 7-Zip | `-mx=9` tuned | 40.07 s | 48,287,980 | 0.2278 | 2088 MB |
| xz | `-9e` | 76.51 s | 48,624,588 | 0.2294 | 1071 MB |
| 7-Zip | `-mx=9` | 34.21 s | 48,688,243 | 0.2297 | 2088 MB |
| xz | `-6` (default) | 11.23 s | 49,586,256 | 0.2340 | 1086 MB |
| 7-Zip | `-mx=5` (default) | 17.25 s | 49,597,414 | 0.2340 | 907 MB |
| zstd | `-19 --long` | 29.52 s | 52,778,162 | 0.2490 | 589 MB |
| RAR | `-m5 -s` | 5.25 s | 53,120,775 | 0.2506 | 580 MB |
| RAR | `-m3` (default) | 3.14 s | 54,218,452 | 0.2558 | 310 MB |
| **cram** | **`--auto`** (default) | **1.50 s** | 58,280,168 | 0.2750 | 764 MB |
| zstd | `-3` (default) | 0.21 s | 66,625,332 | 0.3144 | 249 MB |
| cram | `--fast` | 0.20 s | 69,474,237 | 0.3278 | 133 MB |

cram holds both ends here: the smallest archive of any tool, and the two fastest points.
7-Zip owns the middle.

## enwik9, 1,000,000,000 bytes

| tool | setting | create | bytes | ratio | peak RSS |
|---|---|---:|---:|---:|---:|
| **7-Zip** | **`-mx=9` tuned** | 155.78 s | **208,851,242** | **0.2089** | 9833 MB |
| 7-Zip | `-mx=9` | 155.47 s | 210,604,362 | 0.2106 | 9835 MB |
| xz | `-9e` | 249.50 s | 214,153,080 | 0.2142 | 3666 MB |
| cram | `--small` | 179.24 s | 219,486,008 | 0.2195 | 8210 MB |
| RAR | `-m5 -s -md512m` | 43.40 s | 219,984,649 | 0.2200 | 3910 MB |
| 7-Zip | `-mx=5` (default) | 67.38 s | 224,618,387 | 0.2246 | 3775 MB |
| zstd | `-19 --long` | 66.34 s | 230,822,761 | 0.2308 | 2402 MB |
| xz | `-6` (default) | 39.08 s | 233,402,304 | 0.2334 | 3032 MB |
| RAR | `-m5 -s` | 32.08 s | 237,654,873 | 0.2377 | 564 MB |
| RAR | `-m3` (default) | 22.23 s | 249,222,506 | 0.2492 | 294 MB |
| **cram** | **`--auto`** (default) | **11.32 s** | 267,632,141 | 0.2676 | 1937 MB |
| cram | `--fast` | 0.85 s | 328,890,853 | 0.3289 | 101 MB |

**This is cram's worst corpus and the reason is structural.** A `.cram` compresses each pack
independently, so its match window is one pack — 64 MiB at `--small` — against LZMA's whole-file
solid block. On one 1 GB file that costs 5.09%. RAR at `-md512m` reaches cram's `--small` ratio
in a quarter of the time.

## Linux kernel tree, 1,920,837,858 bytes, 94,778 files

| tool | setting | create | bytes | ratio | peak RSS |
|---|---|---:|---:|---:|---:|
| **7-Zip** | **`-mx=9`** | 97.59 s | **442,486,736** | **0.2304** | 18905 MB |
| xz | `-9e` | 241.03 s | 445,092,440 | 0.2317 | 3842 MB |
| 7-Zip | `-mx=9` tuned | 102.84 s | 448,355,868 | 0.2334 | 18912 MB |
| cram | `--small` | 184.95 s | 450,635,350 | 0.2346 | 7550 MB |
| 7-Zip | `-mx=5` (default) | 49.62 s | 452,190,211 | 0.2354 | 5963 MB |
| zstd | `-19 --long` | 73.29 s | 454,490,514 | 0.2366 | 4104 MB |
| xz | `-6` (default) | 35.53 s | 458,409,724 | 0.2387 | 3185 MB |
| RAR | `-m5 -s -md512m` | 87.20 s | 474,538,515 | 0.2470 | 3962 MB |
| RAR | `-m5 -s` | 74.25 s | 482,801,949 | 0.2513 | 614 MB |
| **cram** | **`--auto`** (default) | **8.36 s** | 493,816,077 | 0.2571 | 1867 MB |
| zstd | `-3` (default) | 1.90 s | 540,088,970 | 0.2812 | 283 MB |
| cram | `--fast` | 2.19 s | 557,998,873 | 0.2905 | 177 MB |
| RAR | `-m3` (default) | 43.47 s | 581,071,715 | 0.3025 | 345 MB |

Default against default, cram is **5.9× faster than 7-Zip** and **5.2× faster than RAR while
also being 15% smaller**. Ignore the extract column here and read
[Decode](#decode) instead. The `extract` column that used to sit in this table was removed: it was
measured before the writes were being counted, and on a real disk the four tools land within 23% of
each other anyway.

7-Zip `-mx=9` reaches a ratio cram does not, and does it in half `--small`'s time. It pays
**18.9 GB of RAM** to get there, against cram's 7.5 GB.

**Writing a `.7z` rather than a `.cram`, cram beats 7-Zip at its own format and its own maximum.**
Same tree, same machine, 14 August:

| tool | setting | create | archive | peak RSS |
|---|---|---:|---:|---:|
| **cram** | **`--small`, `.7z`** | **95.0 s** | **135 MiB** | **4393 MB** |
| 7-Zip | `-mx=9` | 96.8 s | 136 MiB | 14851 MB |
| cram | `--auto`, `.7z` | 58.1 s | 140 MiB | 1265 MB |
| 7-Zip | `-mx=5` (default) | 41.0 s | 145 MiB | 5588 MB |

Smaller, marginally faster, and **3.4× less memory**, all three at once. `--auto` is the better
default even so: 96% of `--small`'s ratio for 61% of the time and 29% of its memory, and still
smaller than 7-Zip's default. Note this is cram *writing the 7z format*; the `.cram` rows above are a
different comparison and `--small` loses that one.

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

Superseded by [Decode](#decode), which measures all five tools across all four corpora with the same
method. On this corpus it gives cram 2.70 s, zstd 2.35 s, 7-Zip 3.95 s, RAR 7.74 s.

The August figures are kept here because the pair of columns is the clearest illustration in this
document of why the newer section exists:

| tool | to disk, `sync` included | to tmpfs |
|---|---|---|
| **cram** | 15.36 s | **2.58 s** |
| 7-Zip | 16.00 s | 3.64 s |
| RAR | 18.85 s | 7.25 s |

All nine disk extractions and all nine tmpfs extractions produced 42,151 files and
2,800,604,582 bytes; nothing was short.

The tmpfs column repeated to within 3% — cram 2.52–2.59, 7-Zip 3.63–3.65, RAR 7.24–7.43 — and
reproduced eight days later on a build thirty commits newer (2.70 / 3.95 / 7.74, with the
differences being real changes rather than noise).

The disk column did not: cram 8.69–18.54, 7-Zip 15.47–19.88, RAR 15.70–18.86, spreads that overlap
completely. **On disk these three are indistinguishable**, and anyone quoting a winner from that
column is quoting noise. That was already written here in August, and the `extract` column in every
other table went on doing exactly what this paragraph warns against until 14 August.

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

**Which archives split, exactly.** 7-Zip's multi-threaded LZMA2 encoder cuts its input into blocks
of **four times the dictionary size**, and each block opens with a dictionary reset. Measured by
varying the dictionary alone on identical input: 256 KiB gives 1.0 MiB segments, 4 MiB gives 16.0,
16 MiB gives 64.0, 32 MiB gives 128.0, 64 MiB gives 256.0. So:

> A `.7z` splits into `content ÷ (4 × dictionary)` segments, and only when it holds more than
> 4 × dictionary. At the `-mx=5` default that means **archives over 128 MiB, in 128 MiB pieces**.

7-Zip also shrinks the dictionary to fit a small input — 16 MiB of input reports a 24 MiB dictionary
— so below that threshold one block covers everything and there is nothing to split whatever `-mmt`
says. Above a 64 MiB dictionary the 4× relation stops holding (a 256 MiB dictionary gave 256 MiB
segments, not 1 GiB); the default is well inside the range that was confirmed.

**What happens when an archive splits less, or not at all.** 1 GiB of the same data written three
ways, extracted to tmpfs on 24 threads, three rounds each:

| written with | segments | cram | 7-Zip |
|---|---|---|---|
| `-mx=5`, stock | 9 | **1.71 s, 366 MB** | 1.77 s, 1917 MB |
| `-mx=9`, max | 5 | 3.09 s, 1096 MB | 2.45 s, 1916 MB |
| `-mmt=1` | 1 | 7.90 s, **64 MB** | 5.38 s, 127 MB |

Fewer seams means less to fan out over, and by `-mmt=1` there is nothing: one segment, one thread,
and 7-Zip's single-stream decoder is simply faster than ours. That is the honest floor of this
design — **cram is 47% slower there**, in a third of the memory.

Nowhere is cram the heavier of the two. That was not true earlier on 2026-08-13: both non-stock
cases used to fall out of the parallel path entirely and back onto the sequential reader, which cost
`-mmt=1` 10.62 s and 2477 MB, nineteen times 7-Zip's. Two gates were judging the wrong quantity —
one refused any block too large to cache, months after `copy_unit` made caching unnecessary, and the
other charged an archive's segments for every core rather than for the workers it could use.

`-mx=1` is the opposite extreme: a 256 KiB dictionary gives 1 MiB segments and over a thousand
units, and it extracts fastest of all at 0.71 s in 235 MB.

Extraction to a real disk is not reported here. On this machine it is write-bound and the four
columns do not separate: repeated runs of the same command ranged 9.85–26.68 CPU-seconds and
16.33–30.32 s wall, which supports no claim in either direction.

## Decode

Measured 14 August. Five tools at their own defaults, **destination `/dev/shm`**, warm-up discarded,
median of 3 with the tool order rotated each round. **All 80 extractions were counted by file and by
byte and `diff -rq`'d against the corpus, and all 80 verified.**

**Why a RAM disk, and why this column is called decode rather than extract.** On this machine the
drive sustains about 84 MiB/s, so extracting the kernel tree to it puts every tool within 23% of
every other — a number that describes a SATA disk in a KVM guest rather than an archiver. It does
not even repeat: anything under about 2 GiB fits the drive's cache and swings **2.5–3.6× between
identical runs**. Removing the write wall leaves decode plus the tool's own overhead, which is the
part that differs between these programs, and it repeats to within **1.00–1.22×** across every cell
below.

So this measures decoding, and it is labelled that way. **Extracting to a real disk is slower for
everyone and the tools converge**: the kernel tree takes cram 35.94 s there against 7-Zip's 44.12,
a 1.23× spread rather than the 2.4× below. If you are choosing a tool because extraction speed
matters and you are writing to a spinning or SATA disk, the honest answer is that it will barely
matter.

| | Silesia<br>202 MiB | enwik9<br>954 MiB | kernel tree<br>1832 MiB | Cram corpus<br>2671 MiB |
|---|---|---|---|---|
| **cram** `--auto` | 0.22 s · 497 MB | 1.23 s · 1245 MB | **1.84 s** · 1223 MB | 2.70 s · 1310 MB |
| zstd `-3` | **0.19 s** · 6 MB | **0.89 s** · 6 MB | 2.21 s · 6 MB | **2.35 s** · 6 MB |
| 7-Zip `-mx=5` | 0.90 s · 255 MB | 1.71 s · 1176 MB | 4.41 s · 2285 MB | 3.95 s · 4877 MB |
| RAR `-m3 -s` | 0.52 s · 44 MB | 2.57 s · 44 MB | 7.50 s · 44 MB | 7.74 s · 44 MB |
| xz `-6` | 1.98 s · 10 MB | 8.05 s · 10 MB | 8.87 s · 10 MB | 18.30 s · 10 MB |

**Against the tools in its class cram wins every cell.** Over 7-Zip: 4.1× on Silesia, 1.4× on
enwik9, 2.4× on the kernel tree, 1.5× on the Cram corpus. Over RAR: 2.4×, 2.1×, 4.1×, 2.9×. Over
xz, between 6.8× and 9.0×.

**zstd `-3` decodes faster than cram on three of the four**, and that belongs here rather than in a
footnote. It is a far weaker compressor — its archive is the largest in every table above, so there
is less to decode — which makes it a different point on the curve rather than a better one. cram
takes it only on the kernel tree, and loses the other three.

**The memory column is two different stories and should not be quoted as one.** Against 7-Zip cram
is decisively cheaper: 1223 MB against 2285 on the kernel tree, and **1310 MB against 4877** on the
Cram corpus. Against the pipe-based tools it is much heavier — zstd and xz sit at 6–10 MB because a
tar pipe holds nothing at all, and RAR at 44 MB. "Cheaper on memory" is true of 7-Zip and false of
zstd.

## Writing `.tar.gz`

`.tar.gz` is its own comparison because the competitor is not an archiver. **The competitor is
`pigz`** — parallel gzip, by Mark Adler, who co-authored zlib and the gzip format, packaged in every
distro since 2007. Same machine and method as the Decode table: destination `/dev/shm`, warm-up
discarded, median of 3 with the order rotated, every tool given all 24 threads.

| kernel tree, 2.1 GB | wall | bytes |
|---|---|---|
| **cram**, default | **3.19 s** | **558,402,429** |
| `pigz -6 -p 24` | 3.46 s | 566,208,712 |
| `gzip -6` (one thread) | 30.75 s | 566,354,268 |

**1.08× faster than pigz, in a 1.38% smaller archive.** The `gzip` row is there for scale, not as a
comparison: an earlier version of this section reported 5.9–9.6× against `tar czf` and that was
true, uninteresting and quietly misleading, because `tar czf` compresses on one thread. Anyone
qualified to read this table knows pigz exists.

The archive is smaller than both because the chunk is 1 MiB where pigz's is 128 KiB, so cram throws
away eight times less dictionary at the seams.

**Extraction is still single-threaded, and is faster anyway.** A standard `.gz` cannot be extracted
in parallel by anyone, cram included: a decoder cannot find the block boundaries without inflating
everything before them, and pigz decompresses in the same time as gzip for that reason. **`cram x` on
a `.tar.gz` is 4.79 s against `gzip -dc | tar`'s 6.14** on the kernel tree — 1.28× faster, on one
decode thread each. It was 2.26× *slower* until 2026-08-15; what changed was ours, not the format's.

`.tar.xz` and `.tar.bz2` are the exception, and for a reason specific to how cram writes them: both
are emitted as a run of complete standalone streams, and a run of streams **can** be split without
inflating anything before the split. Those two decode on every core, which is why `.tar.xz` is
1.54× faster than `xz -dc | tar` rather than 2.32× slower. The remaining tar codecs are in
[Where cram loses](#where-cram-loses).

**What it costs.** The stream is cut into 1 MiB chunks compressed independently, so each starts with
an empty dictionary and the archive grows by 0.19–0.34% against a single-stream gzip. Peak memory is
the window — 177 MB on Silesia against 17 MB before, 235 MB on the kernel tree — and CPU rises
30–39% for the wall-clock. On one core the chunked writer is neither faster nor slower than the
streaming one (6.12 s against 6.28 s on Silesia), so the CPU is the price of concurrency rather than
of chunking, and a machine with nothing to parallelise over does not pay it.

The output does not depend on how many cores wrote it: chunk boundaries are byte offsets in the tar
stream, so a 1-thread and a 24-thread run produce the same archive to the byte.

## Where cram loses

- **Extracting a `.tar.*`, on four of six codecs.** Kernel tree, `/dev/shm`, archives written by
  cram, every competitor re-measured in the same session on the same binary (2026-08-15), two runs
  each agreeing within 2%:

  | codec | cram | native | |
  |---|---|---|---|
  | `xz` | **5.50 s** | 8.81 s | **1.60× faster** |
  | `gz` | **4.94 s** | 6.17 s | **1.25× faster** |
  | `lz4` | 2.10 s | 2.01 s (`lz4 -dc \| tar`) | 1.05× slower |
  | `zst` | 2.69 s | 2.14 s | 1.27× |
  | `br` | 5.40 s | 3.77 s (`brotli -dc \| tar`) | 1.43× |
  | `bz2` | 5.90 s | 3.22 s (lbzip2) | 1.83× |

  A plain `.tar` — no codec at all — is **1.44 s against GNU tar's 1.69**, which is the same engine
  underneath every row above.

  This was 2.2–5.6× behind on every row that morning, and `bz2` was 19.1× behind. Three fixes, none
  of them in a codec. The tar worker allocated and zeroed a 1 MiB buffer for **every entry** — 94,778
  of them on this tree, to carry files averaging 20 KB — and passed results over a one-slot channel,
  which is a ping-pong rather than a pipeline. The concatenated streams cram's chunked writer emits,
  which had been walked one at a time, started being decoded on a pool. The extraction path was
  asking the filesystem the same questions twice per file. And writing those files ran on one thread
  while decoding ran on another, so the machine's other cores did nothing.

  **That last one is the reason every row above moved, and it is worth being precise about.**
  Extracting a plain `.tar` — no codec, nothing to decode — took 3.38 s against GNU tar's 1.73, so
  the gap was entirely ours. `strace` counted **1.83 million syscalls against GNU tar's 0.79
  million**: a `mkdir` per file that failed `EEXIST` 94,779 times where GNU tar issued 6,214 that
  succeeded, two `statx` per file to ask about paths an `openat` was about to answer, a second
  `openat` per file because the mtime was set by path rather than on the handle already open, and
  526,938 `read` calls for 2 GB because a plain `.tar` was handed an unbuffered file. Remembering
  which directories exist, creating with `create_new`, stamping the descriptor and buffering the
  source took it to 2.26 s and every compressed codec down with it.

  **Writing them across a pool took it to 1.44 s, past GNU tar.** Decoding a tar is one pass and has
  to stay one thread; writing what it decodes does not. Small entries accumulate into a bounded batch
  and go out on eight writers — eight because the width knees there and then goes backwards, giving
  1.43 s at eight against 1.50 s and 311% CPU at twenty-four. `gz` and `br` are the two rows that
  pay ~3% for it rather than gaining, because their decode is single-threaded and is the wall, so
  extra writers only contend.

  **`bz2` remains the outlier, and `cram t` says where the rest of it lives.** Decoding without
  writing anything, cram takes 5.22 s against `lbzip2 -dc`'s 1.67 and `bunzip2 -c`'s 33.57 — so the
  pool works, and what is left is CPU per byte: 90 CPU-seconds against `bunzip2`'s 33 for the same
  data. That is the pure-Rust bzip2 backend rather than cram's machinery, and the same machinery on
  `gz` costs 4.7 CPU-seconds against `gzip -dc`'s 5.74. More workers cannot fix it.

  **The `xz` and `bz2` rows depend on the archive being a run of streams, and say so.** Those two
  decode on every core by splitting at the seams cram's own writer leaves; `pbzip2`, `lbzip2` and
  `cat a.xz b.xz` leave the same ones. An archive that is a **single** stream — what plain `bzip2`
  or `xz` produces — has nothing to split, and cram reads it at one-core speed like everyone else:
  the same kernel tree through a single-stream `.tar.bz2` from stock `bzip2 -9` takes **62.4 s**, not
  6.12. Detecting that costs nothing measurable (62.4 s against 62.5 s with the scan disabled
  outright), but the speed is not there to be had.

  For scale: the same tree out of a `.cram` extracts in 1.84 s.
- **Creating a `.tar.bz2` and a `.tar.xz`**, against the threaded specialists: bz2 7.85 s against
  lbzip2's 4.99, xz 41.35 s against `xz -T0`'s 34.45. Both write a smaller or equal archive
  (0.88% smaller on xz), and both improved substantially on 2026-08-15, but neither wins.
- **Maximum ratio on large corpora.** 7-Zip `-mx=9` is 1.84% smaller on the kernel tree and
  5.09% smaller on enwik9. On the kernel tree cram `--small` is dominated outright: bigger and
  slower. Use `--auto` there and take the speed.
- **Single-file archives, on extraction** — mostly closed on 2026-08-14, and still a loss. The
  parallel path's unit of work was the entry, so one entry meant one thread whatever the machine.
  A `.cram` entry can now be cut at its pack boundaries and its pieces decoded concurrently.
  Measured on the 24-thread Linux box, enwik9 to tmpfs, three rounds, output byte-identical to the
  original 1,000,000,000-byte file:

  | | wall | effective cores | peak |
  |---|---|---|---|
  | before | 9.05 s | 1.0 | 568 MB |
  | after | **2.06 s** | 5.4 | 900 MB |
  | 7-Zip | 1.64 s | 4.4 | 1176 MB |

  4.4× faster and 26% behind 7-Zip rather than 5.5×, in 23% less memory. The memory rise is the
  decode window, which is bounded by the worker count.
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
