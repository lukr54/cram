# Changelog

All notable changes to Cram are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [1.0.0], unreleased

First public release of the Cram engine and command line. Everything below is new, so there is
nothing to list as changed or fixed. The date goes in when the tag does; nothing has been published
yet, so the link at the foot of this file will 404 until then.

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
cached in `%APPDATA%\cram\profile.toml`. `--skip` leaves a destination file alone only when a
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

**`cram shell`**, Cram on the Windows Explorer right-click menu. Extract here, extract to a
subfolder and test on an archive; add to a `.cram` or a `.zip` on anything else. A COM
`IContextMenu` handler, the same mechanism WinRAR and 7-Zip use, registered under `HKCU` only so it
needs no elevation and changes nothing for other accounts. On Windows 11 it appears under "Show more
options". `cram shell uninstall` removes it and `cram shell status` reports what is registered.

**`cram update`**, replace the installation with the latest published release. It fetches the
checksum the release publishes before downloading anything and refuses to install what it cannot
verify; the download URL is built locally rather than taken from the API response; and the running
binary is replaced by a move-aside and a rename, so a failure leaves the previous version in place.
`--check` reports and changes nothing. Needs the `download` feature.

**`cram conv`**, re-export any readable archive into another format, so no archive is a dead end.
Conversion does not carry encryption across: `-p` opens an encrypted *source*, `--encrypt <pw>`
encrypts the *destination*, and converting an encrypted archive without `--encrypt` writes a
readable, unencrypted copy.

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
- **`cram test` cannot detect every bit flip.** An unencrypted *stored* `.cram`, `tar` / `.tar.zst`,
  and ISO and RAR, for which Cram computes no checksum of its own; carry no per-chunk or per-file
  content checksum, so a flip inside file content can decode to wrong bytes undetected. What you get
  there is a clean decode plus a declared-size match, plus whatever the underlying decoder rejects;
  truncation and structural damage *are* caught. For guaranteed content integrity use ZIP, 7z, or a
  compressed or encrypted `.cram`, or pair any archive with `cram sign` or `cram rec`; both cover
  the whole file.
- **RAR is read-only** and always will be, the UnRAR licence forbids building a RAR compressor from
  its source.
- **Each RAR entry is buffered whole in memory** by the UnRAR engine, so an entry larger than 2 GiB
  is refused and reported as a per-entry failure rather than extracted.
- **Mounting tar / 7z / RAR / a bare compressed stream is capped at 2 GiB** of uncompressed content.
- **Symlinks and other special files are not archived on create**, only regular files and
  directories are stored, and the `.cram` format stores no timestamps by design.
- **Nothing is code-signed**, so Windows SmartScreen warns on first run of a downloaded binary.

[1.0.0]: https://github.com/lukr54/cram/releases/tag/v1.0.0
