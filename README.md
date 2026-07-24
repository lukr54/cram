# Cram

Cram is a multi-format archive tool for Windows. One `cram` command lists, extracts, creates, tests,
converts and mounts archives, signs them and builds parity sidecars for them, plus a native format
(`.cram`) that stores repeated data once.

This repository is the **engine and the command line**. The `cram` CLI is free and fully featured;
nothing in it is restricted. It is written in Rust and targets Windows on the GNU (mingw) toolchain.
Everything is pure Rust **except the UnRAR C++ decoder**, which is always compiled in because it is
what reads RAR; the optional `zstd-c` feature links C libzstd, and `libdeflate` is a further opt-in
C dependency.

Licensed under **MIT OR Apache-2.0**.

---

## Formats

| | ZIP | 7z | tar(.\*) | ISO 9660 | RAR | `.cram` |
|---|:-:|:-:|:-:|:-:|:-:|:-:|
| List / extract | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Create | ✅ | ✅ | ✅ | — | — ✱ | ✅ |
| Test (integrity) † | CRC-32 / AES auth | CRC-32 if stored | decode | decode | decode | decode |
| Mount as a folder ‡ | on-demand | ≤ 2 GiB RAM | ≤ 2 GiB RAM | on-demand | ≤ 2 GiB RAM | on-demand |
| Encryption | AES-256 | AES-256 | — | — | read | AES-256-GCM |

`tar(.*)` covers plain tar plus gzip, xz, zstd, bzip2, lz4 and brotli. A **bare single-stream
compressed file** (`foo.gz`, `foo.xz`, `foo.zst`, `foo.bz2`, `foo.lz4`, `foo.br`) is read as a
one-entry archive; like tar it has no per-entry checksum, and mounting one goes through the same
in-RAM path as 7z/tar/RAR.

✱ Cram never *creates* RAR, and never will: the UnRAR licence forbids using its source to build a
RAR compressor. Cram reads RAR only.

† **`cram t` does not mean the same thing for every format.** *CRC-32*: the checksum stored in the
container is recomputed over the decoded bytes and compared — real content integrity. ZIP stores one
for every entry except a WinZip AES entry written in the **AE-2** form, which deliberately stores no
CRC because the AES authentication code already covers the data; those entries are checked against
that authentication code instead, which fails if any byte of the entry changed. In 7z the CRC field
is optional, so an entry carrying none falls back to the *decode* check. *decode*: Cram recomputes no
checksum of its own, so the check is "every entry decodes cleanly and its decoded length matches its
declared size", plus whatever the underlying decoder rejects. For `.cram`, encrypted packs are
additionally authenticated by their AES-GCM tag and compressed packs by their codec framing — an
unencrypted *stored* pack has neither. So `cram t` catches truncation, structural damage and broken
codec streams on every format, but a **silent in-file bit flip** is guaranteed to be caught only for
ZIP, for 7z entries that carry a stored CRC, and for compressed or encrypted `.cram`. See
[Limitations](#limitations).

‡ **Mount requires the optional Windows feature `Client-ProjFS`**, which is off by default. Enable it
from an elevated PowerShell:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS   # a restart may be required
```

The ProjFS DLL is bound lazily, so every other command works without the feature. *on-demand*:
content is read out of the archive as a file is opened, with no up-front extraction. *≤ 2 GiB RAM*:
7z, tar, RAR and bare compressed streams have no random-access seam, so mounting one **decodes the
whole archive into memory up front** and refuses anything whose total uncompressed size exceeds
2 GiB — extract those instead.

A mount is **read-only**. You can browse it and open files from it, but nothing is ever written back
into the archive, and unmounting removes the mount directory — so a file you edit and save inside it
is discarded, not stored. Copy what you need out of the mount, or use `cram x`.

Signing (`.cramsig`) and Reed-Solomon recovery (`.cramrec`) are sidecars computed over a file's bytes
and work on **any file**, not just Cram's own formats. The self-extracting `.exe` does not:
`make-sfx` checks the payload's magic and refuses anything that is not a `.cram`.

---

## Install

Prebuilt Windows x86-64 binaries — `cram.exe` and `cram-extract.exe` — are attached to the releases
at <https://github.com/lukr54/cram/releases>. They are **not code-signed**, so SmartScreen will warn
on first run (see [Limitations](#limitations)).

### Linux

The `cram` CLI runs on Linux x86-64. Install the latest release binary with:

```sh
curl -fsSL https://raw.githubusercontent.com/lukr54/cram/master/install.sh | sh
```

That drops `cram` into `~/.local/bin` (no root, no daemon, nothing else touched); re-run it to upgrade.
Prefer to read before you pipe to a shell? Download [`install.sh`](install.sh), read it, then run it.
The Linux tarball is also attached to each release if you'd rather place the binary yourself. Archive
**mount** (`cram mount`) is Windows-only (it uses ProjFS); every other verb works identically on Linux.

### Building from source

Cram targets **`x86_64-pc-windows-gnu`** (WinLibs mingw / GCC). No MSVC toolchain is used or
required. The UnRAR C++ dependency needs the linker flags already configured in
[`.cargo/config.toml`](.cargo/config.toml).

```sh
cargo build --release -p cram-cli -p cram-extract
```

That produces, in `target/release/`:

- **`cram.exe`** — the CLI.
- **`cram-extract.exe`** — a standalone `.cram` decoder and the self-extractor stub. Keep it beside
  `cram.exe`; `cram make-sfx` shells out to it and fails without it.

Deploying either binary also needs `libwinpthread-1.dll` alongside it (it lives on the mingw PATH
during development).

On **Linux**, the toolchain is just a system C/C++ compiler (`build-essential` — g++ builds the UnRAR
dependency) and a stable Rust toolchain; there is no mingw and no bundled DLL. Build the same way:

```sh
cargo build --release -p cram-cli -p cram-extract
```

The downloader's TLS uses pure-Rust rustls on Linux, so the binary needs no system OpenSSL.

Optional features are opt-in, so the base build always compiles on a bare mingw toolchain:

| Feature | Effect |
|---|---|
| `zstd-c` | full-range zstd encoder (C libzstd), and the default `.cram` pack codec once enabled. Without it, `.cram` packs use pure-Rust XZ. |
| `download` | enables the `cram dl` segmented downloader. It is a client and opens no listening socket. |
| `libdeflate` | a faster DEFLATE backend (C libdeflate). |

```sh
cargo build --release -p cram-cli --features zstd-c,download
```

`cram --version` prints which optional features a given binary was compiled with. A `zstd-c` build
writes different `.cram` bytes than the pure-Rust default, so it is worth checking before comparing
two archives.

Run the tests with `cargo test`; that is 149 tests across the workspace. `cargo fmt --all -- --check`
and `cargo clippy --workspace --all-targets -- -D warnings` are clean. See
[CONTRIBUTING.md](CONTRIBUTING.md).

---

## Using it

```sh
cram a  backup.cram ./project        # create (adaptive compression; --store / --fast / --best)
cram l  backup.cram                  # list
cram t  backup.cram                  # test integrity (no extract; exits non-zero if bad)
cram x  backup.cram -o ./out         # extract (--skip: see the note below)
cram conv backup.cram backup.zip     # convert to another format
cram mount backup.cram .\view        # mount as a virtual folder; press Enter to unmount
```

### Command reference

```
cram l  <archive>                                 list entries
cram x  <archive> [-o <dir>] [-p <pw>] [--skip]   extract
cram a  <archive> <input...> [-p <pw>]            create [--store|--fast|--best] [--encrypt-names]
cram t  <archive> [-p <pw>]                       test integrity (decode + checksums, no extract)
cram conv <in> <out> [-p <pw>] [--encrypt <pw>]   convert to <out>'s format [--best|--fast|--store]
cram dl <url…|FILE.meta4> [-o <out>] [--extract <dir>] [-n <conns>] [--chunk <mb>]
                                                  segmented download; several URLs are mirrors of
                                                  one file. --discover finds mirrors, --auto ramps
                                                  connections, --sha256 <hex> verifies. Needs a
                                                  build with the `download` feature.
cram mount [--selftest] [-p <pw>] <archive> <dir> mount as a virtual folder (ProjFS)
cram rec <create|verify|repair> <file> …          Reed-Solomon recovery sidecar (.cramrec)
cram sign <file> -k <keyfile>                     write a detached ed25519 signature (.cramsig)
cram verify <file> [--key <hex>]                  verify a signature (pin --key to require a signer)
cram keygen <keyfile>                             create a signing key (prints its public key)
cram make-sfx <archive.cram> <out.exe>            build a self-extracting executable
cram --version                                    version + which optional features are compiled in
```

- `-p <pw>` supplies a password: to read an encrypted source, or to encrypt on create.
- `--skip` leaves a file alone when the destination already holds exactly that entry. A match has to
  be *proven* from a per-entry CRC stored in the container, so it takes effect on ZIP and 7z only; on
  tar, RAR, ISO, a bare compressed file and `.cram` there is no stored CRC to compare against and
  every entry is re-extracted as normal.
- `--encrypt-names` (7z and `.cram` only) hides the file listing as well as the contents.
- The format on create is chosen from the output extension (`.zip` / `.7z` / `.cram` /
  `.tar[.gz|.xz|.bz2|.lz4|.br|.zst]`).

### What a damaged archive does

Extraction is best-effort. A damaged entry does not abort the job: intact entries are written,
each failure is printed by entry name, and **the process exits non-zero**. `cram t` behaves the same
way: it reports how many entries were bad and exits non-zero.

A clean exit code means every entry Cram listed came out — written, or skipped under `--skip` as
already correct — and that each one's decoded length matched the length its container declared. Two
things sit outside that guarantee, and a script should know both:

- A **bare single-stream compressed file** (`foo.gz`, `foo.xz`, …) declares no uncompressed length,
  so there is no length to check against; the check is that the stream decoded cleanly.
- An entry whose stored name cannot be represented safely on Windows — a `..` component, a `:` or a
  NUL byte in any component, or a path thousands of components deep — is **refused**: it is not
  listed, not tested and not extracted, and that on its own does not make the exit code non-zero.
  Archives written on other platforms can legitimately carry such names, so if it matters that
  nothing was left behind, compare `cram l` against the source.

In the CLI, any verb that reads a `.rar` runs the decode in a child process. If that child terminates
abnormally, the command reports it as an error and the shell it was launched from is unaffected.

---

## Design notes

These describe how Cram works, not a measured comparison against anything else. This repository
contains no benchmark harness, so nothing here is a performance claim.

- **Parallel extraction** on the formats with a random-access seam — ZIP, ISO and `.cram` — where
  entries can be decoded independently. Sequential formats (7z, tar, RAR, bare streams) stream
  front-to-back through the same write machinery.
- **`.cram`** applies content-defined chunking, then global BLAKE3-keyed dedup across every input in
  one archive, then compressed packs and a footer index. Dedup is global: identical data anywhere in
  the inputs is stored once, with no dictionary-window limit. Optional encryption is Argon2id +
  AES-256-GCM. Unencrypted `.cram` output is byte-for-byte reproducible; encrypted output is
  deliberately not (a fresh random salt per archive).
- The format is **frozen at v1** and specified normatively in
  [`docs/CRAM_FORMAT.md`](docs/CRAM_FORMAT.md) — that document, not the code, is the contract. The
  decoder in [`crates/cram-extract`](crates/cram-extract) is an independent implementation of the
  same spec.

---

## Limitations

- **`cram test` cannot detect every bit flip.** On an *unencrypted, stored* `.cram` (incompressible
  media with no password), and on `tar` / `.tar.zst`, there is no per-file content checksum, so a
  flipped bit inside a file's content can decode to wrong bytes undetected. Truncation and
  structural damage *are* caught. ISO and RAR are in the same position: Cram computes no checksum of
  its own for either, so the verdict is a clean decode plus a declared-size match, plus whatever the
  underlying decoder rejects. For guaranteed content integrity, use ZIP, 7z, or compressed/encrypted
  `.cram`, or pair any archive with `cram sign` or `cram rec` — both cover the whole file.
- **Mount needs an optional Windows feature.** `Client-ProjFS` is off by default and must be enabled
  from an elevated PowerShell (see ‡ above); a restart may be required. Every other command works
  without it.
- **Mounting 7z / tar / RAR / a bare compressed file decodes the whole archive into RAM**, capped at
  2 GiB; above that the mount is refused. Only ZIP, ISO and `.cram` are projected lazily.
- **A mount is read-only, and its directory is removed on unmount.** Edits saved into a mounted
  folder never reach the archive and do not survive the unmount (see ‡ above).
- **RAR entries larger than 2 GiB are refused.** The RAR decoder hands an entry back in one piece
  rather than in chunks, so an entry has to fit in memory; past 2 GiB Cram reports it as a per-entry
  failure and carries on with the rest of the archive.
- **`cram conv` cannot read a `.cram` entry larger than 512 MiB.** Conversion walks the source entry
  by entry and holds one whole entry in memory, so a `.cram` containing a single file above that
  limit fails to convert ("entry too large to buffer in memory") even though `cram x` extracts the
  same archive fine — extraction streams each entry to disk instead. Extract it and re-archive to
  move such a file into another container.
- **RAR is read-only** and always will be — see ✱ above.
- **Symlinks and other special files are skipped on create.** Classic-container creation covers
  regular files and directories.
- **Timestamps:** extraction restores a file's modification time when the source container records
  one (ZIP, tar, 7z, RAR). `.cram` stores no timestamps at all, by design, because the format is
  reproducible.
- **Nothing is code-signed.** There is no Authenticode certificate, so Windows SmartScreen warns on
  first run of the released binaries. (`cram sign` signs *archives*; that is unrelated to Windows
  executable trust.)
- **Windows-first.** Mount is Windows-only via ProjFS; the rest is portable in principle but built
  and tested for Windows/GNU.

---

## Cram Studio

Cram Studio is a separate Windows desktop application built on this engine. Its source is not in
this repository, and nothing in this repository depends on it.

---

## Repository layout

`crates/cram-core` (engine, format backends and `.cram`), `crates/cram-cli` (the `cram` binary),
`crates/cram-mount` · `cram-recovery` · `cram-sign` (mount and the sidecar tools),
`crates/cram-extract` (standalone decoder / SFX stub), `crates/rdm-core` (the segmented-download
engine behind `cram dl`). [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) explains how they fit
together; every source file carries a module-level doc comment for the local detail.

## Security

Cram parses untrusted input. To report a vulnerability, follow
[SECURITY.md](SECURITY.md) — please do not open a public issue for one.

## License

Dual-licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
Unless you state otherwise, any contribution you submit for inclusion shall be dual-licensed as
above, with no additional terms.

Cram links and redistributes third-party components — the UnRAR C++ engine, the MinGW winpthreads
runtime, and its Rust dependency graph — each under its own licence. See
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
