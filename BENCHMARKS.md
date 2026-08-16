# Benchmarks

Measured 16 August 2026 against 7-Zip 26.01, RAR 7.12, xz 5.4.5, zstd 1.5.5, pigz 2.8 and lbzip2
2.5 on one Linux machine, in one afternoon, on one binary: **cram 1.1.0, commit `a84d77d`**, features
`download,zstd-c,phash,mimalloc`. Every archive in every table was extracted and compared against
the source: **68 of 68 create cases, 60 of 60 decode cases and 45 of 45 tar-family cases verified,
zero failures.** On the Cram corpus every extraction was counted by file and by byte as well.
Commands are given in full so the numbers can be checked.

Nothing here is estimated. Where cram loses, the row is in the same table. Where a figure was
carried forward from an earlier run rather than re-measured on 16 August, it says so and gives its
date.

**One machine, one afternoon.** Every ratio below is a property of this box and these four corpora
as much as of the tools.

**What reproduced across runs.** The 5–6 August and 14 August runs are superseded by this one. Of
the create sizes, `--auto` and `--fast` reproduced to the byte across all four corpora and three
builds. `--small` did not: Silesia moved 48,012,170 → 48,394,418, the kernel tree
450,635,350 → 450,691,514 and the Cram corpus 1,660,841,040 → 1,660,873,216, while enwik9 held at
219,486,008. Filter selection at `--small` changed in between, so those three are a real difference
and not noise. The `--store` rows are new in this run and have nothing to reproduce against.

> **The `extract` column was removed from the four create tables on 14 August and replaced by
> [Decode](#decode).** Its times implied write rates the destination cannot reach: extracting the
> 1,920,837,858-byte kernel tree in the 2.21 s it published needs 829 MiB/s, on a volume measuring
> **84 MiB/s sustained**. It affected every tool equally, not cram alone. Two causes, both in the
> method: `sync` flushes the whole system rather than what the tool just wrote, and the method never
> said where extractions were written. The replacement measures to a RAM disk and calls itself
> decode. On a real disk the tools converge to within 23% and it barely matters which you pick.
>
> **A third fault was found on 16 August and is fixed here.** The decode harness never warmed the
> archive before starting the clock, so every decode row also timed a SATA read. On the Cram corpus
> the rows spread 17–140% between identical runs. The old numbers were reading the drive. Warming
> the archive first brings every cell in [Decode](#decode) to a 0.0–13.3% spread.

**Four corpora, and they say different things.** Silesia and enwik9 contain no duplicate content at
all. The kernel tree contains a little, and measuring how little turned out to be the most
interesting result in this document; see [Corpora](#corpora). The Cram corpus is 15% duplicate by
construction and is the one built to measure deduplication. The size claims move a long way between
them, and which is relevant depends on whether your data repeats itself.

## Summary

**On data that repeats itself, cram wins on both axes against two of the three settings it is
compared with.** On the Cram corpus, default against default: **9.8× faster than 7-Zip and 13.4%
smaller**, and **7.7× faster than RAR `-m3` and 16.3% smaller**. Against RAR at `-m5 -s`, which is
the setting that matches cram's ratio rather than RAR's default, it is **12.5× faster at a ratio
0.12% apart**, which is a tie on size.

**On data that does not repeat, cram is faster and larger.** On Silesia, enwik9 and the kernel tree,
cram's default is 6.0–11.4× faster than 7-Zip's default and writes a 9.2–19.2% larger archive. That
is a different point on the speed/size frontier, not a dominance.

**`--store` is the cleanest demonstration of what `.cram` is for.** With every compressor switched
off on both sides, cram writes **15.3% less than 7-Zip `-mx=0`** and 15.4% less than RAR `-m0 -s` on
the Cram corpus. Nothing is being compressed by anybody; the difference is deduplication, and there
is no flag on either competitor that recovers it. On the kernel tree the same switch reads 0.9791,
which is the finding in [Corpora](#corpora).

**cram no longer holds the smallest archive on Silesia.** 7-Zip `-mx=9` tuned writes 48,287,980
against cram `--small`'s 48,394,418, so 7-Zip is 0.22% ahead. cram `--small` still beats `xz -9e`,
by 0.47%. **It does not hold the minimum on the other two
pure-compression corpora either**: cram `--small` is 1.85% larger than 7-Zip `-mx=9` on the kernel
tree and 5.09% larger than 7-Zip `-mx=9` tuned on enwik9, and on the kernel tree it is *dominated* by
7-Zip `-mx=9`, which is both smaller and faster.

**Nobody extracts a `.zip` on more than one core, and cram does.** Reading the same cram-written
archive, cram is **3.81× 7-Zip's ZIP reader and 5.11× Info-ZIP** on the kernel tree, at 272–409% CPU
against their 99–100%. Creating one it is 4.42× and 9.21× faster, for an archive 0.55% larger than
7-Zip's and 1.41% smaller than Info-ZIP's. See [`.zip`](#zip-against-7-zip-and-info-zip).

**Decoding, cram beats every tool in its class and loses to zstd.** With the write wall removed it
is **1.4–4.0× faster than 7-Zip**, 2.1–4.2× faster than RAR and 5.1–9.1× faster than xz across four
corpora. zstd `-3` decodes faster on three of the four, which is what a much weaker compressor buys:
its archives are the largest in every table here. Full numbers, including the three cram loses, in
[Decode](#decode).

**Memory is a range and it reverses.** Decoding, cram peaks at 1271 MB against 7-Zip's 4877 on the
Cram corpus and 1183 against 2284 on the kernel tree, but **475 against 255 on Silesia and 1245
against 1176 on enwik9, where cram is the heavier of the two.** Against the pipe-based tools (zstd
and xz at 6–10 MB, RAR at 44) cram is far heavier everywhere, because a tar pipe holds nothing.
"Cheaper on memory" is true of 7-Zip on the two large corpora and false everywhere else.

**On a real disk none of the decode ordering shows.** Extraction is write-bound: the same tools land
within 23% of each other, and the choice barely matters if you are writing to a spinning or SATA
disk.

**Writing a `.tar.gz` is faster than `pigz`, by 1.07×, in a 1.38% smaller archive.** pigz is the
right comparison and `tar czf` is not, because `tar czf` pipes through one thread, so beating it says
nothing except that we use the machine. Numbers, and what the chunking costs, in
[Writing `.tar.gz`](#writing-targz).

**Reading one is 2.33× faster than `gzip -dc | tar`**, 2.63 s against 6.13 on the kernel tree, both
on a single decode thread, because a standard `.gz` cannot be parallelised by anybody. Until
2026-08-15 this was 2.26× *slower*, and every cause was ours: a megabyte allocated and zeroed for
every one of 94,778 entries, a decoder that could never get more than one message ahead of its
consumer, an extraction path issuing 1.83 million syscalls where GNU tar issues 0.79 million, files
written on one thread while another decoded, and a compressed tar being decoded **twice**, once to
list it and once to extract it.

**Five of the six tar codecs are faster than the tool everyone already has, the sixth is level, and
a plain `.tar` is faster than GNU tar.** `.tar.bz2` used to be the loss in this list; over five
rounds it is now 3.16 s against lbzip2's 3.22, with spreads of 1.0% and 2.2%. That gap is inside the
noise and is claimed as neither a win nor a loss. See [Tar extraction](#tar-extraction).

**One corpus exposed a real weakness and it is now closed against 7-Zip.** enwik9 is a single 1 GB
file, and extraction fanned out per entry, so one entry meant one thread whatever the machine.
Cutting the entry at its pack boundaries took it from **9.05 s at 1.0 effective cores to 2.06 s at
5.4** (measured 14 August, not re-run under the warmed method). Under the 16 August method enwik9
decodes in **1.20 s against 7-Zip's 1.69**, so cram is 1.4× ahead there; it is still behind zstd's
0.89 s.

**Opening somebody else's `.7z` is level with 7-Zip on time and well under it on memory**, at 3.26 s
against 3.68 s on the Cram corpus, in **867 MB against 7-Zip's 4876 MB**. **Measured 13 August and
not re-run on 16 August**, where 7-Zip extracting the same corpus took 3.82 s with a 5.6% spread, so
the 3.68 s beside it sits inside today's band. Stated with its conditions, because they are
load-bearing:

- It applies to a `.7z` holding **more than 128 MiB, written at stock settings**. That is where a
  multi-threaded encoder leaves dictionary resets a decoder can start from. The rule is exact,
  block size is four times the dictionary, and derived from measurement rather than assumed.
- Below that threshold, or with `-mmt=1`, there are no seams. cram gives time back and is **47%
  slower**, in a third of the memory.
- At `-mx=9` the segments are 256 MiB and cram is **26% slower**, in 1.75× less memory.

Every one of those is measured in that section, including the ones where cram loses.

**Creating the Cram corpus, default against default, costs 2590 MB against 7-Zip's 7084 and RAR
`-m3`'s 332.** At maximum, 7-Zip needs 21,830 MB at `-mx=9 -mmt=24` and 21,840 at `-mx=9` tuned,
against cram `--small`'s 7858; on the kernel tree it is 18.9 GB against cram's 11.0 GB. Against the
threaded tar tools cram is dearer, and against the pipe-based readers dearer by one to two orders of
magnitude: extracting a `.tar.gz` costs 199 MB against `gzip -dc | tar`'s 3 MB, because a pipe holds
nothing and a pipeline holds its buffers. That is the price of the parallel paths and it is not
going away.

Extraction also holds a bounded batch of decoded entries for its writer pool, which is most of why a
plain `.tar` peaks at 251 MB. Where a `.tar.bz2` or `.tar.xz` decodes on a pool as well the price is
explicit and bounded: 537 MB and 368 MB, held to a 256 MiB budget for the decoded bytes in flight
plus the decoders themselves. `CRAM_PARALLEL_DECODE=0` gives the memory back and takes the speed
with it. On `bz2` this is not the expensive option: `lbzip2` peaks at 537 MB on the same archive,
which is cram's figure to the megabyte.

## Machine

| | |
|---|---|
| CPU | AMD Ryzen 9 5900X, 24 threads |
| RAM | 23 GiB |
| OS | Ubuntu 24.04.4, kernel 6.8 |
| Storage | dedicated ext4 volume for archives; decode destination `/dev/shm` (tmpfs) |

cram 1.1.0, commit `a84d77d`, built with the shipping feature set
(`download,zstd-c,phash,mimalloc`) — the set `.github/workflows/ci.yml` builds and the one users get.
A default-feature build writes XZ where the shipped one writes zstd, links the system allocator
rather than mimalloc, and is not the tool users get. `cargo install cram-cli` produces that
default-feature build and will not reproduce these tables.

## Method

- Silesia is the median of 3 runs; enwik9, the kernel tree and the Cram corpus the median of 2.
  Decode is the median of 3 rounds with a warm-up discarded, the tar family the median of 3, and the
  `.tar.bz2` comparison in [Tar extraction](#tar-extraction) the median of 5. Every repetition
  rotates the tool order, so no tool is permanently first or last.
- Median convention: the middle value for odd counts, the mean of the two middle values for even.
- The corpus is read into the page cache immediately before every timed create, so no tool pays to
  warm the disk for the next one.
- **The archive is read into the page cache before every timed decode.** This is new on 16 August
  and it is what fixed the decode column. Without it the timer includes a read of the archive from a
  SATA volume, which on the Cram corpus made identical runs spread 17–140%. Those older extraction
  numbers were measuring the drive.
- **Every tool is given all 24 threads explicitly** (`-mmt=24`, `-mt24`, `-T0`) rather than left to
  its default.
- **Every tool is run at both its own default and its documented maximum.** Comparing one tool's
  maximum against another's default is the standard way these comparisons mislead.
- **RAR gets two rows on the Cram corpus, and the reason is that solidity is not a default it
  shares with 7-Zip.** 7-Zip is solid by default; RAR is not. `rar -m3` is what a user gets by
  typing the command, and `rar -m5 -s` is the setting that lands on cram's ratio. Publishing only
  the first hides that RAR can reach this ratio; publishing only the second compares cram's default
  against a competitor's tuned run. Both are in the table.
- Archives are deleted between runs; 7-Zip and RAR append to an existing archive otherwise.
- Peak RSS via `/usr/bin/time -f '%M'`.
- Every archive is extracted and `diff -rq`'d against the source. A ratio from an archive nobody
  opened is a rumour.
- **Every extraction is counted, not trusted.** File count and total bytes are compared against the
  corpus afterwards. A fast extraction that dropped half the files is not fast, and an exit code
  does not catch it.
- **Decode timings are taken to a RAM disk** (`/dev/shm`), stated as the destination, with the
  destination emptied before every timed run. That last part matters for one tool only: tmpfs pages
  are RAM and cram's planner reads available RAM to size its worker count, so leaving the previous
  extraction in place would change cram's plan and no competitor's.
- Create timings stop at process exit, which is the conventional measurement and is stated here
  rather than hidden. Creation writes far less than it reads, so the effect is much smaller than on
  the read side.

**The harness applies a memory cap, and this document used to publish its effect as a tool
failure.** `tools/corpus/bench-corpus.sh` sets `MEMCAP` (default `20G`) and runs every tool under
`systemd-run -p MemoryMax=$MEMCAP`. The cap is a fixed figure, not the machine's RAM: this box has
23 GiB, so a tool can be killed at 20 GB with room still on the machine. That is what produced the
`7-Zip -mx=9 -mmt=24 killed at 20 GB` row previously published on the Cram corpus, and the cap was
never disclosed here. The cap also applies only when `systemd-run --user` passes the harness's
enforcement probe; otherwise the run proceeds uncapped and the harness records `CAPPED=no`. On
16 August that command completed both repetitions at 21,830 MB, which is above the cap, so the cap
was not in force for this run.

**A previous method used `sync` inside the timed extraction region and it was not enough.** `sync`
flushes the whole system rather than the writes the tool just made, so on a machine doing anything
else each run is charged for unrelated writeback, and the resulting figures still implied write rates
the drive cannot reach. Per-file `fsync` fixes the accounting but does not fix that this drive
cannot measure anything under 2 GiB repeatably, which is why the RAM disk is used instead. The
reasoning behind that older paragraph was right and its conclusion was wrong; it is recorded here
rather than deleted.

**All tools archive exactly the same bytes.** The kernel tree ships 99 symbolic links, twelve of
them pointing at directories. cram skips symlinks and reports each one; 7-Zip and RAR dereference
them, which on this tree silently duplicates 8,011 files. Rather than caveat that asymmetry, the
symlinks were removed from the corpus, so every tool sees an identical file set.

## Corpora

| corpus | files | bytes | duplicate | source |
|---|---|---|---|---|
| Silesia | 12 | 211,938,580 | none | `silesia.zip`, sha256 `0626e25f45c0ffb5dc801f13b7c82a3b75743ba07e3a71835a41e3d9f63c77af` |
| enwik9 | 1 | 1,000,000,000 | none | mattmahoney.net/dc/enwik9.zip |
| Linux kernel tree | 94,778 | 1,920,837,858 | **0.20% whole-file, 2.09% chunk-level** | `.git` and symlinks excluded |
| **Cram corpus 1.0** | 42,151 | 2,800,604,582 | **15.0%** | [`tools/corpus`](tools/corpus), id `deb5f932d27a913ad6da2b994be7e66bffd03d6bf8546abd3de8ca7344efe599` |

Silesia and enwik9 are the corpora the compression field publishes against, so anyone can re-run
these. The kernel tree stands in for what people actually archive: many small files, mixed text and
binary.

**The kernel tree repeats itself, and by ten times more than a file-level scan can see.** This
document used to record its duplicate content as "none". Hashing all 94,778 files gives 94,144
distinct contents: **634 redundant files, 3,850,979 duplicate bytes, 0.20% of the tree.** But
`cram --store` writes 1,880,759,769 bytes against 1,920,837,858 in, a saving of **40,078,089 bytes,
2.09%**, of which whole-file duplication accounts for 3,850,979. The remaining 36.2 MB, **1.89% of
the tree, is sub-file dedup.**

`--store` provably does not compress. On enwik9, which is highly compressible text, it writes
1,000,193,329 bytes for 1,000,000,000 in; on Silesia 211,978,851 for 211,938,580. Both are *larger*
than the input by about 0.02% of container overhead. So the only size-reducing mechanism in play on
the kernel tree is deduplication, and it is not operating at file granularity. **On a real source
tree, content-defined chunking finds ten times the redundancy that whole-file dedup finds, 2.09%
against 0.20%, with every compressor switched off.**

**The Cram corpus is the one built to measure deduplication at a realistic scale.** It is built by a
script in this repository from the Linux kernel, Big Buck Bunny and 202 Wikimedia Commons
photographs, all redistributable, and every download is checksum-pinned, so two people building it
get byte-identical trees. `CORPUS.id` above is a digest over every file in it; if yours matches, you
have the same corpus.

Its duplicate content is **15.0%**, sitting in one top-level directory called `dup/`. That figure is
an assumption about what a working drive looks like, and it is the assumption every dedup number
below depends on, so it is deliberately deletable: `rm -rf dup/` and re-run to see the corpus
without it. If you think 15% is generous, measure both and say so.

## Silesia, 211,938,580 bytes

Medians of 3.

| tool | setting | create | spread | bytes | ratio | peak RSS |
|---|---|---:|---:|---:|---:|---:|
| 7-Zip | `-mx=9` tuned | 40.47 s | 0.5% | **48,287,980** | **0.2278** | 2088 MB |
| **cram** | `--small` | 57.01 s | 0.5% | 48,394,418 | 0.2283 | 2180 MB |
| xz | `-9e` | 80.05 s | 1.4% | 48,624,588 | 0.2294 | 1071 MB |
| 7-Zip | `-mx=9` | 34.54 s | 1.3% | 48,688,243 | 0.2297 | 2088 MB |
| 7-Zip | `-mx=9` own mt | 34.59 s | 1.5% | 48,688,243 | 0.2297 | 2088 MB |
| xz | `-6` (default) | 11.87 s | 1.4% | 49,586,256 | 0.2340 | 1086 MB |
| 7-Zip | `-mx=5` (default) | 17.33 s | 1.0% | 49,597,414 | 0.2340 | 907 MB |
| zstd | `-19 --long` | 31.90 s | 1.0% | 52,778,162 | 0.2490 | 589 MB |
| RAR | `-m5 -s` | 5.30 s | 1.9% | 53,120,775 | 0.2506 | 580 MB |
| RAR | `-m5 -s -md512m` | 6.42 s | 0.6% | 53,134,102 | 0.2507 | 2392 MB |
| RAR | `-m3` (default) | 3.06 s | 1.0% | 54,218,452 | 0.2558 | 309 MB |
| **cram** | `--auto` (default) | **1.52 s** | 1.3% | 58,280,168 | 0.2750 | 915 MB |
| zstd | `-3` (default) | 0.21 s | 9.5% | 66,625,332 | 0.3144 | 250 MB |
| **cram** | `--fast` | **0.18 s** | 0.0% | 69,474,237 | 0.3278 | 236 MB |
| 7-Zip | `-mx=0` | 0.19 s | 0.0% | 211,939,007 | 1.0000 | 6 MB |
| RAR | `-m0 -s` | 0.19 s | 5.6% | 211,939,937 | 1.0000 | 20 MB |
| **cram** | `--store` | 0.26 s | 4.0% | 211,978,851 | 1.0002 | 333 MB |

cram holds the two fastest points: `--fast` is the quickest row in the table at 0.18 s, and `--auto`
at 1.52 s is the quickest of everything reaching its ratio or better. **7-Zip `-mx=9` tuned holds
the smallest archive**, 0.22% ahead of cram `--small`; earlier versions of this document claimed
that cell for cram and were wrong. 7-Zip owns the middle. cram `--small` is 0.47% smaller than
`xz -9e`.

`--store` writes 40,271 bytes more than the input, a ratio of 1.0002. That is the container
overhead on a corpus with no duplicate content for dedup to remove.

## enwik9, 1,000,000,000 bytes

Medians of 2.

| tool | setting | create | spread | bytes | ratio | peak RSS |
|---|---|---:|---:|---:|---:|---:|
| 7-Zip | `-mx=9` tuned | 159.45 s | 0.7% | **208,851,242** | **0.2089** | 9833 MB |
| 7-Zip | `-mx=9` | 155.81 s | 0.5% | 210,604,362 | 0.2106 | 9835 MB |
| 7-Zip | `-mx=9` own mt | 155.22 s | 0.6% | 210,604,362 | 0.2106 | 9835 MB |
| xz | `-9e` | 252.19 s | 0.8% | 214,153,080 | 0.2142 | 3666 MB |
| **cram** | `--small` | 138.70 s | 4.4% | 219,486,008 | 0.2195 | 9696 MB |
| RAR | `-m5 -s -md512m` | 42.40 s | 0.6% | 219,984,649 | 0.2200 | 3910 MB |
| 7-Zip | `-mx=5` (default) | 69.06 s | 0.3% | 224,618,387 | 0.2246 | 3775 MB |
| zstd | `-19 --long` | 65.08 s | 3.4% | 230,822,761 | 0.2308 | 2402 MB |
| xz | `-6` (default) | 38.56 s | 0.4% | 233,402,304 | 0.2334 | 3032 MB |
| RAR | `-m5 -s` | 31.34 s | 1.4% | 237,654,873 | 0.2377 | 564 MB |
| RAR | `-m3` (default) | 21.48 s | 0.4% | 249,222,506 | 0.2492 | 293 MB |
| **cram** | `--auto` (default) | **11.50 s** | 0.6% | 267,632,141 | 0.2676 | 2135 MB |
| zstd | `-3` (default) | 0.80 s | 1.3% | 312,548,072 | 0.3125 | 259 MB |
| **cram** | `--fast` | 0.84 s | 1.2% | 328,890,853 | 0.3289 | 251 MB |
| 7-Zip | `-mx=0` | 0.82 s | 0.0% | 1,000,000,122 | 1.0000 | 5 MB |
| RAR | `-m0 -s` | 1.00 s | 0.0% | 1,000,000,150 | 1.0000 | 20 MB |
| **cram** | `--store` | 1.11 s | 3.7% | 1,000,193,329 | 1.0002 | 163 MB |

**This is cram's worst corpus and the reason is structural.** A `.cram` compresses each pack
independently, so its match window is one pack — 64 MiB less one maximum chunk at `--small` — against
LZMA's whole-file solid block. On one 1 GB file that costs 5.09% against 7-Zip's tuned maximum. RAR
at `-md512m` reaches cram's `--small` ratio in 42.40 s against 138.70, under a third of the time.

## Linux kernel tree, 1,920,837,858 bytes, 94,778 files

Medians of 2.

| tool | setting | create | spread | bytes | ratio | peak RSS |
|---|---|---:|---:|---:|---:|---:|
| 7-Zip | `-mx=9` | 93.59 s | **8.4%** | **442,486,736** | **0.2304** | 18907 MB |
| 7-Zip | `-mx=9` own mt | 100.62 s | 0.5% | 442,486,736 | 0.2304 | 15877 MB |
| xz | `-9e` | 243.03 s | 0.2% | 445,092,440 | 0.2317 | 3842 MB |
| 7-Zip | `-mx=9` tuned | 107.48 s | **6.5%** | 448,355,868 | 0.2334 | 18913 MB |
| **cram** | `--small` | 118.86 s | 2.9% | 450,691,514 | 0.2346 | 11024 MB |
| 7-Zip | `-mx=5` (default) | 50.06 s | 0.2% | 452,190,211 | 0.2354 | 5963 MB |
| zstd | `-19 --long` | 73.23 s | 0.9% | 454,490,514 | 0.2366 | 4104 MB |
| xz | `-6` (default) | 35.81 s | 3.0% | 458,409,724 | 0.2387 | 3194 MB |
| RAR | `-m5 -s -md512m` | 86.41 s | 0.2% | 474,538,515 | 0.2470 | 3961 MB |
| RAR | `-m5 -s` | 72.88 s | 0.6% | 482,801,949 | 0.2513 | 614 MB |
| **cram** | `--auto` (default) | **7.64 s** | 0.7% | 493,816,077 | 0.2571 | 2092 MB |
| zstd | `-3` (default) | 1.90 s | 2.1% | 540,088,970 | 0.2812 | 272 MB |
| **cram** | `--fast` | 1.96 s | 3.1% | 557,998,873 | 0.2905 | 334 MB |
| RAR | `-m3` (default) | 43.69 s | 5.4% | 581,071,715 | 0.3025 | 345 MB |
| **cram** | `--store` | 2.44 s | 4.2% | **1,880,759,769** | **0.9791** | 603 MB |
| 7-Zip | `-mx=0` | 6.78 s | 0.4% | 1,921,891,176 | 1.0005 | 130 MB |
| RAR | `-m0 -s` | 4.02 s | 0.5% | 1,934,212,853 | 1.0070 | 59 MB |

Default against default, cram is **6.6× faster than 7-Zip** and **5.7× faster than RAR while also
being 15% smaller**. Read [Decode](#decode) for the extraction side; the `extract` column that used
to sit in this table was removed because it was measured before the writes were being counted.

**The two 7-Zip `-mx=9` rows are the only timings in this table with spreads above 6%,** at 8.4% and
6.5%. They allocate about 18.9 GB on a 23 GiB box, so they time the page cache as much as the
compressor. Quote them with the spread attached or not at all. Two other cells in this document run
wider — Silesia `zstd -3` at 9.5% and the Cram corpus's 7-Zip `-mx=0` at 14.3% — but both are runs
of under five seconds, where a fraction of a second is a large percentage and a small fact.

With that caveat: 7-Zip `-mx=9` reaches a ratio cram does not, with cram `--small` 1.85% larger, and
does it in 93.59 s against 118.86, so 79% of the time. It pays **18.9 GB of RAM** to get there,
against cram's 11.0 GB, a gap of 1.7×. cram `--small` is 0.33% smaller than 7-Zip's default `-mx=5`
and takes 2.37× as long.

The `--store` row is the interesting one on this corpus and is discussed under [Corpora](#corpora):
0.9791 on a tree with 0.20% whole-file duplication.

**Writing a `.7z` rather than a `.cram`, cram beats 7-Zip at its own format and its own maximum.**
Same tree, same machine. **Measured 14 August and not re-run on 16 August**, so do not compare these
timings against the table above:

| tool | setting | create | archive | peak RSS |
|---|---|---:|---:|---:|
| **cram** | **`--small`, `.7z`** | **95.0 s** | **135 MiB** | **4393 MB** |
| 7-Zip | `-mx=9` | 96.8 s | 136 MiB | 14851 MB |
| cram | `--auto`, `.7z` | 58.1 s | 140 MiB | 1265 MB |
| 7-Zip | `-mx=5` (default) | 41.0 s | 145 MiB | 5588 MB |

Smaller, marginally faster, and 3.4× less memory, all three at once. `--auto` is the better default
even so: 96% of `--small`'s ratio for 61% of the time and 29% of its memory, and still smaller than
7-Zip's default. Note this is cram *writing the 7z format*; the `.cram` rows above are a different
comparison and `--small` loses that one.

## Cram corpus 1.0, 2,800,604,582 bytes, 42,151 files

Medians of 2, tool order rotated per round, corpus re-read into page cache before every create.
15.0% of this corpus is duplicate content; see [Corpora](#corpora) for what that means and how to
delete it.

### Default settings

| tool | mode | create | spread | archive | ratio | peak RSS |
|---|---|---:|---:|---:|---:|---:|
| **cram** | `--auto` | **6.95 s** | 0.1% | 1,989,536,373 | 0.7104 | 2590 MB |
| 7-Zip | `-mx=5` | 68.25 s | 0.5% | 2,297,095,363 | 0.8202 | 7084 MB |
| RAR | `-m3` (RAR's default) | 53.25 s | 0.2% | 2,377,874,586 | 0.8491 | 332 MB |
| RAR | `-m5 -s` (ratio-matched) | 86.78 s | 0.3% | 1,987,130,640 | 0.7095 | 598 MB |

**9.8× faster than 7-Zip and 13.4% smaller.** Against RAR's own default, **7.7× faster and 16.3%
smaller**. Against RAR `-m5 -s`, which is where RAR reaches this ratio, **12.5× faster** with RAR
2,405,733 bytes ahead out of 1.99 GB, a difference of 0.12% that is a tie rather than a win for
either.

RAR appears twice because it is not solid by default and 7-Zip is; the Method section says why both
rows are published.

### As small as each one goes

| tool | mode | create | spread | archive | ratio | peak RSS |
|---|---|---:|---:|---:|---:|---:|
| **cram** | `--small` | 192.62 s | 0.4% | **1,660,873,216** | **0.5930** | 7858 MB |
| 7-Zip | `-mx=9` tuned | 111.94 s | 0.7% | 1,973,627,743 | 0.7047 | 21840 MB |
| RAR | `-m5 -s -md512m` | 104.42 s | 0.2% | 1,984,165,981 | 0.7085 | 3946 MB |
| RAR | `-m5 -s` | 86.78 s | 0.3% | 1,987,130,640 | 0.7095 | 598 MB |
| 7-Zip | `-mx=9 -mmt=24` | 100.34 s | 1.2% | 2,293,955,575 | 0.8191 | 21830 MB |
| 7-Zip | `-mx=9` own mt | 90.95 s | 0.4% | 2,293,955,575 | 0.8191 | 16998 MB |

**cram `--small` is 15.9% smaller than anything else in the field here**, and 27.6% smaller than
7-Zip `-mx=9` untuned. It costs **2.1× 7-Zip's time** at `-mx=9` with 7-Zip's own threading, and
1.7× against `-mx=9` tuned.

Two things in that table need saying plainly. **7-Zip's tuning matters far more than its `-mx`
level does on this corpus**: `-mx=9` at stock settings writes 2,293,955,575 bytes, 0.14% under its
own default, while the tuned invocation writes 1,973,627,743, which is 14.1% under the default. A
bigger `-mx` alone finds almost nothing on a corpus that is 51% incompressible media; a bigger
dictionary and word size find a great deal.

**And the previously published `7-Zip -mx=9 -mmt=24 — killed at 20 GB` row does not reproduce.** It
completed both repetitions on 16 August in 100.34 s at 21,830 MB, and the tuned run completed at
21,840 MB. The original kill was the harness's own doing: `tools/corpus/bench-corpus.sh` caps every
tool at a fixed `MEMCAP` of 20 GB on a 23 GiB machine, and this document never disclosed the cap
while publishing its effect as a 7-Zip failure. The cap is now documented in [Method](#method). It
remains true that this configuration has been killed on this machine under that cap; that is a
statement about the cap, not about 7-Zip.

### With every compressor switched off

| tool | mode | create | spread | archive | ratio |
|---|---|---:|---:|---:|---:|
| **cram** | `--store` | 2.58 s | 1.2% | **2,372,984,126** | **0.8473** |
| RAR | `-m0 -s` | 3.48 s | 1.7% | 2,806,286,454 | 1.0020 |
| 7-Zip | `-mx=0` | 4.93 s | 14.3% | 2,801,029,565 | 1.0002 |

**15.3% smaller than 7-Zip, 15.4% smaller than RAR**, with nothing compressed on any side. The whole
difference is deduplication, and it tracks this corpus's 15.0% duplicate content closely.

That closeness is a property of *this* corpus, whose duplicates were placed whole-file in one
directory. It is not what the mechanism does in general. On the kernel tree the same switch saves
2.09% where whole-file duplication accounts for only 0.20%, so nine tenths of what it found there
was sub-file; see [Corpora](#corpora). An earlier version of this document claimed the column
"would read 1.0000" on a corpus that does not repeat itself. On the kernel tree it reads 0.9791.

The 14.3% spread on the 7-Zip row is the widest of any create cell in this document; at 4.93 s the
absolute variation is a fraction of a second, which is why it is wide and not why it matters.

### `--fast`

| tool | mode | create | spread | archive | ratio | peak RSS |
|---|---|---:|---:|---:|---:|---:|
| **cram** | `--fast` | **2.45 s** | 1.2% | 2,013,900,705 | 0.7191 | 358 MB |
| 7-Zip | `-mx=5` | 68.25 s | 0.5% | 2,297,095,363 | 0.8202 | 7084 MB |

**27.9× faster than 7-Zip's default and still 12.3% smaller**, in 358 MB of RAM against 7.1 GB.

### Extraction

Superseded by [Decode](#decode), which measures every tool across all four corpora with the same
method. On this corpus it gives cram 2.39 s, zstd 2.21 s, 7-Zip 3.82 s, RAR 7.30 s, xz 18.19 s.

The August figures are kept here because the pair of columns is the clearest illustration in this
document of why the newer section exists:

| tool | to disk, `sync` included | to tmpfs |
|---|---|---|
| **cram** | 15.36 s | **2.58 s** |
| 7-Zip | 16.00 s | 3.64 s |
| RAR | 18.85 s | 7.25 s |

All nine disk extractions and all nine tmpfs extractions produced 42,151 files and 2,800,604,582
bytes; nothing was short.

**The claim previously made here, that the tmpfs column "repeated to within 3%", was false.** It
held only for the runs quoted, and the harness never warmed the archive before starting the clock,
so every one of those numbers included a read of a 1.9 GB archive from a SATA volume. Re-run under
the same unwarmed method, this corpus's rows spread **17–140%** between identical runs. The
16 August figures above warm the archive first and repeat to within 3.9%.

The disk column never repeated and was reported that way at the time: cram 8.69–18.54, 7-Zip
15.47–19.88, RAR 15.70–18.86, spreads that overlap completely. **On disk these three are
indistinguishable**, and anyone quoting a winner from that column is quoting noise.

## Reading archives other tools wrote

Everything above measures each tool on its *own* format. This measures the other thing people
actually do: open a `.7z` somebody else made.

**Measured 2026-08-13 and not re-run on 16 August.** Same machine, two builds: released 1.1.0 as the
baseline, and an unreleased build for the segment work described below. The corpus is on a different
volume of the same box. Do not compare these timings against the tables above; compare them against
the 7-Zip column beside them, which was re-run at the same time. For a sense of drift since: 7-Zip
extracting this corpus on 16 August took 3.82 s with a 5.6% spread, so the 3.68 s below sits inside
today's band. Three rounds with the tool order rotated, `/dev/shm` as the destination, every
extraction counted by file and byte and checked against the corpus `MANIFEST.sha256`: 42,151 files
each, all matching.

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

**On 7-Zip's own archive the two are level on time.** 3.26 against 3.68 is close enough that reading
a winner into it is reading noise. What is not noise is the memory: 7-Zip needs 4876 MB to reach that
time and cram needs 867 MB. On a cram-written `.7z` cram is 2.4× faster, at 303 MB against 173 MB.

**Most of that memory gap was closed in one line.** Until the declared LZMA2 dictionary size became
readable, the window had to be bounded by the segment's own length: always safe, since a segment
opens on a dictionary reset, and about four times too large, since 7-Zip writes 32 MiB dictionaries
into 128 MiB thread blocks. Asking the archive instead took peak RSS from 2809 MB to 867 MB on this
run, and 15% off the CPU with it, from the allocation that stopped happening. The declared value
comes from the archive, so it is used only to shrink the window and never to grow it.

**Why a single-folder archive is divisible at all.** 7-Zip's `-mx=5` default puts the whole 2.8 GB
in one solid block, which cram used to decode on one thread. Its multi-threaded encoder resets the
LZMA2 dictionary at each thread-block boundary, and a chunk with a dictionary reset can be decoded
cold. Walking the framing of that archive: 47,011 chunks, **21 dictionary resets**, segments of
110.9–128.0 MiB. Twenty-one places a decoder can start.

**Which archives split, exactly.** 7-Zip's multi-threaded LZMA2 encoder cuts its input into blocks
of **four times the dictionary size**, and each block opens with a dictionary reset. Measured by
varying the dictionary alone on identical input: 256 KiB gives 1.0 MiB segments, 4 MiB gives 16.0,
16 MiB gives 64.0, 32 MiB gives 128.0, 64 MiB gives 256.0. So:

> A `.7z` splits into `content ÷ (4 × dictionary)` segments, and only when it holds more than
> 4 × dictionary. At the `-mx=5` default that means **archives over 128 MiB, in 128 MiB pieces**.

7-Zip also shrinks the dictionary to fit a small input (16 MiB of input reports a 24 MiB dictionary),
so below that threshold one block covers everything and there is nothing to split whatever `-mmt`
says. Above a 64 MiB dictionary the 4× relation stops holding: a 256 MiB dictionary gave 256 MiB
segments, not 1 GiB. The default is well inside the range that was confirmed.

**What happens when an archive splits less, or not at all.** 1 GiB of the same data written three
ways, extracted to tmpfs on 24 threads, three rounds each, 13 August:

| written with | segments | cram | 7-Zip |
|---|---|---|---|
| `-mx=5`, stock | 9 | **1.71 s, 366 MB** | 1.77 s, 1917 MB |
| `-mx=9`, max | 5 | 3.09 s, 1096 MB | 2.45 s, 1916 MB |
| `-mmt=1` | 1 | 7.90 s, **64 MB** | 5.38 s, 127 MB |

Fewer seams means less to fan out over, and by `-mmt=1` there is nothing: one segment, one thread,
and 7-Zip's single-stream decoder is simply faster than ours. That is the honest floor of this
design, **cram is 47% slower there**, in a third of the memory.

Nowhere is cram the heavier of the two. That was not true earlier on 2026-08-13: both non-stock
cases used to fall out of the parallel path entirely and back onto the sequential reader, which cost
`-mmt=1` 10.62 s and 2477 MB, nineteen times 7-Zip's. Two gates were judging the wrong quantity. One
refused any block too large to cache, months after `copy_unit` made caching unnecessary; the other
charged an archive's segments for every core rather than for the workers it could use.

`-mx=1` is the opposite extreme: a 256 KiB dictionary gives 1 MiB segments and over a thousand units,
and it extracts fastest of all at 0.71 s in 235 MB.

Extraction to a real disk is not reported here. On this machine it is write-bound and the four
columns do not separate: repeated runs of the same command ranged 9.85–26.68 CPU-seconds and
16.33–30.32 s wall, which supports no claim in either direction.

## Decode

Measured 16 August. Every tool at its own default, **destination `/dev/shm`**, **the archive read
into the page cache before the clock starts**, warm-up discarded, median of 3 with the tool order
rotated each round. **All 60 extractions were counted by file and by byte and `diff -rq`'d against
the corpus, and all 60 verified.**

**Why a RAM disk, and why this column is called decode rather than extract.** On this machine the
drive sustains about 84 MiB/s, so extracting the kernel tree to it puts every tool within 23% of
every other, which describes a SATA disk in a KVM guest rather than an archiver. Removing the write
wall leaves decode plus the tool's own overhead, which is the part that differs between these
programs.

**Warming the archive is new on 16 August and it is what made this table repeat.** The harness used
to start the clock on a cold archive, so each row also timed a read of up to 2.2 GB from the SATA
volume; on the Cram corpus identical runs spread 17–140%. Warmed, the worst spread below is 13.3%
and eighteen of twenty cells are under 6%.

| corpus | tool | decode | spread | peak RSS | archive MiB |
|---|---|---:|---:|---:|---:|
| Silesia | zstd | **0.18 s** | 5.9% | 6 MB | 64 |
| Silesia | **cram** | 0.22 s | 4.8% | 475 MB | 56 |
| Silesia | RAR | 0.50 s | 8.0% | 44 MB | 52 |
| Silesia | 7-Zip | 0.88 s | 1.1% | 255 MB | 47 |
| Silesia | xz | 2.01 s | 1.0% | 10 MB | 47 |
| enwik9 | zstd | **0.89 s** | 3.4% | 6 MB | 298 |
| enwik9 | **cram** | 1.20 s | 3.4% | 1245 MB | 255 |
| enwik9 | 7-Zip | 1.69 s | 3.7% | 1176 MB | 214 |
| enwik9 | RAR | 2.52 s | 4.5% | 44 MB | 238 |
| enwik9 | xz | 8.20 s | 1.2% | 10 MB | 223 |
| kernel | **cram** | **1.75 s** | 13.3% | 1183 MB | 471 |
| kernel | zstd | 2.15 s | 2.8% | 6 MB | 515 |
| kernel | 7-Zip | 4.35 s | 0.0% | 2284 MB | 431 |
| kernel | RAR | 7.29 s | 1.8% | 44 MB | 468 |
| kernel | xz | 8.93 s | 0.6% | 10 MB | 437 |
| corpus | zstd | **2.21 s** | 1.8% | 6 MB | 2253 |
| corpus | **cram** | 2.39 s | 3.9% | 1271 MB | 1897 |
| corpus | 7-Zip | 3.82 s | 5.6% | 4877 MB | 2191 |
| corpus | RAR | 7.30 s | 0.7% | 44 MB | 1898 |
| corpus | xz | 18.19 s | 0.7% | 10 MB | 2213 |

**Against the tools in its class cram wins every cell.** Over 7-Zip: 4.0× on Silesia, 1.4× on
enwik9, 2.5× on the kernel tree, 1.6× on the Cram corpus. Over RAR: 2.3×, 2.1×, 4.2×, 3.1×. Over xz,
between 5.1× and 9.1×.

**zstd `-3` decodes faster than cram on three of the four**, and that belongs here rather than in a
footnote. It is a far weaker compressor and its archive is the largest in every table above, so
there is less to decode. cram takes it only on the kernel tree and loses the other three.

**The memory column is a range, not a headline, and it reverses at the small end.** Against 7-Zip:
1271 MB against 4877 on the Cram corpus, 3.8× lighter; 1183 against 2284 on the kernel tree, 1.9×
lighter; **475 against 255 on Silesia and 1245 against 1176 on enwik9, where cram is the heavier of
the two.** Against the pipe-based tools it is much heavier everywhere: zstd and xz sit at 6–10 MB
because a tar pipe holds nothing at all, and RAR at 44 MB. "Cheaper on memory" is true of 7-Zip on
the two large corpora and of nothing else in this table.

**Extracting to a real disk is slower for everyone and the tools converge**: the kernel tree takes
cram 35.94 s there against 7-Zip's 44.12, a 1.23× spread rather than the 2.5× above (measured
14 August). If you are choosing a tool because extraction speed matters and you are writing to a
spinning or SATA disk, the honest answer is that it will barely matter.

## Writing `.tar.gz`

`.tar.gz` is its own comparison because the competitor is not an archiver. **The competitor is
`pigz`**, parallel gzip, by Mark Adler, who co-authored zlib and the gzip format, packaged in every
distro since 2007. Same machine and method as the Decode table: kernel tree, destination
`/dev/shm`, warm-up discarded, median of 3 with the order rotated, every tool given all 24 threads.

| kernel tree, 1,920,837,858 bytes | wall | bytes | peak RSS |
|---|---:|---:|---:|
| **cram**, default | **3.28 s** | **558,402,429** | 233 MB |
| `pigz -6 -p 24` | 3.50 s | 566,208,712 | 19 MB |
| `gzip -6` (one thread) | 30.92 s | 566,354,268 | 3 MB |

**1.07× faster than pigz, in a 1.38% smaller archive.** The `gzip` row is there for scale, not as a
comparison: an earlier version of this section reported 5.9–9.6× against `tar czf`, which was true,
uninteresting and quietly misleading, because `tar czf` compresses on one thread. Anyone qualified
to read this table knows pigz exists.

The archive is smaller than both because the chunk is 1 MiB where pigz's is 128 KiB, so cram throws
away eight times less dictionary at the seams.

**Extraction is still single-threaded, and is faster anyway.** A standard `.gz` cannot be extracted
in parallel by anyone, cram included: a decoder cannot find the block boundaries without inflating
everything before them, and pigz decompresses in the same time as gzip for that reason. **`cram x`
on a `.tar.gz` is 2.63 s against `gzip -dc | tar`'s 6.13** on the kernel tree, 2.33× faster, on one
decode thread each. It was 2.26× *slower* until 2026-08-15; what changed was ours, not the format's.

`.tar.xz` and `.tar.bz2` are the exception, and for a reason specific to how cram writes them: both
are emitted as a run of complete standalone streams, and a run of streams **can** be split without
inflating anything before the split. Those two decode on every core, which is why `.tar.xz` reads at
2.85 s against `xz -dc -T0 | tar`'s 8.91, **3.13× faster**, rather than 2.32× slower. Every tar
codec is in [Tar extraction](#tar-extraction).

**What it costs.** The stream is cut into 1 MiB chunks compressed independently, so each starts with
an empty dictionary and the archive grows by 0.19–0.34% against a single-stream gzip. Peak memory is
the window, 233 MB on the kernel tree against 3 MB for `gzip -6`, and CPU rises 30–39% for the
wall-clock. On one core the chunked writer is neither faster nor slower than the streaming one
(6.12 s against 6.28 s on Silesia, measured 6 August and not re-run), so the CPU is the price of
concurrency rather than of chunking, and a machine with nothing to parallelise over does not pay it.

The output does not depend on how many cores wrote it: chunk boundaries are byte offsets in the tar
stream, so a 1-thread and a 24-thread run produce the same archive to the byte.

## Tar extraction

Kernel tree, `/dev/shm`, archives written by cram, every competitor re-measured in the same session
on the same binary (2026-08-16), medians of 3 with the warm-up discarded, spreads 0.7–4.7%, all 45
extractions verified.

| codec | cram | native | | RSS cram / native | archive bytes |
|---|---:|---:|---|---|---:|
| `xz` | **2.85 s** | 8.91 s (`xz -dc -T0`) | **3.13× faster** | 368 / 10 MB | 454,397,720 |
| `gz` | **2.63 s** | 6.13 s (`gzip -dc`) | **2.33× faster** | 199 / 3 MB | 558,402,429 |
| `lz4` | **1.22 s** | 2.04 s (`lz4 -dc`) | **1.67× faster** | 175 / 3 MB | 763,711,608 |
| plain `.tar` | **1.19 s** | 1.69 s (GNU tar) | **1.42× faster** | 251 / 3 MB | 1,997,117,440 |
| `zst` | **1.53 s** | 2.14 s (`zstd -dc`) | **1.40× faster** | 200 / 6 MB | 534,639,379 |
| `br` | **2.91 s** | 3.71 s (`brotli -dc`) | **1.27× faster** | 193 / 7 MB | 487,982,888 |
| `bz2` | 3.14 s | 3.24 s (`lbzip2 -dc`) | level | 537 / 537 MB | 497,686,838 |

**`.tar.bz2` is level and is claimed as neither a win nor a loss.** It was the one loss in this table
and is not any more, but it is not a win either. Over five rounds on the same archive: cram median
**3.16 s** (min 3.13, max 3.16, spread 1.0%), lbzip2 median **3.22 s** (min 3.20, max 3.27, spread
2.2%). cram is 1.9% faster and the largest spread is 2.2%, so the gap is inside the noise.

Every row here was 2.2–5.6× behind two days before this run, and `bz2` was 19.1× behind. Five fixes,
none of them in a codec. The tar worker allocated and zeroed a 1 MiB buffer for **every entry**,
94,778 of them on this tree, to carry files averaging 20 KB, and passed results over a one-slot
channel, which is a ping-pong rather than a pipeline. The concatenated streams cram's chunked writer
emits, which had been walked one at a time, started being decoded on a pool. The extraction path was
asking the filesystem the same questions twice per file. Writing those files ran on one thread while
decoding ran on another, so the machine's other cores did nothing.

**And a compressed tar was decoded twice.** A tar's headers are interleaved with its bodies, so
building the member list means decompressing everything, and then extraction decompressed it all
again. The list is now built only when something asks for it: `cram l` pays for one pass, and an
extraction, which is about to stream every entry anyway, pays for none. It was contributing nothing
to the plan in any case, since `block_count` returns 1 for the container and `plan_codec` reads only
the codec. **That single change is worth 1.9–2.0× on every row above.**

**The plain `.tar` row is the one that explains the rest.** Extracting a plain `.tar`, no codec and
nothing to decode, took 3.38 s against GNU tar's 1.73, so the gap was entirely ours. `strace` counted
**1.83 million syscalls against GNU tar's 0.79 million**: a `mkdir` per file that failed `EEXIST`
94,779 times where GNU tar issued 6,214 that succeeded, two `statx` per file to ask about paths an
`openat` was about to answer, a second `openat` per file because the mtime was set by path rather
than on the handle already open, and 526,938 `read` calls for 2 GB because a plain `.tar` was handed
an unbuffered file. Remembering which directories exist, creating with `create_new`, stamping the
descriptor and buffering the source took it to 2.26 s and every compressed codec down with it.

**Writing them across a pool took it past GNU tar**, to the 1.19 s in the table. Decoding a tar is
one pass and has to stay one thread; writing what it decodes does not. Small entries accumulate into
a bounded batch and go out on **eight writers, which is the shipped cap** (`hw.physical.clamp(1, 8)`).
The width knees at eight and then goes backwards: 1.43 s at eight against 1.50 s and 311% CPU at
twenty-four. That sweep predates the current decode path and was not re-run on 16 August, so treat
it as the reason for the cap rather than as a current figure; the cap itself is 8 and a second,
older sweep in the tree that reads faster at sixteen is stale. `gz` and `br` are the two rows that
pay about 3% for the pool rather than gaining, because their decode is single-threaded and is the
wall, so extra writers only contend.

**On `bz2`, what remains is the decoder's single-thread speed.** The figures in the next two
paragraphs were measured 15 August, when `bz2` was still a loss, and were not re-run on 16 August;
they explain the row rather than time it. Both sides scale about as well, 92% efficiency at four
threads for us against lbzip2's 94%, so the difference is what each thread achieves. On the same
archive single-threaded, `lbzip2 -dc -n 1` takes 24.80 s where our decoder takes 31.59 s and
`bunzip2 -c` takes 33.26: lbzip2 ships its own decompressor and it is about 1.3× the reference
implementation's. Ours is the reference implementation.

**It is specifically not the codec library**, which was the obvious suspect and was measured rather
than assumed: decoding an identical 400 MB stream, the pure-Rust `libbz2-rs-sys` takes 8.32 s and the
C `bzip2-sys` 8.55 s, so the Rust one is marginally the faster of the two and both beat the `bunzip2`
CLI's 9.03 s. Swapping the backend to C would gain nothing. The same holds for brotli: the Rust crate
decodes a stream in 0.55 s against the C CLI's 0.48 s.

**The `xz` and `bz2` rows depend on the archive being a run of streams, and say so.** Those two
decode on every core by splitting at the seams cram's own writer leaves; `pbzip2`, `lbzip2` and
`cat a.xz b.xz` leave the same ones. An archive that is a **single** stream, which is what plain
`bzip2` or `xz` produces, has nothing to split, and cram reads it at one-core speed like everyone
else: the same kernel tree through a single-stream `.tar.bz2` from stock `bzip2 -9` takes tens of
seconds, not 3.14. Detecting that costs nothing measurable, 62.4 s against 62.5 s with the seam scan
disabled outright (measured 15 August, before the double decode was removed), but the speed is not
there to be had.

For scale: the same tree out of a `.cram` decodes in 1.75 s.

## `.zip`, against 7-Zip and Info-ZIP

`.zip` is the format people actually exchange, so it gets both of its reference implementations:
7-Zip's ZIP encoder (`-tzip`, not its own `.7z`) and **Info-ZIP** `zip 3.0` / `unzip 6.00`, which is
the reference implementation and is on every machine. Measured 16 August, destination `/dev/shm`,
medians of 3, order rotated each round, every tool given all 24 threads it can use.

**Creating:**

| | cram | 7-Zip `-tzip -mmt=24` | Info-ZIP `zip -r` |
|---|---:|---:|---:|
| Silesia | **3.01 s** / 68,099,749 | 3.52 s / 65,636,076 | 5.38 s / 68,230,075 |
| kernel tree | **3.08 s** / 615,233,804 | 13.61 s / 611,862,121 | 28.37 s / 624,029,389 |

**Extracting.** All three readers open *the archive cram wrote*, so the only variable is the reader.
It also proves interop in the same pass: every extraction produced the corpus byte for byte,
211,938,580 and 1,920,837,858 bytes.

| | cram | 7-Zip | Info-ZIP |
|---|---:|---:|---:|
| Silesia | **0.12 s**, 36 MB | 0.67 s, 6.6 MB | 0.89 s, 3.6 MB |
| kernel tree | **1.89 s**, 150 MB | 7.21 s, 31 MB | 9.65 s, 4.9 MB |

**Extraction is 3.81× 7-Zip and 5.11× Info-ZIP on the kernel tree**, 5.58× and 7.42× on Silesia. The
kernel figure repeated at 1.89 s in all three rounds.

The reason is in the CPU column rather than in any cleverness: cram runs at 272–409% CPU and both
competitors at 99–100%. Neither extracts a `.zip` on more than one core, though every entry in the
format is independently addressable and always has been. What it costs is memory, 150 MB against
7-Zip's 31 and Info-ZIP's 4.9.

**On create the archive is larger, and an earlier version of this comparison said otherwise.** cram
writes 0.55% more than 7-Zip on the kernel tree and 3.75% more on Silesia. The earlier run put cram
smaller than both, and that was an artefact: it used the kernel tree *with* its 99 symlinks, which
cram skips and reports while 7-Zip dereferences, silently storing 8,011 duplicate files and
inflating its own archive. On the symlink-free tree every tool sees the same file set and 7-Zip's
`.zip` is honestly the smaller one.

Silesia's cram extraction ran 0.36 / 0.12 / 0.12. The harness warms create and not extract, so the
first round is the warm-up; the median is sound and the 200% spread is the harness, not the tool.

## Where cram loses

- **Creating a `.tar.bz2` and a `.tar.xz`**, against the threaded specialists: bz2 7.85 s against
  lbzip2's 4.99, xz 41.35 s against `xz -T0`'s 34.45. Both write a smaller or equal archive (0.88%
  smaller on xz), and both improved substantially on 2026-08-15, but neither wins. **Measured
  15 August and not re-run on 16 August.**
- **`.zip` size, against 7-Zip's ZIP encoder.** 0.55% larger on the kernel tree and 3.75% larger on
  Silesia, at 4.42× and 1.17× the speed. Against Info-ZIP cram is smaller on both.
- **Maximum ratio on the three corpora without duplicate content.** cram `--small` is 1.85% larger than
  7-Zip `-mx=9` on the kernel tree (quote that row with its 8.4% spread), 5.09% larger than 7-Zip
  `-mx=9` tuned on enwik9 and 0.22% larger than the same setting on Silesia. On the kernel tree
  cram `--small` is dominated outright, bigger and slower. Use `--auto` there and take the speed.
- **Memory at maximum, on Silesia**: 2180 MB against 7-Zip `-mx=9` tuned's 2088 MB, cram 4.4%
  heavier. On the kernel tree the position reverses, 11.0 GB against 18.9 GB.
- **Memory on decode, on the two small corpora**: 475 MB against 7-Zip's 255 on Silesia and 1245
  against 1176 on enwik9. Against zstd, xz and RAR, which decode through a tar pipe, cram is heavier
  on all four corpora by one to two orders of magnitude.
- **Decode against zstd, on three corpora of four**: 0.22 s against 0.18 on Silesia, 1.20 against
  0.89 on enwik9, 2.39 against 2.21 on the Cram corpus. cram takes only the kernel tree, 1.75 s
  against 2.15.
- **Small archives, on creation.** cram does not parallelise *within* a pack, so a corpus smaller
  than a few packs compresses single-threaded while 7-Zip splits it across every core. On a 20 MB
  input, cram at maximum took 15.89 s against 7-Zip `-mx=9`'s 4.55 s. **Measured 6 August and not
  re-run.**
- **`--small` is a bad trade on a media-heavy corpus.** On the Cram corpus it is 16.5% smaller than
  `--auto` for **27.7× the time**, and it holds 7.9 GB while doing it. Most of that corpus is
  already-compressed photographs and video, where a wider window and a longer match search have
  almost nothing to find. Reach for it on text and source, not on a photo library.

**Single-file archives on extraction used to be here and no longer are.** The parallel path's unit
of work was the entry, so one entry meant one thread whatever the machine. A `.cram` entry can now
be cut at its pack boundaries and its pieces decoded concurrently. Measured on the 24-thread Linux
box, enwik9 to tmpfs, three rounds, output byte-identical to the original 1,000,000,000-byte file,
**14 August, before the warmed-archive method**:

| | wall | effective cores | peak |
|---|---|---|---|
| before | 9.05 s | 1.0 | 568 MB |
| after | **2.06 s** | 5.4 | 900 MB |
| 7-Zip | 1.64 s | 4.4 | 1176 MB |

Under the 16 August method the same corpus decodes in 1.20 s against 7-Zip's 1.69, so cram is now
1.4× ahead rather than 26% behind. It remains behind zstd's 0.89 s, and its 1245 MB is above
7-Zip's 1176.

## What these corpora do not measure

**Nothing here measures a corpus larger than memory.** Every table above fits in this machine's
23 GiB page cache, so no result is bounded by re-reading source data from disk. A 200 GB backup is a
different measurement and it has not been done.

**Nothing here measures a spinning disk**, and extraction is write-bound: the Cram corpus decodes in
2.39 s and took about 15 s to write to NVMe when that was last measured, on 6 August. On a
120 MB/s HDD every tool in this document would be pinned to the disk and the decode column would
stop mattering at all.

**Whether extraction is write-bound at all is a property of your drive, not of any archiver here.**
Measured 2026-08-07 with `calibrate --recalibrate --write-probe` on two machines, one sample each:
the benchmark box decodes DEFLATE at 948 MiB/s on one core against an 84 MiB/s sustained write wall,
a ratio of 11.3, so a single worker already outruns the disk by an order of magnitude. A desktop
NVMe in the same room decodes at 674 MiB/s against a 757 MiB/s wall, a ratio of 0.89, where one
decoding thread does not saturate the drive and a second has real work to do. The same tool is
write-bound on one and roughly balanced on the other, which is why the engine measures rather than
assuming. Run that command to find out which regime you are in; do not assume this table's answer is
yours.

**Both drives stepped down at ~2 GiB written**, from 349 to 84 MiB/s and from 1218 to 757 MiB/s, as
the SLC cache filled. Any extraction benchmark whose output fits under that is measuring cache
rather than disk. The decode runs above go to tmpfs and avoid the question entirely; the create runs
write ~2 GB and land on the knee.

**Nothing here measures Windows**, which is the platform Cram is built for first. These are Linux
numbers on one machine, and the Windows file-open path is measurably different; see
[`docs/PERFORMANCE_FINDINGS.md`](docs/PERFORMANCE_FINDINGS.md) §7, where `File::open` dominates
create on Windows and is nearly free on Linux.

**The Cram corpus dedup figures depend entirely on one assumption**, which is that 15% of a working
drive repeats itself. That number is stated, is confined to a deletable directory, and is the input
every Cram corpus deduplication claim rests on. Silesia and enwik9 repeat nothing at all, and on
those two cram's size advantage disappears completely, which is what their tables show. The kernel
tree is the interesting middle case and is measured rather than assumed: 0.20% whole-file,
2.09% at chunk level.

## Determinism

An unencrypted `.cram` is byte-for-byte reproducible: the same inputs give the same archive on any
machine, at any thread count, with any amount of RAM. Pinned by
`crates/cram-core/tests/reproducible.rs`, `batch_invariance.rs` and `chunk_lanes.rs`, the last of
which builds the same tree at 1, 2, 4 and 16 chunk workers and compares the bytes.

Held on real data too: every Cram corpus figure above was produced at a lane count the machine chose
for itself, and a sweep from 1 to 24 chunk lanes over three effort levels produced the same archive
bytes every time, 1,989,536,373 at `--auto` and 2,372,984,126 at `--store`, across 24 runs per level
and across the commit that introduced the lane pool. Both figures are the ones in the tables above,
measured eleven days later on a different build.

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
# MEMCAP defaults to 20G and is a fixed cap, not the machine's RAM. This box has 23 GiB,
# and 7-Zip -mx=9 -mmt=24 peaks at 21,830 MB on the Cram corpus.
MEMCAP=24G tools/corpus/bench-corpus.sh ./cram-corpus-1.0 /tmp/bench 3
```

The other three corpora, and one configuration of each tool as run:

```sh
curl -LO https://sun.aei.polsl.pl//~sdeor/corpus/silesia.zip
curl -LO https://mattmahoney.net/dc/enwik9.zip

cram a OUT.cram <inputs> --auto -y
7zz  a -mmt=24 -mx=9 OUT.7z <inputs>
rar  a -mt24 -m3 -r -y OUT.rar <inputs>        # RAR's default, not solid
rar  a -mt24 -m5 -s -r -y OUT.rar <inputs>     # solid, ratio-matched to 7-Zip
tar -cf - <inputs> | xz -9e -T0 > OUT.tar.xz
tar -cf - <inputs> | zstd -19 --long=27 -T0 -o OUT.tar.zst
```

Peak RSS comes from `/usr/bin/time -f '%M'` around each command. The build is
`cargo build --release --features download,zstd-c,phash,mimalloc`; `cram --version` prints the
feature list and belongs in any log you keep.
