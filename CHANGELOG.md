# Changelog

All notable changes to Cram are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

7z extraction. It ran on one thread whatever the archive or the machine, which on a 7-Zip-written
archive of the benchmark corpus meant 25 s against 7-Zip's 3.7 s. It is now level with 7-Zip there
and 2.4× faster on an archive cram wrote, in less memory than 7-Zip needs in either case.

### Changed

- **7z extracts in parallel.** Entries in a 7z share a solid block, so the block is the unit of work
  rather than the entry. On the Cram corpus that took a cram-written `.7z` from 8.62 s to 1.35 s
  against 7-Zip's 3.25 s, every extraction checked file-by-file against the corpus manifest.

- **A 7z written by a multi-threaded encoder is split finer than its blocks.** 7-Zip's default puts
  an entire archive in ONE solid block, which no amount of block-level parallelism can divide. But
  its multi-threaded encoder resets the LZMA2 dictionary at each thread-block boundary, and a chunk
  with a dictionary reset can be decoded without anything before it. Cram walks that framing and
  treats each reset as a starting point: the corpus archive has 47,011 chunks and 21 resets, and
  extracting it went from 25.01 s to 3.66 s against 7-Zip's 3.68 s, using 2795 MB where 7-Zip uses
  4876 MB.

  This depends on how the archive was written, not on the format. A `.7z` written single-threaded,
  or smaller than one thread-block, has one segment and extracts at the old speed. Chains with a BCJ
  or delta filter keep the old path too, since filter state crosses the boundary.

- **Extraction no longer holds a decoded block in memory.** Serving a solid block's entries one at a
  time meant decoding the block and keeping every entry's bytes until asked for them — 1.8 GB of
  peak RSS on the corpus. Entries are now handed over as they decode, which is both smaller and
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
  the other 48 blocks of the benchmark corpus down with it, leaving extraction on 1.3 effective
  cores. The bound now applies only to blocks that more than one entry shares.

### Added

- `CRAM_PROFILE=1` prints the extraction plan and every input to it — bottleneck, workers, decode
  units, measured decode rate and write wall — so "why did this run on two threads" is one line
  rather than an afternoon of bisecting.

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

**Lossless JPEG recompression in `.cram`**, on by default. A photo is already entropy-coded, so
general-purpose compressors gain roughly nothing on one; redoing that coding with a stronger coder
(Lepton) is worth about 23% while extraction reconstructs the original file byte-for-byte. Measured
on one folder of 34 phone photos (26.1 MB): ZIP and 7z both produced output fractionally *larger*
than the originals, `tar.xz` managed 2.7%, and `.cram` was 23.6% smaller with all 34 files
extracting byte-identical. That is a single sample rather than a benchmark. Every candidate is verified to round-trip before it is stored, and anything that
fails verification is stored untouched. `cram a --no-recompress` turns it off.

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
