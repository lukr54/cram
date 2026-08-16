# Changelog

All notable changes to Cram are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

**Ships as 1.2.0.** The manifests are already at 1.2.0; this heading and its link reference get the
version and the date when the tag is cut.

`cram-core`'s Rust API did break in this cycle — `hw::load_profile` and `hw::save_profile` changed
signature and `hw::Wall` is new — and an earlier reading of the workspace policy would have made
that 2.0.0. The policy was narrowed instead (see the comment in the workspace `Cargo.toml`): the
version number tracks the `cram` command line and the `.cram` format, not the Rust API of crates
that are on crates.io only because publishing the CLI requires it. No flag was removed or
repurposed and no archive an older reader could open has become unreadable, so no CLI user faces a
break. Anyone depending on those crates directly should pin an exact version.

Two themes. Reading a `.tar.*` was the largest weakness in the project, and it is now faster than
the tool everyone already has on six of seven codecs and level on the seventh. And the containers
that still wrote an archive on one core, `.zip` and `.tar.xz` and `.tar.bz2` and `.tar.zst` and a
`.7z` holding one large file, now use the machine.

**Where the figures come from.** Anything below dated 16 August 2026 is from one canonical run:
`cram 1.1.0`, commit `a84d77d`, features `download,zstd-c,phash,mimalloc`, on a Ryzen 9 5900X with
24 threads and 23 GiB of RAM under Ubuntu 24.04.4, decoding to `/dev/shm` with the archive read into
page cache first. Every other figure was measured when the change landed, between 12 and 16 August
2026, on the same machine but not under that method, and carries its date where it appears. One
machine and one afternoon either way.

**Reading a `.tar.*`.** Linux kernel tree, 94,778 files, 1,920,837,858 bytes, extracted to
`/dev/shm` with the archive warmed; medians of 3 with the warm-up discarded, spreads 0.7–4.7%, every
extraction `diff -rq`'d against the source. Measured 16 August 2026.

| codec | cram | native | standing | archive bytes |
|---|---:|---|---|---:|
| plain `.tar` | **1.19 s** | GNU tar 1.69 s | **1.42x faster** | 1,997,117,440 |
| `.tar.lz4` | **1.22 s** | `lz4 -dc` 2.04 s | **1.67x faster** | 763,711,608 |
| `.tar.zst` | **1.53 s** | `zstd -dc` 2.14 s | **1.40x faster** | 534,639,379 |
| `.tar.gz` | **2.63 s** | `gzip -dc` 6.13 s | **2.33x faster** | 558,402,429 |
| `.tar.xz` | **2.85 s** | `xz -dc -T0` 8.91 s | **3.13x faster** | 454,397,720 |
| `.tar.br` | **2.91 s** | `brotli -dc` 3.71 s | **1.27x faster** | 487,982,888 |
| `.tar.bz2` | 3.14 s | `lbzip2 -dc` 3.24 s | level | 497,686,838 |

`.tar.bz2` was run five more times on its own to settle which side of level it falls on. cram's
median is 3.16 s over a 1.0% spread against lbzip2's 3.22 s over 2.2%, so the 1.9% gap is inside the
noise: not a win, and not a loss either. Extraction to a real disk is write-bound and the tools
converge, so these figures say what a decoder can do rather than what a drive will.

### Added

- **`--tiny`**, a rung below `--small` that reaches for a slower encoder where one exists. Today
  that is one thing: a `.zip` is written with zopfli instead of the usual DEFLATE encoder, and the
  output is an ordinary DEFLATE stream that every unzip already reads. Silesia: **64,712,418 bytes
  against `--small`'s 67,799,807, 4.55% off, in 316 s against 5.5**, so about sixty times the wall
  time. A separate rung rather than part of `--small` because the trade is different in kind, and
  folding it in would turn a flag people use into one they would avoid. No other container here has
  a slower encoder to reach for. Measured 15 August 2026.

- `CRAM_PROFILE=1` prints the extraction plan and every input to it — bottleneck, workers, decode
  units, measured decode rate and write wall — so "why did this run on two threads" is one line
  rather than an afternoon of bisecting. It now also prints the walk, the store-versus-compress
  probe and the zip writer's own wait/parse/copy split, for every backend rather than only `.cram`.

- **`--no-solid`** (7z) writes one independently-decodable pack per entry instead of packing members
  together: a much larger archive, and cheaper to read one member out of. Solid remains the default
  and `--solid` states it explicitly. It was previously reachable only through an environment
  variable, while `CreateOptions::solid` said `false` and the writer ignored it and made every
  archive solid regardless.

- **mimalloc**, behind a feature and on in the shipped binary. Worth **1.22x on extraction** for 13%
  more memory. Not worth what the note claimed: on zip create it is 1.08x for 2.7x the memory, and
  on `.cram` create it is nothing at all. Measured 14 August 2026. The corpus was not recorded and
  none of the three ratios was re-run on 16 August, so carry them forward with that attached.

- `CRAM_WORKERS=n` forces the pool width, so a benchmark can ask what the plan is worth. There was
  no way to: `taskset` narrows which CPUs the process may use without narrowing the core count the
  planner sees, so it still asks for every worker and simply gets them descheduled, which measures
  contention rather than the count. Deliberately not a CLI flag, and it prints a line of its own
  when in effect so it cannot be quietly set during a measurement.

- **A regression table for the planner.** Adaptive parallelism is the thesis everything here rests
  on and nothing asserted it as a whole; the plan flipped between 8 and 24 workers twice in one day
  and both times a human caught it reading `CRAM_PROFILE` output. Seven named scenarios across three
  machines that have been measured, each asserting a whole plan and carrying the reason it is that
  answer, so a change has to state what it meant to change.

### Changed

- **A compressed tar is no longer decoded twice to extract it once.** A tar's headers are
  interleaved with its bodies, so enumerating its members means decompressing the whole archive, and
  extraction then decompressed it all over again. It showed as an exact factor of two on every
  codec: listing a compressed tar cost a full decompression pass, and testing it cost two. The list
  was feeding nothing — `block_count`
  returns 1 for a tar whatever is in it, and `plan_codec` reads only the codec — so it is now built
  only when something asks for it. `cram l` pays for one pass, and an extraction that is about to
  stream every entry anyway pays for none. `ArchiveReader::entries_are_cheap` says which backends
  can answer cheaply; the trait's own doc had claimed all of them could. The extraction figures this
  produced are in the table at the top of this section, measured 16 August 2026.

- **The tar worker stopped allocating a megabyte per entry.** It allocated and zeroed one for every
  entry, 94,778 of them on the kernel tree, to carry files averaging 20 KB, and passed the result
  over a one-slot channel, so the decoder could never get more than one message ahead of whoever was
  writing the files. As a stage, on the kernel tree: `.tar.gz` 13.99 s → 5.58; `.tar.zst`
  11.94 → 3.93; `.tar.lz4` 10.95 → 3.66; `.tar.br` 15.77 → 6.05. Writing the 94,778 files was never
  the cost, at 0.32 s, and neither was inflate. Measured 15 August 2026.

- **A `.tar.bz2` or `.tar.xz` decodes on every core.** Compressing on every core means cutting the
  tar into chunks and writing each as a complete standalone stream, and cram had been writing those
  seams for months and then reading them back one at a time. They are findable: a bzip2 stream
  begins with a header the previous stream's end-of-stream magic sits in front of, and an xz stream
  with a header whose CRC checks out behind a `YZ` footer. Cram scans for them, decodes the spans
  between them on a pool, and yields the bytes in order. As a stage, on the kernel tree: `.tar.xz`
  20.79 s → 6.61; `.tar.bz2` 62.08 → 7.46. Measured 15 August 2026.

  This works on any archive that is a run of concatenated streams, not only cram's own: `pbzip2` and
  `lbzip2` output, Wikipedia multistream dumps, `cat a.xz b.xz`. A single-stream archive falls back
  to the sequential decoder unchanged, and nothing about what cram *writes* changed. A false seam
  cannot corrupt an extraction silently, since the spans either side of it fail to decode. Bare
  `.xz` and `.bz2` files take the same path. `CRAM_PARALLEL_DECODE=0` turns it off.

- **Extraction stopped asking the filesystem the same question twice per file.** A plain `.tar`,
  with no codec and nothing to decode, took 3.38 s against GNU tar's 1.73 in the same run, so none
  of that gap was compression. `strace` counted **1.83 million syscalls against GNU tar's 0.79
  million**, and most of the excess was work whose answer cram already had: a `mkdir` for every
  *file* rather than every directory, which failed `EEXIST` 94,779 times on the kernel tree; two
  `statx` per file asking about paths the following `openat` was about to answer; a second `openat`
  per file because the modification time was stamped by path instead of on the descriptor still
  open; and 526,938 `read` calls to move 2 GB, because a plain `.tar` was handed an unbuffered file
  and the tar parser reads 512-byte headers. Measured 15 August 2026.

- **A tar's files are written on every core.** Decoding one is a single pass and has to stay one
  thread; writing what it decodes does not, and both were running on one. Small entries now
  accumulate into a bounded batch, 32 MiB or 4,096 entries, whichever comes first, and go out across
  a pool of writers; entries over 4 MiB still stream inline. The width knees at eight and then goes
  backwards. Swept on one binary, a plain `.tar` took 2.26 s at one writer, 1.52 at four, **1.43 at
  eight**, 1.46 at twelve and 1.50 at twenty-four, where it burned 311% CPU to be slower, so **the
  shipped cap is eight**. A second sweep of the same knob on the same corpus recorded 1.52 at eight
  and 1.44 at sixteen; the two disagree, one of them is older, and neither is guidance for raising
  the cap until they are re-run together. `gz` and `br` pay about 3% for the pool rather than
  gaining, because both decode on a single thread and that thread is the wall, so extra writers only
  contend for page allocation. The batch and the pool cost about 100 MB: peak RSS on a plain `.tar`
  is 251 MB on 16 August against GNU tar's 3 MB, and the codec rows run 175 to 537 MB against native
  tools holding 3 to 10 — `lbzip2` alone matches cram, at 537 MB either side. The sweep
  above is a width comparison on one binary rather than a standing; the standings are in the table
  at the top. Measured 16 August 2026.

  The five entries above are stages of one piece of work. Every codec moved on all five, because
  the work is in the shared engine, and extracted trees are byte-identical to the source on all
  seven with modification times unchanged.

- **A `.zip` is created on every core.** cram wrote every non-native container on a single core: a
  `.zip` averaged 1.00 effective cores against `.cram`'s 8.69 on the same tree, because the engine's
  create loop streams each file into the writer in turn and only `.cram` escaped it. Workers now
  build a complete one-entry zip in memory and the writer thread copies the already-compressed bytes
  in with `raw_copy_file`, in submission order, which keeps every header field the zip crate's
  business rather than growing a second encoder that can disagree with the first. Output is
  byte-identical to the sequential writer, and that is what the tests assert. **14,386 entries:
  27.20 s → 7.96.**

  A deeper queue then kept the pool fed through a slow entry, since entry durations on a real tree
  span three orders of magnitude and a queue only twice the pool size leaves every worker idle while
  the writer waits on one big file. On a 41,305-file tree at 16 threads, depth 32 gave 11.24 s and
  depth 2048 gave **6.68 s**, at 222 MB peak RSS; depth is now 2048 and the in-flight byte ceiling
  does the memory bounding. Encrypted archives joined once a round-trip test proved WinZip-AES
  framing survives `raw_copy_file`: the kernel tree at AES-256, 94,778 files, went from **72.83 s at
  0.9 effective cores to 5.68 s at 16.8**, so 12.8x, with the two archives identical in size and
  7-Zip accepting both.

  Two escape hatches ship with it. `CRAM_ZIP_SEQUENTIAL` restores the old single-threaded writer for
  one run, and `CRAM_ZIP_DEPTH` re-finds the queue knee on hardware that is not this machine.
  Measured 12–15 August 2026.

  Against the two reference implementations on 16 August, kernel tree, `/dev/shm`, medians of 3:
  creating is **4.42× 7-Zip's ZIP encoder and 9.21× Info-ZIP**, for an archive 0.55% larger than
  7-Zip's and 1.41% smaller than Info-ZIP's. Reading the same archive back is **3.81× and 5.11×**,
  because neither of them extracts a `.zip` on more than one core; cram runs that at 272–409% CPU
  against their 99–100%, and holds 150 MB against 31 and 4.9. Full table in `BENCHMARKS.md`.

- **`.tar.gz` is created on every core.** gzip is one stream with no boundaries a writer has to
  respect, so cram wrote it through a single encoder and used one thread. The tar stream is now cut
  into 1 MiB chunks, each deflated by its own compressor and ended with a sync flush so it stops on
  a byte boundary, and the chunks are concatenated; the result is one ordinary gzip member that
  `gzip -t`, `zcat` and `tar -xzf` read as usual. Workers take the next chunk the moment they finish
  one and a writer thread emits in index order, so a straggler cannot idle the pool. The gzip
  trailer's CRC is folded in that same in-order pass, because `Crc::combine` is associative but not
  commutative and folding chunks as they finished would make the checksum depend on thread
  scheduling.

  Kernel tree, 16 August 2026: **3.28 s for 558,402,429 bytes**, against `pigz -6 -p 24` at 3.50 s
  for 566,208,712. That is 1.07x faster in an archive 1.38% smaller. `gzip -6` takes 30.92 s for
  566,354,268; it compresses on one thread, so it is scale rather than a comparison. The cost is
  memory: **233 MB peak RSS against pigz's 19 MB**, and 0.19–0.34% in size
  against cram's own single-threaded output (measured 14 August), because each chunk starts with an
  empty dictionary. **Only create is parallel.** A standard `.gz` cannot be extracted in parallel by
  anyone, this included, because a decoder cannot find the block boundaries without inflating
  everything before them.

- **A `.tar.xz` and a `.tar.bz2` are created on every core as well.** The chunking that gzip got
  stopped there, so `.tar.xz` built the kernel tree in 442.63 s at 1.0 effective cores while
  `xz -T0` took 34.45 at 6.0. The compressor was never the problem; it simply used one core.
  Complete streams of both codecs concatenate, which is what `xz -T0`, `pbzip2` and `lbzip2` all
  rely on and what cram's own reader already handled, so a window of chunks compresses in parallel
  and is written in order. Chunk sizes are set per codec against its window, 32 MiB for xz against
  an 8 MiB dictionary and 4 MiB for bzip2 against a 900 KB block, and the pool is bounded by bytes
  in flight rather than by the core count, because 24 workers each holding a 32 MiB xz chunk is most
  of a gigabyte on a machine that may not have one.

  Kernel tree: `.tar.xz` **442.63 s at 1.0 effective cores → 41.35 at 18.9**, against `xz -T0`'s
  34.45 s while writing 0.88% less; `.tar.bz2` 9.54 s at 15.5 → **7.85 at 19.2**. The memory goes
  the other way and is worth stating: xz peak RSS 2709 MB → 3960, above `xz -T0`'s 3196. Measured
  14–15 August 2026.

- **The `zstd-c` feature reaches `.tar.zst`, which changes the bytes cram writes at `--auto`.** The
  feature had reached `formats/cram.rs` and nowhere else, so a `.tar.zst` was written at ruzstd's
  Fastest, its only level, and read back by the pure-Rust decoder, in a shipping build that already
  links the C library for `.cram` packs. On the kernel tree that was 18.78 s for 742,491,196 bytes
  against `zstd -T0 -3`'s 1.73 s for 540,088,970, and extraction 16.18 s against GNU tar's 2.12;
  going through libzstd took create to 5.26 s and the archive to 534,639,379 bytes. **`--auto` now
  means zstd level 3**, zstd's own default, the way `--auto` already means gzip 6 and xz 6, so a
  `.tar.zst` written by a `zstd-c` build is a different file from the one 1.1.0 wrote and a
  head-to-head against `zstd` at its default compares like with like. The pure-Rust path is
  unchanged and still there for builds without the feature. Measured 14 August 2026; the 16 August
  run confirms the archive size.

- **A `.tar.zst` is then written by libzstd's own workers.** At 5.26 s it was running at 0.9
  effective cores, the last codec in the tar family still doing everything on one. libzstd's workers
  share one context and one window, so unlike chunking this costs no ratio and needs no seams, and
  the output is a plain single-frame `.zst` that `zstd -t` accepts. Kernel tree, 24 threads:
  **5.26 s at 0.9 effective cores → 1.92 at 3.1**, against `zstd -T0`'s 1.71 s. Peak RSS 84 MB →
  339, against its 275. Proven on windows-gnu before landing, which was the risk, and Cargo.lock is
  unchanged. Measured 15 August 2026.

- **A `.tar.br` stops using a 16,384-bucket hash table whatever its size.** brotli picks its hash
  table from `size_hint` and nothing else, and a caller that never sets one gets whatever
  `update_size_hint` infers from the first write. tar streams through `io::copy` in 8 KiB writes, so
  every `.tar.br` inferred 8 KiB and used the smallest table brotli has, however large the archive
  was. The walk already counts every byte for the progress bar, so that figure now reaches the
  encoder; an archive whose size could not be counted guesses high, since the hint selects a hasher
  and bounds nothing. Kernel tree: **608,920,976 → 487,982,888 bytes, 19.9% off**, marginally under
  what `brotli -q 6` writes. Round-trip verified byte-identical and `brotli -t` accepts the output.
  Measured 14 August 2026; the 16 August run confirms the archive size.

- **One big file into a `.7z` used one core.** Solid mode asks for one uninterrupted LZMA2 stream
  per pack, and an archive of a single file is a single pack, so enwik9 ran at 99% CPU. A block
  holding a lone entry now uses the multi-threaded encoder, at a 64 MiB chunk rather than the
  dictionary-sized default the many-packs branch uses, because there the block is the whole archive
  and smaller chunks only buy more seams. **enwik9: 375.41 s → 46.51**, which is 8.1x, and 1.48x
  faster than 7-Zip's 68.79 s for 0.51% more bytes. Peak RSS 122 MB → 2961, under 7-Zip's 3866.
  Restricted to a lone entry on purpose: a block of many small files keeps the cross-file matching
  that is the point of solid, and keeps its bytes exactly, with Silesia byte-identical before and
  after at 49,197,225. Measured 15 August 2026.

- **`--tiny` feeds zopfli in master blocks, which puts it past 7-Zip's DEFLATE encoder.** Zopfli
  compresses one master block per `write` call and splits each at most fifteen ways, so handing it a
  51 MB entry whole bought sixteen Huffman trees for the file. Fed in 1 MiB blocks, Silesia goes
  from 1.26% behind to **0.02% ahead of `7z -tzip -mx=9`**, 64,712,418 bytes against 64,725,403, and
  peak memory falls from 2,390,804 KB to 728,652 KB at unchanged wall time. That comparison is
  DEFLATE against DEFLATE and nothing more: 7-Zip writing its own `.7z` format at `-mx=9` puts the
  same corpus in 48,688,243 bytes, 25% smaller than either, in a fraction of the time. Verified
  byte-identical three ways, by `cram t`, by Info-ZIP `unzip`, and by comparing all twelve entries.
  Measured 15 August 2026; the 7-Zip `.7z` figure is from the 16 August run.

- **Extraction stopped giving every file an 8 MiB buffer it could not use.** One is live per
  concurrent worker, so a 24-thread machine held 192 MB of buffer whatever the archive, and the
  kernel tree averages 20 KB an entry. A buffer is now never larger than the file, and the block
  itself is 1 MiB rather than 8, which is still enough to saturate a write stream. Peak RSS
  extracting a zip: **Silesia 125 MB → 36.6, kernel tree 214 → 146**, both corpora marginally
  faster. Only the parallel path changes; the sequential and streaming paths hold one buffer in
  total rather than one per worker. Measured 15 August 2026.

- **`--small` chooses its pre-filter from a sample instead of compressing each pack six times.**
  `pack_compress_cold` screened six candidates by compressing the whole pack with each at preset 1
  and then encoded once more at the real preset, seven full LZMA passes per pack to keep one, and
  profiling put 38.5% of a `--small` run in preset 1's match finder against 17.2% in preset 9's.
  What the screen decides is which pre-filter and which literal-context settings suit the data,
  which is a property of its structure rather than of its length, so four windows spread across the
  pack, 4 MiB in total, answer it. Cram corpus 1.0, 42,151 files, 2,800,604,582 bytes, to
  `/dev/shm`: **336.50 s → 191.98 for 32,176 more bytes, which is 0.0019%.** Peak RSS goes the other
  way, 7.18 GB → 8.34, because packs are no longer throttled by the screen;
  `CRAM_COLD_SCREEN_MIB=0` restores the whole-pack screen. `--small` output changes as a result and
  stays deterministic. Measured 15 August 2026; the 16 August run puts the same corpus at 192.62 s
  and 1,660,873,216 bytes.

- **A ranged read of a large solid `.7z` starts at a segment instead of the block.** This is the
  mount primitive, and on a block too large to cache it decoded from the block's first byte, so
  reading sixteen bytes out of a 2.3 GB archive decoded however much of it lay in front of them.
  It now begins at the LZMA2 segment holding the range, using the same seams block extraction
  already uses. On that archive a range costs **0.54 s against 24.80 s**, and costs the same at the
  start of an entry as at the end — the cost is the segment, not the distance into the archive.
  Nothing is held while it decodes: only the dictionary window stays resident, so a segment far
  larger than memory costs its window rather than its length.

- **A single large file no longer extracts on one thread.** The parallel path's unit of work was the
  entry, so an archive holding one file used one core however many were free. A `.cram` entry is a
  list of chunks and every chunk names its pack, so the entry can be cut at pack boundaries — the
  only seams that do not make two workers decode the same pack — and its pieces decoded
  concurrently. enwik9 goes from **9.05 s at 1.0 effective cores to 2.06 s at 5.4** (measured
  14 August, with the archive unwarmed, which reads the drive), byte-identical to the original
  1,000,000,000-byte file. The 16 August run measures the same extraction at **1.20 s in 1245 MB,
  against 7-Zip's 1.69 s in 1176 MB**, so cram is 1.4x faster in 6% more memory. The claim this
  entry used to carry, that cram was still 26% behind 7-Zip here, came from the unwarmed run and
  does not hold.

- **A block too large to cache is streamed rather than refused.** Two gates were judging the wrong
  quantity, and between them a 1 GiB `.7z` written by a single-threaded encoder fell all the way back
  to the sequential reader: 10.62 s and 2477 MB against 7-Zip's 5.53 s and 127 MB. One refused any
  multi-entry block that could not be held in the cache, months after `copy_unit` made holding it
  unnecessary; the other charged an archive's segments for every core rather than for the workers it
  could actually use. Now **7.90 s and 64 MB**, and a `-mx=9` archive that decoded on one thread
  goes from 11.16 s to 3.09 s. Output byte-identical on every archive tested.

- **The write-bound worker count no longer scales by a figure that is not a ceiling.** It is
  `wall / decode_rate`, linear in a number the inline probe gets wrong: 512 MiB is absorbed whole by
  a drive with a gigabyte of SLC cache, so it reports the cache — 331.9 MiB/s for a volume whose
  4 GiB probe measures 84, and on a RAM disk it reports memory bandwidth. Twenty decoders were being
  fielded where the measured knee is eight. A write figure now records whether it is a sustained
  ceiling or a burst, only a ceiling is scaled by, and the distinction survives a reload (profile
  schema 4, so older profiles are re-measured rather than misread). On Cram corpus 1.0 that is
  **40% less CPU and 41% less memory for 11% more wall time**, which is a trade and is written down
  as one.

- **`cram t` and extraction of many small files.** Both were listed as open findings, one at the
  highest severity, and both had been fixed by earlier work that never came back to say so. The
  kernel checkout used by those findings, 86,618 files, extracts at **29,664 files/s against the
  39 files/s** the finding recorded, and verifies in **1.10 s against 17.64 s**. Every one of the
  86,618 files byte-identical at both compression levels. That is an older and smaller snapshot than
  the 94,778-file / 1,920,837,858-byte kernel tree the tables above use.

- **7z extraction asks the archive how big its dictionary is, and uses a third of the memory.** A
  segment's own length was the only bound available while the 7z crate kept coder
  properties private. It is always safe — a segment opens on a dictionary reset, so nothing in it
  reaches further back than its start — and it is about four times too large, since 7-Zip writes
  32 MiB dictionaries into 128 MiB thread blocks and every concurrent segment holds a window.
  Reading the declared size took peak RSS on the corpus from **2809 MB to 867 MB**, and 15% off the
  CPU as well, from the allocation that no longer happens. The declared value is attacker-controlled
  and is used only to *shrink* the window, never to grow it.

- **7z packs are compressed concurrently instead of chunked inside one.** Packs were compressed
  straight into the output stream, so two could never be built at once and every thread had to come
  from LZMA2's chunking within a single pack — capped at pack size over dictionary, and costing
  ratio because a match cannot cross a chunk boundary. Creating a 2.8 GB corpus went from 57.9 s to
  43.4 s, and the archive came out *smaller*. The two archive sizes this entry used to quote could
  not be reconciled with any corpus this project measures, so they are gone rather than restated.

  Both of the above needed API that `sevenz-rust2` keeps private, so cram now depends on
  `sevenz-rust2-cram`, upstream 0.21.3 plus those two additions and nothing else (163 inserted
  lines, no deletions). Both are offered upstream as pull requests and the fork retires if they
  land.

- **7z extracts in parallel.** Entries in a 7z share a solid block, so the block is the unit of work
  rather than the entry. On Cram corpus 1.0 that took a cram-written `.7z` from 8.62 s to 1.35 s
  (measured 13 August), every extraction checked file-by-file against the corpus manifest. 7-Zip
  extracts its own archive of that corpus in 3.82 s on 16 August, at a 5.6% spread; the 3.25 s this
  entry used to compare against sits outside that band and has been dropped rather than kept.

- **A 7z written by a multi-threaded encoder is split finer than its blocks.** 7-Zip's default puts
  an entire archive in ONE solid block, which no amount of block-level parallelism can divide. But
  its multi-threaded encoder resets the LZMA2 dictionary at each thread-block boundary, and a chunk
  with a dictionary reset can be decoded without anything before it. Cram walks that framing and
  treats each reset as a starting point: the Cram corpus 1.0 archive has 47,011 chunks and 21
  resets, and extracting it went from 25.01 s to 3.66 s against 7-Zip's 3.68 s, using 2795 MB where
  7-Zip uses 4876 MB. Measured 13 August. The 16 August run puts 7-Zip on that corpus at 3.82 s and
  4877 MB with a 5.6% spread, so its half of the comparison holds; cram's 2795 MB does not, because
  the dictionary-size entry above took it to 867 MB the same day.

  This depends on how the archive was written, not on the format. A `.7z` written single-threaded,
  or smaller than one thread-block, has one segment and extracts at the old speed. Chains with a BCJ
  or delta filter keep the old path too, since filter state crosses the boundary.

- **Extraction no longer holds a decoded block in memory.** Serving a solid block's entries one at a
  time meant decoding the block and keeping every entry's bytes until asked for them — 1.8 GB of
  peak RSS on Cram corpus 1.0. Entries are now handed over as they decode, which is both smaller and
  faster: 175 MB, and 38% less CPU than the path it replaced. `cram test` shares it, 8.04 s to
  1.16 s.

- **A write-bound extraction sizes its thread pool from the measured rates rather than from a
  fraction of the core count.** Saturating a write wall at a given per-worker decode rate takes
  `wall / decode_rate` workers; the previous fixed cap was chosen for a codec fast enough that eight
  of them outrun any drive, and on a slow one it contradicted itself — projecting twenty-one units
  of LZMA decode to classify the extraction as write-bound, then running eight. The old value is
  kept as a floor, so no archive gets fewer workers than before.

- **The calibration profile records a write wall per volume instead of one per machine** (schema 3).
  The wall is a property of the destination, not of the computer, and keeping one meant whichever
  destination was extracted to first set the figure for every later one — a RAM-disk measurement
  planning writes to a hard disk. Codec rates stay per machine, since those belong to the CPU. A
  profile written by an older version is re-measured rather than migrated.

### Fixed

- **A multi-frame `.lz4` was decoded to the end of its first frame and reported as complete.** A
  `.lz4` is a run of frames: `cat a.lz4 b.lz4` produces one, and so does any parallel lz4 writer.
  `lz4_flex`'s `FrameDecoder` stops at the first frame's EndMark, so cram returned what it had with
  **no error and no short-read signal**. Two concatenated frames gave 40,000 bytes of 80,000; five
  gave 49,152 of 197,385. The comment beside that decoder claimed the opposite, that `FrameDecoder`
  already advances through concatenated frames, and nothing checked it. Silent wrong output on a
  read path, so it is worth reading twice if you hold `.lz4` files written by anything other than
  the reference CLI. There is a test now, and it fails against the old reader. zstd already had this
  walk; both formats use the same skippable-frame layout, so the machinery is shared.

- **A RAR archive of many small files re-detected the hardware once per entry.** `inmem_ceiling`
  decides whether an entry is held in RAM or routed through a scratch file, and it called
  `HwProfile::detect`, which re-reads CPU topology and `/proc/meminfo` and probes the work drive
  through `/sys/block` or an IOCTL on Windows. That is about 1.25 ms of syscalls per entry to answer
  a question whose answer cannot usefully change between two entries of the same archive. It is now
  resolved once when the reader opens. On the 94,778-file kernel tree, `cram t` goes **124.03 s →
  75.09** and extraction **135.88 → 79.35**; Silesia's twelve files are unchanged at 0.53 s, which
  is the shape the finding predicted. This does not close the gap to `unrar`, which tests the same
  archive in 4.28 s: 42% of what is left is inside UnRAR's own header reading, which is quadratic in
  extract mode and not reachable from the `unrar` crate. Measured 15 August 2026.

- **A directory passed to `l`, `x`, `t` or `conv` surfaced as the platform's error for opening a
  folder as a file.** On Windows that is `Access is denied`, which sends people looking for a
  permissions problem that is not there. The check sits in `sniff_path`, which every read verb
  funnels through. Pointing at the wrong thing is also not a bug report, so this one error no longer
  writes a diagnostic report; with diagnostics on, a typo was producing a crash-style file and
  burying the genuine reports.

- **`cram x --help` printed `No such file or directory` instead of help**, and so did `t` and `l`.
  Those three take an archive as their first positional and `--help` looked like one, which made the
  first thing a new user types the first thing that breaks. `--help` or `-h` anywhere in a verb's
  arguments now prints that verb's own section of the usage block, to stdout, exit 0, and
  `cram help <verb>` works too. The section is extracted from `USAGE` rather than written out a
  second time, so the two cannot drift, and a value that looks like a flag is still a value:
  `cram x a.zip -p -h` is a password of `-h`.

- **The README named the wrong crate to install.** `cargo install cram` fetches somebody else's
  package; the CLI is `cram-cli`.

- **Creating a `.7z` of a large tree died with `Too many open files`.** An 86,618-file kernel
  checkout, on a tree 7-Zip archives without complaint. A block holds up to 8,192 open handles and
  up to `inflight_max` packs hold as many again, so on a 24-thread machine the writer wants 205,000
  descriptors against a raised soft limit of 65,536. The recovery existed and could not work: it
  drained one finished pack and retried once, and because that drain succeeded it never went on to
  flush the block holding most of the handles. It now releases progressively and retries after each
  step, finished packs before the open block.

- **A crafted 7z could make `cram t` and `cram x` run forever.** A solid block is one stream shared
  by several entries, so a reader that has reported corrupt input is asked for bytes again — once by
  the code that records the failed entry and carries on, and again by the drain that advances to the
  next entry. An LZMA2 stream does not return from that second read. A 2,208-byte archive found by
  the fuzz harness pinned one core for over two hours with no error, no output and no memory growth.
  Reads of a block or segment now stop touching the source after its first failure, and the unit is
  failed rather than continued, so every entry it never reached is reported instead of being dropped
  from an otherwise successful-looking run. The archive is kept as a regression fixture.

- **The fuzz harness now fails on a hang instead of waiting for one.** Each input gets its own thread
  and 60 seconds, because a parser that spins produces no panic and no error and so looked exactly
  like slow work — which is why the hang above went unnoticed for two hours.

- **A single large file no longer disabled parallel 7z extraction for the whole archive.** The
  memory bound was applied to the largest block, and a block holding one big entry needs no cache at
  all — it can be streamed. One 263 MiB video, alone in its block, was 5.5% over the budget and took
  the other 48 blocks of Cram corpus 1.0 down with it, leaving extraction on 1.3 effective cores.
  The bound now applies only to blocks that more than one entry shares.

---

## [1.1.0] - 2026-08-12

Mostly work on finding duplicates, and on what you can see and do with the result. The engine gains
progress reporting and a verification pass; Cram Studio gains a gallery, a full-size viewer and the
ability to delete. As with every release here, the Cram Studio installer ships as an asset and is a
separate proprietary product under its own EULA.

### Added

- **A duplicate scan reports what it has found while it is still running.** A scan over a whole
  drive spends minutes walking before it can group anything, and until now it said nothing for all
  of it, which is indistinguishable from being hung. `Progress::on_scan_progress(files, dirs)` is
  called at most every 250 ms, and Studio shows the running counts.

- **Photos and video open in their own tab in Cram Studio, as a gallery.** A duplicate set of
  images cannot be judged from a list of paths, so image and video sets now go to a separate tab
  with thumbnails, per-file or whole-row selection, and identical and look-alike kept on separate
  panes. Everything that is not an image or a video stays where it was: a thumbnail of a `.dll`
  tells you nothing.

- **A full-size preview behind every thumbnail**, from the magnifier on hover or a double-click.
  The arrows step through the set with both neighbours prefetched, and the image is contained in a
  fixed stage so two near-identical shots land in the same place and only the difference moves.
  Where the file is small enough and the format is one the view can draw directly, the original
  bytes are shown rather than a re-encode, so nothing you are judging is an artefact of the
  preview; when it does have to downscale, it says so.

- **Duplicates can be deleted from Cram Studio, to the Recycle Bin.** Deliberately not a button you
  can hit in passing: it has to be held, it says how many and how much, and everything goes to the
  Recycle Bin so a mistake is recoverable from Explorer. Selecting every copy in a set raises a
  warning rather than being forbidden.

- **The hard-link option explains itself**, behind a `?` beside it, and a duplicate can be shown in
  its folder rather than only named.

### Fixed

- **A look-alike group could swallow hundreds of unrelated images.** One scan put 936 different
  terminal screenshots in a single group. Grouping was by perceptual hash alone, unioned, which is
  single-linkage clustering: A joins B and B joins C, so one bridging pair merges two sets that
  resemble each other not at all. Tightening the hash threshold could not have fixed it, because
  single-linkage always finds a bridge. A candidate pair is now verified against the pixels — same
  aspect ratio within 10%, and a mean absolute difference no greater than 0.007 over a 64-pixel
  colour render. Colour matters here: discarding chroma is what makes a *hash* robust, and it is
  also what makes two unrelated dark terminals look identical. The threshold sits between the
  noisiest thing that must stay together (a photo resized and re-encoded at JPEG q40, 0.0037) and
  the closest thing that must separate (two different terminals, 0.0132) — about 1.9x from each. A
  retake of the same terminal with one word changed scores 0.0009 and stays grouped. Those are
  synthetic images, so treat the margin as real but not generous.

- **A cancelled extraction took back more than it wrote.** It now removes only files and directories
  that did not exist when it created them, so cancelling an extraction into a folder that already
  held a file of the same name leaves that file alone.

- **A directory cycle walked forever.** Past 1,000 levels the walk checks file identity and stops
  when it reaches somewhere it has already been. Legitimately deep trees are unaffected, which has
  a test.

- **Thumbnails were drawn at roughly a quarter of the resolution they were displayed at.** The
  request was sized in CSS pixels rather than device pixels, the result was then cropped to fill and
  upscaled again, the fast integer resampler aliased text, and JPEG's default quality rang around
  every glyph edge. Now: sized from the display's pixel ratio, Lanczos3, quality 90, and the whole
  image shown rather than a crop. Against a perfect render at the display size, 12.4x closer for
  4.1x the bytes.

- **Installing a new version over an old one failed with Cram Studio closed.** `cram shell install`
  registers `cram_shell.dll` as an in-process context-menu handler, so the first right-click after
  an install maps it into `explorer.exe`, which does not let go; the installer then could not open
  it for writing. Explorer is not something an installer can close, and the check for a running app
  looks for Studio rather than Explorer. A mapped file can still be renamed, so the installer now
  moves the old one aside and writes the new one over the name it vacated. Explorer keeps serving
  right-clicks from the previous version until it next restarts, which is a better outcome than an
  install that will not proceed.

- **The browser hand-off left Firefox's own copy of every download on disk.** The add-on called
  `downloads.erase()`, which removes the entry from Firefox's history and never touches the file.
  Deleting the bytes is `downloads.removeFile()`, which was not called at all. The leftover file
  also explains the `file(1).type` names: Cram found its own destination taken and uniquified around
  it. The add-on needs updating separately from Cram itself.

## [1.0.2] - 2026-08-11

Skips 1.0.1, which the CLI already used for a crates.io-only release. `cram --version` and Studio's
updater both compare their own version against the release tag, so the two have to agree, and every
crate here is on 1.0.2 rather than leaving the CLI a version ahead of everything else.

### Fixed

- **A directory tree deeper than about 640 levels killed the process.** Both tree walks recursed.
  Each frame of the duplicate-scan walk costs 3,264 bytes on the shipped Windows binary, so a scan
  running on a 2 MiB worker thread ran out of stack and died with `0xc00000fd`. It died silently: a
  stack overflow is a hardware exception rather than a Rust panic, so nothing unwinds, no error is
  reported and no diagnostic is written — `cram dedup` and Cram Studio both simply disappeared.
  Found by scanning a drive that happened to hold a 14,566-level tree. Both walks now carry their
  own stack, so depth is bounded by memory rather than by the thread it runs on. `cram a` had the
  same defect on the create side and is fixed with it; archive member order is unchanged, and now
  has a test that says so.

### Added

- **A checkpoint that outlives a run which dies without unwinding.** The event log lives in memory
  and dies with the process, so a crash left nothing at all to read, and the duplicate-scan engine
  recorded nothing even with detailed diagnostics switched on. A running operation now mirrors its
  operation, phase, item count and current depth to a file once a second, and deletes it on a clean
  finish — so a file left behind is itself evidence that the run did not finish, and says where it
  had got to. `cram diag report` adopts it into the next report. Cost is one atomic load per item,
  and a checkpoint another process is still rewriting is left alone.

## [cram-cli 1.0.1] - 2026-08-07

A crates.io-only release. The published binaries and the `v1.0.0` tag are unaffected, and no other
crate in the workspace changed, so they remain at 1.0.0.

### Fixed

- `cargo install cram-cli` failed to link on the **windows-gnu** toolchain. The three link flags
  that build needs (`-static`, `--allow-multiple-definition`, `-ladvapi32`) lived only in
  `.cargo/config.toml`, which cargo does not include in a published `.crate`, so installing from the
  registry linked without them and hit a `pthread_*` multiple-definition error. They are now emitted
  from a build script, which is published, gated on the target actually being windows-gnu. Linux and
  macOS were never affected, nor was windows-msvc.

---

## [1.0.0] - 2026-08-06

The first public release of the Cram engine and command line. Everything below is new, so there is
nothing to list as changed or fixed.

The release will also carry the **Cram Studio** installer as an asset. Studio is a separate,
proprietary product under its own EULA; the MIT OR Apache-2.0 licence covers the engine and CLI in
this repository and not that installer.

### Added

**The `cram` CLI**, one command for the whole lifecycle: `l` (list), `x` (extract), `a` (create),
`t` (test), `conv` (convert), `dedup` (find duplicate files), `mount`, `rec` (recovery sidecar),
`sign` / `verify` / `keygen`, `make-sfx`, and `dl` (segmented download, behind the opt-in `download`
feature). Free and open
source under MIT OR Apache-2.0.

**Formats.** Reads ZIP, 7z, tar (+ gzip / xz / zstd / bz2 / lz4 / brotli), ISO 9660, RAR, bare
single-stream compressed files (`foo.gz`, `foo.xz`, …), and Cram's own `.cram`. Writes ZIP, 7z, tar
(+ the same codecs), and `.cram`. RAR is **read-only**, creating RAR is forbidden by the UnRAR
licence and never will be supported.

**The `.cram` format.** Content-defined chunking (FastCDC) → BLAKE3-keyed **global
dedup** with no dictionary-window limit → compressed packs → a footer index. An archive is v1 unless
it uses a per-entry transform (see JPEG recompression below), in which case it declares v2 and a
v1-only reader refuses it rather than misreading it. Optional Argon2id +
AES-256-GCM encryption, when a password is set the footer index is sealed along with the packs, so
the file listing is hidden as well as the contents, and byte-for-byte reproducible when unencrypted.
Specified normatively in [`docs/CRAM_FORMAT.md`](docs/CRAM_FORMAT.md).

**Three effort levels**, `--fast`, `--auto` (the default) and `--small`, plus `--store` for an
uncompressed archive that is still deduplicated. `--small` is the far end: the widest pack the format
allows, LZMA's extreme match search, and a per-pack search over pre-filters and coder parameters,
keeping whichever came out smallest. That search is worth its cost because the answer is
content-dependent, the x86 BCJ filter takes Silesia's `ooffice` down 14.1% and makes `mozilla` 0.9%
larger, so it can only ever be a candidate. Measured on Silesia, `--small` is 2.5% smaller than the
same archive built without the search, and smaller than `xz -9e` with a 256 MiB dictionary. Nothing
about it reaches the reader: an xz block header carries its own filter chain, so a `--small` archive
is read by any Cram build.

`--store` is **not** the fast option, despite compressing nothing. Measured on a 94,778-file tree it
ties `--fast` on create while writing 3.4x the bytes, then extracts 2.6x slower carrying them back.
What it is for is reading part of an archive without decompressing anything.

**`cram dedup`**, find duplicate files across folders and drives, without archiving anything. A file
whose size is unique cannot have a byte-identical twin, so it is never read; same-size files are
separated by a partial hash of their first and last 64 KiB; only what survives both is read in full
and confirmed with BLAKE3. Reads are scheduled per drive, every volume at once, but one sequential
reader on a spinning disk and several on an SSD, since parallel reads make an HDD slower. Hard links
are counted as one physical file, so reclaimable space is not overstated.

By default it only reports. `--link` replaces duplicates with hard links (every filename and folder
stays where it is), `--quarantine <dir>` moves them aside instead, and both preview unless `--apply`
is given. Nothing is ever deleted. Each pair is re-hashed at the moment of action, so a plan made
earlier cannot act on a file that has changed since.

`--similar` additionally flags images that look alike without being byte-identical (a resize, a
re-save). These are reported separately, are never counted as reclaimable, and no action can consume
them: a perceptual hash cannot tell a redundant re-encode from two different frames of a burst.
Needs the `phash` feature.

**Lossless JPEG recompression in `.cram`**, at `--small` or on any level with `--recompress`. It is
off at the default `--auto`, and `--no-recompress` overrides both. (This entry said "on by default"
when 1.0.0 shipped and that was wrong then too: `recompress_choice` has always been
`--recompress` or `Level::Cold`.) A photo is already entropy-coded, so
general-purpose compressors gain roughly nothing on one; redoing that coding with a stronger coder
(Lepton) is worth about 23% while extraction reconstructs the original file byte-for-byte. Measured
on one folder of 34 phone photos (26.1 MB): ZIP and 7z both produced output fractionally *larger*
than the originals, `tar.xz` managed 2.7%, and `.cram` was 23.6% smaller with all 34 files
extracting byte-identical. That is a single sample rather than a benchmark. Every candidate is verified to round-trip before it is stored, and anything that
fails verification is stored untouched.

**Linux and macOS support** (`x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`), built and tested
alongside Windows, plus an `install.sh` that fetches the right binary for either. See Known
limitations for what still differs between the three.

**A second, independent `.cram` decoder.** `cram-extract.exe` implements the same spec from the
document alone, shares no code with the engine, and takes five direct pure-Rust dependencies
(`lzma-rust2`, `ruzstd`, `aes-gcm`, `argon2`, `lepton_jpeg`). It contains no C or C++ code,
so unlike `cram.exe` it needs no DLL beside it beyond the OS's own. Your data stays recoverable even
if the main build is not available. It doubles as the `make-sfx` self-extractor stub.

**Parallel extraction** for the formats with a random-access interface (ZIP, ISO, `.cram`). The worker
count is derived from the *destination* drive, hardware auto-detect plus a one-shot calibration
cached in the per-user config directory (`%APPDATA%\cram\profile.toml` on Windows,
`~/.config/cram/` on Linux, `~/Library/Application Support/cram/` on macOS). `--skip` leaves a destination file alone only when a
per-entry CRC proves it identical, so it helps on ZIP and 7z entries that store a CRC, and does
nothing on `.cram`, tar, RAR or ISO, or on a WinZip AES entry written in AE-2 form, which stores no
CRC and is proven by its AES authentication instead.

**Damage is contained per entry.** A damaged or truncated archive does not abort the job: intact
entries are extracted, every failure is reported by name, and the command exits non-zero. A partial
extraction can never report itself as a clean one.

**Encryption** on create and extract: AES-256 for ZIP and 7z, AES-256-GCM for `.cram`. Hiding the
file listing as well as the contents: `.cram` always does it when a password is set, 7z does it on
`--encrypt-names`, and ZIP cannot; ZIP encrypts contents but leaves the central-directory names in
the clear, so `--encrypt-names` on a ZIP is refused rather than silently ignored.

**Integrity and repair, on any file; not just archives.**
- `cram sign` / `cram verify`, detached ed25519 signatures (`.cramsig`), with `--key` to pin a
  required signer. The hash is streamed, so file size does not matter.
- `cram rec`, Reed-Solomon parity sidecars (`.cramrec`) that verify and repair bit-rot or
  truncation. This one works on the file **in memory**: creating a sidecar reads the whole file in,
  and verifying or repairing reads in both the file and its sidecar, so allow for roughly twice the
  file size in RAM. Files above about 200 GiB are refused outright.

**`cram mount`**, browse an archive as a virtual folder through Windows ProjFS. ZIP, ISO and `.cram`
are served by byte range straight from disk; tar, 7z, RAR and bare compressed streams are decoded
into RAM up front and capped at 2 GiB. ProjFS is an optional Windows feature (`Client-ProjFS`, off by
default); the DLL is bound lazily at run time, so every other command works whether or not it is
enabled.

`--writable` makes the mount folder a persistent layer over the archive. ProjFS makes a mount
writable whether or not anyone asks — a modified placeholder becomes a full file and a deleted one a
tombstone, both on disk — so a read-only mount never prevented writes, it only discarded them along
with the folder. With `--writable` they are kept: the archive is the immutable base, the folder is
everything that diverged, and re-mounting resumes over it. A modified file wins over the archive's
copy, an untouched one still comes from the archive, and the `.cram` is never written to. Deleting
the folder resets to a pristine archive and is the only way, since ProjFS cannot un-tag a
virtualization root. Without the flag, a folder that has picked up files not in the archive is now
kept rather than deleted, which previously lost them silently.

`--remember` adds a mount to a list that `cram mount --restore` brings back after a reboot, in one
process holding all of them. `--list` shows it, `--forget` drops an entry without touching the
folder. Nothing is remembered unless asked for: the list starts empty and there is no setting that
turns auto-remount on for everything. Encrypted archives are refused, their password not being
something Cram will store. Cram Studio, when set to start with Windows, runs
`--restore` at boot and only at boot, in a detached `cram.exe` so the mounts outlive the Studio
window; opening Studio by hand re-mounts nothing.

**`cram shell`**, Cram on the Windows Explorer right-click menu. Extract here, extract to a
subfolder and test on an archive; add to a `.cram` or a `.zip` on anything else. A container
document (`.docx`, `.jar`, `.epub`) gets both sets, since it is legitimately both. Where
`cram-studio.exe` sits beside `cram.exe`, two more entries appear, "Open in Cram Studio" and
"Add to archive…", which open Studio rather than running a `cram` command. A COM
`IContextMenu` handler, the same mechanism WinRAR and 7-Zip use, registered under `HKCU` only so it
needs no elevation and changes nothing for other accounts. On Windows 11 it appears under "Show more
options". `cram shell uninstall` removes it and `cram shell status` reports what is registered.

**`cram update`**, replace the installation with the latest published release. It fetches the
checksum the release publishes before downloading anything and refuses to install what it cannot
verify; the download URL is built locally rather than taken from the API response; and the running
binary is replaced by a move-aside and a rename, so a failure leaves the previous version in place.
`--check` reports and changes nothing. Needs the `download` feature.

**`cram conv`**, re-export any readable archive into another format.
Conversion does not carry encryption across: `-p` opens an encrypted *source*, `--encrypt <pw>`
encrypts the *destination*, and converting an encrypted archive without `--encrypt` writes a
readable, unencrypted copy.

**`cram diag`**, a diagnostic report a user can attach to a bug report, in the CLI and in Studio.
It records the build, the machine profile that decides Cram's thread and pack sizing, the archive's
structure, the failing error and the entries that failed.

*Nothing is ever sent anywhere.* There is no telemetry in Cram and no code in either binary that
could upload a report; it is a text file on disk, and sending it is something the user does by hand.

*Names are redacted by default.* For an archiver the paths are the sensitive part, so an entry is
described by its shape — extension, size, depth, name length, alphabet, and flags for the cases that
are themselves the bug (a reserved device name, a trailing dot, control characters, an over-long
path) — rather than by its name. That makes a report safe to attach to a public issue without
reading it first. `--full-paths` includes the real names for anyone who would rather just send them.
Passwords never reach a report: the value after `-p`, `--password`, `--encrypt` or `--key` is
replaced before the command line is recorded.

*Detailed recording is opt-in.* Off by default, because an event per entry across tens of thousands
of files is a real cost on a tool whose point is speed. With it off, a report still describes the
build, the machine, the error, the failed entries, the archive's pack layout and codec mix, and the
create timings — everything reconstructable after the fact. With it on, a per-entry trace is added,
and Cram also writes a report when an operation fails, since the recording lives in that process and
would be gone by the time anyone asked for it.

*`--diag-report` works on any command* and writes a report about that run whether it succeeded or
not. "It worked, but it took four minutes" is a bug report too, and the timings that answer it exist
only while the command is running.

### Security

- A single centralized path-traversal (zip-slip) guard every backend must funnel entry names through,
  plus an independent equivalent in the standalone decoder.
- Explicit decompression-bomb bounds (pack size, total decompression work relative to bytes written,
  Argon2 parameters checked before the KDF runs, metadata-listing caps, sidecar shard caps).
- A short decode is a failure, not a success: a body that ends early against its declared size errors
  and removes the partial file.
- RAR is decoded by the C++ UnRAR engine. A verb that reads a `.rar` re-runs itself in a child
  process, so a fault in that engine kills only the child and is reported as a clean error.
- A bounded parser smoke-fuzz runs as part of the ordinary test suite.

Full policy, scope and reporting channel: [`SECURITY.md`](SECURITY.md).

### Known limitations

- **Platform support is not uniform.** Windows (`x86_64-pc-windows-gnu`), Linux
  (`x86_64-unknown-linux-gnu`) and macOS (`aarch64-apple-darwin`) each build and run the full test
  suite. Mount is Windows-only.
- **`cram test` cannot detect every bit flip.** Cram computes no checksum of its own for an
  unencrypted *stored* `.cram`, for `tar` and its compressed forms, or for ISO and RAR. None of
  those carries a per-chunk or per-file content checksum, so a flip inside file content can decode
  to wrong bytes undetected. What you get
  there is a clean decode plus a declared-size match, plus whatever the underlying decoder rejects;
  truncation and structural damage *are* caught. For guaranteed content integrity use ZIP, 7z, or a
  compressed or encrypted `.cram`, or pair any archive with `cram sign` or `cram rec`; both cover
  the whole file.
- **RAR is read-only** and always will be, the UnRAR licence forbids building a RAR compressor from
  its source.
- **A RAR entry is buffered whole in memory** by the UnRAR engine, which has no per-chunk hook.
  Entries above a memory-derived threshold are extracted to a scratch file beside the archive and
  streamed from there instead, costing one extra write and read; no entry is refused for its size.
- **Mounting tar / 7z / RAR / a bare compressed stream is capped at 2 GiB** of uncompressed content.
- **Symlinks and other special files are not archived on create**, only regular files and
  directories are stored, and the `.cram` format stores no timestamps by design.
- **Nothing is code-signed**, so Windows SmartScreen warns on first run of a downloaded binary.

<!-- These trail the last section, so the release workflow's notes extractor (which reads from a
     "## [x.y.z]" heading to the next one, or to EOF) picks them up along with 1.0.0's notes. That
     is harmless: a link reference definition renders as nothing. Worth knowing before anyone
     "fixes" the published notes by moving them. -->

[1.0.0]: https://github.com/lukr54/cram/releases/tag/v1.0.0
[1.0.2]: https://github.com/lukr54/cram/releases/tag/v1.0.2
[1.1.0]: https://github.com/lukr54/cram/releases/tag/v1.1.0
[Unreleased]: https://github.com/lukr54/cram/compare/v1.1.0...HEAD
