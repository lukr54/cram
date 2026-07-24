# Changelog

All notable changes to Cram are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [1.0.0] — 2026-07-24

First public release of the Cram engine and command line. Everything below is new, so there is
nothing to list as changed or fixed.

### Added

**The `cram` CLI** — one command for the whole lifecycle: `l` (list), `x` (extract), `a` (create),
`t` (test), `conv` (convert), `mount`, `rec` (recovery sidecar), `sign` / `verify` / `keygen`,
`make-sfx`, and `dl` (segmented download, behind the opt-in `download` feature). Free and open
source under MIT OR Apache-2.0.

**Formats.** Reads ZIP, 7z, tar (+ gzip / xz / zstd / bz2 / lz4 / brotli), ISO 9660, RAR, bare
single-stream compressed files (`foo.gz`, `foo.xz`, …), and Cram's own `.cram`. Writes ZIP, 7z, tar
(+ the same codecs), and `.cram`. RAR is **read-only** — creating RAR is forbidden by the UnRAR
licence and never will be supported.

**The `.cram` format, frozen at v1.** Content-defined chunking (FastCDC) → BLAKE3-keyed **global
dedup** with no dictionary-window limit → compressed packs → a footer index. Optional Argon2id +
AES-256-GCM encryption — when a password is set the footer index is sealed along with the packs, so
the file listing is hidden as well as the contents — and byte-for-byte reproducible when unencrypted.
Specified normatively in [`docs/CRAM_FORMAT.md`](docs/CRAM_FORMAT.md).

**A second, independent `.cram` decoder.** `cram-extract.exe` implements the same spec from the
document alone, shares no code with the engine, and takes four direct pure-Rust dependencies
(`lzma-rust2`, `ruzstd`, `aes-gcm`, `argon2`). It contains no C or C++ code, so unlike `cram.exe` it
needs no DLL beside it beyond the OS's own. Your data stays recoverable even if the main build is not
available. It doubles as the `make-sfx` self-extractor stub.

**Parallel extraction** for the formats with a random-access seam (ZIP, ISO, `.cram`). The worker
count is derived from the *destination* drive — hardware auto-detect plus a one-shot calibration
cached in `%APPDATA%\cram\profile.toml`. `--skip` leaves a destination file alone only when a
per-entry CRC proves it identical, so it helps on ZIP and 7z entries that store a CRC, and does
nothing on `.cram`, tar, RAR or ISO — or on a WinZip AES entry written in AE-2 form, which stores no
CRC and is proven by its AES authentication instead.

**Damage is contained per entry.** A damaged or truncated archive does not abort the job: intact
entries are extracted, every failure is reported by name, and the command exits non-zero. A partial
extraction can never report itself as a clean one.

**Encryption** on create and extract: AES-256 for ZIP and 7z, AES-256-GCM for `.cram`. Hiding the
file listing as well as the contents: `.cram` always does it when a password is set, 7z does it on
`--encrypt-names`, and ZIP cannot — ZIP encrypts contents but leaves the central-directory names in
the clear, so `--encrypt-names` on a ZIP is refused rather than silently ignored.

**Integrity and repair, on any file — not just archives.**
- `cram sign` / `cram verify` — detached ed25519 signatures (`.cramsig`), with `--key` to pin a
  required signer. The hash is streamed, so file size does not matter.
- `cram rec` — Reed-Solomon parity sidecars (`.cramrec`) that verify and repair bit-rot or
  truncation. This one works on the file **in memory**: creating a sidecar reads the whole file in,
  and verifying or repairing reads in both the file and its sidecar, so allow for roughly twice the
  file size in RAM. Files above about 200 GiB are refused outright.

**`cram mount`** — browse an archive as a virtual folder through Windows ProjFS. ZIP, ISO and `.cram`
are served by byte range straight from disk; tar, 7z, RAR and bare compressed streams are decoded
into RAM up front and capped at 2 GiB. ProjFS is an optional Windows feature (`Client-ProjFS`, off by
default); the DLL is bound lazily at run time, so every other command works whether or not it is
enabled.

**`cram conv`** — re-export any readable archive into another format, so no archive is a dead end.
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

- **Windows-first.** Built and tested for `x86_64-pc-windows-gnu`; the mount is Windows-only.
- **`cram test` cannot detect every bit flip.** An unencrypted *stored* `.cram`, `tar` / `.tar.zst`,
  and ISO and RAR — for which Cram computes no checksum of its own — carry no per-chunk or per-file
  content checksum, so a flip inside file content can decode to wrong bytes undetected. What you get
  there is a clean decode plus a declared-size match, plus whatever the underlying decoder rejects;
  truncation and structural damage *are* caught. For guaranteed content integrity use ZIP, 7z, or a
  compressed or encrypted `.cram`, or pair any archive with `cram sign` or `cram rec` — both cover
  the whole file.
- **RAR is read-only** and always will be — the UnRAR licence forbids building a RAR compressor from
  its source.
- **Each RAR entry is buffered whole in memory** by the UnRAR engine, so an entry larger than 2 GiB
  is refused and reported as a per-entry failure rather than extracted.
- **Mounting tar / 7z / RAR / a bare compressed stream is capped at 2 GiB** of uncompressed content.
- **Symlinks and other special files are not archived on create** — only regular files and
  directories are stored — and the `.cram` format stores no timestamps by design.
- **Nothing is code-signed**, so Windows SmartScreen warns on first run of a downloaded binary.

[1.0.0]: https://github.com/lukr54/cram/releases/tag/v1.0.0
