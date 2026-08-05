# Cram

Cram is a multi-format archive tool. One `cram` command lists, extracts, creates, tests, converts and
mounts archives, signs them and builds parity sidecars for them, finds duplicate files across your
drives, plus a native format (`.cram`) that stores repeated data once and losslessly repacks JPEGs.

This repository is the **engine and the command line**. The `cram` CLI is free and fully featured;
nothing in it is restricted. It is written in Rust, and Windows (the GNU/mingw toolchain), Linux and
Apple Silicon macOS each build and run the full test suite. Archive **mount** is Windows-only (see
[Limitations](#limitations)).
Everything is pure Rust **except the UnRAR C++ decoder**, which is always compiled in because it is
what reads RAR; the optional `zstd-c` feature links C libzstd.

Licensed under **MIT OR Apache-2.0**.

---

## Formats

| | ZIP | 7z | tar(.\*) | ISO 9660 | RAR | `.cram` |
|---|:-:|:-:|:-:|:-:|:-:|:-:|
| List / extract | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Create | ✅ | ✅ | ✅ | ❌ | ❌ ✱ | ✅ |
| Test (integrity) † | CRC-32 / AES auth | CRC-32 if stored | decode | decode | decode | decode |
| Mount as a folder ‡ | on-demand | ≤ 2 GiB RAM | ≤ 2 GiB RAM | on-demand | ≤ 2 GiB RAM | on-demand |
| Encryption | AES-256 | AES-256 | ❌ | ❌ | read | AES-256-GCM |

`tar(.*)` covers plain tar plus gzip, xz, zstd, bzip2, lz4 and brotli. A **bare single-stream
compressed file** (`foo.gz`, `foo.xz`, `foo.zst`, `foo.bz2`, `foo.lz4`, `foo.br`) is read as a
one-entry archive; like tar it has no per-entry checksum, and mounting one goes through the same
in-RAM path as 7z/tar/RAR.

✱ Cram never *creates* RAR, and never will: the UnRAR licence forbids using its source to build a
RAR compressor. Cram reads RAR only.

† **`cram t` does not mean the same thing for every format.** *CRC-32*: the checksum stored in the
container is recomputed over the decoded bytes and compared, real content integrity. ZIP stores one
for every entry except a WinZip AES entry written in the **AE-2** form, which stores no
CRC because the AES authentication code already covers the data; those entries are checked against
that authentication code instead, which fails if any byte of the entry changed. In 7z the CRC field
is optional, so an entry carrying none falls back to the *decode* check. *decode*: Cram recomputes no
checksum of its own, so the check is "every entry decodes cleanly and its decoded length matches its
declared size", plus whatever the underlying decoder rejects. For `.cram`, encrypted packs are
additionally authenticated by their AES-GCM tag and compressed packs by their codec framing, an
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
7z, tar, RAR and bare compressed streams have no random-access hand-off point, so mounting one **decodes the
whole archive into memory up front** and refuses anything whose total uncompressed size exceeds
2 GiB, extract those instead.

**The archive is never modified.** By default a mount is for reading: unmounting removes the mount
directory, so a file you edit inside it is discarded. If the folder ends up holding files that are
not in the archive, it is kept rather than deleted, and Cram says so.

`cram mount --writable` keeps them on purpose. The archive stays the immutable base and the mount
folder becomes everything that has diverged from it, so a program can write settings, saves and new
files into the mount and find them again next time. Re-mounting the same archive on the same folder
resumes over what is there: a modified file wins over the archive's copy, an untouched one still
comes from the archive, and a deleted one stays deleted. Nothing is ever written back into the
`.cram`; **deleting the mount folder is how you reset to a pristine archive**, and the only way,
since ProjFS has no way to un-tag a virtualization root.

That last point matters for the case this was built for: a game archived once and mounted rather than
installed keeps its saves and config beside it, and the archive it reads from cannot be changed by
playing. It only captures writes that land *inside* the mount, so anything a program writes to
`%APPDATA%` or the registry still goes there.

Signing (`.cramsig`) and Reed-Solomon recovery (`.cramrec`) are sidecars computed over a file's bytes
and work on **any file**, not just Cram's own formats. The self-extracting `.exe` does not:
`make-sfx` checks the payload's magic and refuses anything that is not a `.cram`.

---

## Install

**No binary release has been published yet.** There is no tag, the Releases page is empty, and
everything in this section starts working the moment a `v*` tag is pushed. Until then, build from
source — see [Building from source](#building-from-source) below.

Once the first release exists, Windows x86-64 binaries ship as a single zip,
`cram-<tag>-x86_64-pc-windows-gnu.zip`, alongside `SHA256SUMS.windows` and a version-free
`cram-latest-x86_64-pc-windows-gnu.zip` so a permanent
`https://github.com/lukr54/cram/releases/latest/download/cram-latest-x86_64-pc-windows-gnu.zip`
link keeps working. The zip holds `cram.exe`, `cram-extract.exe`, `cram_shell.dll` (the Explorer
right-click menu, which does nothing until you run `cram shell install`) and `libwinpthread-1.dll`,
which `cram.exe` links against and will not start without. Keep the contents together. They are
**not code-signed**, so SmartScreen will warn on first run (see [Limitations](#limitations)).

`cram update` then fetches the next release, checks it against the SHA-256 the release publishes,
and replaces itself. It refuses to install anything it cannot verify. Run against an empty Releases
page today it reports that no published release exists yet.

### Linux and macOS

The `cram` CLI runs on Linux x86-64 and on Apple Silicon macOS. Once a release is published, install
it with:

```sh
curl -fsSL https://raw.githubusercontent.com/lukr54/cram/main/install.sh | sh
```

The script resolves the newest release from the GitHub API, so it exits with "could not determine the
latest release" until one exists; build from source in the meantime.

It installs two binaries into `~/.local/bin` (no root, no daemon, nothing else touched) — `cram` and
`cram-extract`, which `cram make-sfx` shells out to — and re-running it upgrades them.
Prefer to read before you pipe to a shell? Download [`install.sh`](install.sh), read it, then run it.
The tarball is also attached to each release if you'd rather place the binary yourself. Archive
**mount** (`cram mount`) is Windows-only (it uses ProjFS); every other verb works identically.

The macOS binaries are **not signed or notarised**, so a download is quarantined by Gatekeeper; 
`install.sh` clears that flag on the file it just fetched, and building from source avoids it
entirely. Drive detection there goes through `diskutil`, which matters because it is what decides
between one sequential reader and several parallel ones: getting it wrong on an external spinning
disk (where a large collection usually lives) causes seek thrash rather than speed.

### Building from source

Cram targets **`x86_64-pc-windows-gnu`** (WinLibs mingw / GCC). No MSVC toolchain is used or
required. The UnRAR C++ dependency needs the linker flags already configured in
[`.cargo/config.toml`](.cargo/config.toml).

The stock Windows rustup default is MSVC, which is **not** the configuration this ships as: the
linker flags in `.cargo/config.toml` are scoped to `x86_64-pc-windows-gnu`, so a default build
silently drops them and fails on `multiple definition of pthread_*`. Select the toolchain first:

```sh
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup default stable-x86_64-pc-windows-gnu
cargo build --release -p cram-cli -p cram-extract -p cram-shell
```

CI pins the same target through `CARGO_BUILD_TARGET` rather than relying on the default.

That produces, in `target/release/`:

- **`cram.exe`**, the CLI.
- **`cram-extract.exe`**, a standalone `.cram` decoder and the self-extractor stub. Keep it beside
  `cram.exe`; `cram make-sfx` shells out to it and fails without it.
- **`cram_shell.dll`**, the Explorer right-click handler. `cram shell install` refuses to run
  without it beside `cram.exe`, which is why it is in the build line above.

Deploying either binary also needs `libwinpthread-1.dll` alongside it (it lives on the mingw PATH
during development).

On **Linux**, the toolchain is just a system C/C++ compiler (`build-essential`; g++ builds the UnRAR
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
| `phash` | perceptual image hashing, so `cram dedup --similar` can flag visually-alike photos. Pure Rust, but a large dependency tree, hence opt-in. |

```sh
cargo build --release -p cram-cli --features zstd-c,download
```

`cram --version` prints which optional features a given binary was compiled with. A `zstd-c` build
writes different `.cram` bytes than the pure-Rust default, so it is worth checking before comparing
two archives.

Run the tests with `cargo test`; that is 212 tests across the workspace, and 224 with the features
the release is built with (`download,zstd-c,phash`), which compile code the default build leaves out.
Exact counts drift with every commit; what matters is that the suite is green.
`cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` are clean.
See [CONTRIBUTING.md](CONTRIBUTING.md).

---

## Using it

```sh
cram a backup.cram ./project        # create (adaptive compression; --fast / --auto / --small)
cram l backup.cram                  # list
cram t backup.cram                  # test integrity (no extract; exits non-zero if bad)
cram x backup.cram -o ./out         # extract (--skip: see the note below)
cram conv backup.cram backup.zip     # convert to another format
cram mount backup.cram .\view        # mount as a virtual folder; press Enter to unmount
```

`cram a` writes a new archive. If a file of that name is already there it stops and changes nothing;
pass `--overwrite`, or `-y`, to replace it. `cram conv` and `cram make-sfx` guard their output the
same way.

### Command reference

```
cram l  <archive>                                 list entries
cram x  <archive> [-o <dir>] [-p <pw>] [--skip]   extract
cram a  <archive> <input...> [-p <pw>]            create [--fast|--auto|--small|--store]
           [--overwrite]                          [--overwrite] replaces an existing <archive>
cram t  <archive> [-p <pw>]                       test integrity (decode + checksums, no extract)
cram conv <in> <out> [-p <pw>] [--encrypt <pw>]   convert to <out>'s format [--fast|--auto|--small]
           [--overwrite]                          [--overwrite] replaces an existing <out>
cram dl <url…|FILE.meta4> [-o <out>] [--extract <dir>] [-n <conns>] [--chunk <mb>]
                                                  segmented download; several URLs are mirrors of
                                                  one file. --discover finds mirrors, --auto ramps
                                                  connections, --sha256 <hex> verifies. Needs a
                                                  build with the `download` feature.
cram dedup <folder|file…> [--similar]             find duplicate files across folders and drives.
           [--similar-distance <0-15>]            Previews by default; --link (hard-link) or
           [--link] [--quarantine <dir>]          --quarantine <dir>, each with --apply, reclaim
           [--apply] [--min-size <bytes>]         the space. --similar also flags visually-alike
           [--json]                               images for human review.
cram mount [--selftest] [-p <pw>] <archive> <dir> mount as a virtual folder (ProjFS)
cram rec <create|verify|repair> <file> …          Reed-Solomon recovery sidecar (.cramrec)
cram sign <file> -k <keyfile>                     write a detached ed25519 signature (.cramsig)
cram verify <file> [--key <hex>]                  verify a signature (pin --key to require a signer)
cram keygen <keyfile>                             create a signing key (prints its public key)
cram make-sfx <archive.cram> <out.exe>            build a self-extracting executable
           [--overwrite]                          [--overwrite] replaces an existing <out.exe>
cram shell <install|uninstall|status>             add or remove Cram's Explorer right-click menu
                                                  (Windows; writes under HKCU only, no elevation)
cram update [--check] [--force]                   download the latest published release, verify its
                                                  published SHA-256 and replace this install.
                                                  --check reports and changes nothing. Needs a
                                                  build with the `download` feature.
cram --version                                    version + which optional features are compiled in
```

- `-p <pw>` supplies a password: to read an encrypted source, or to encrypt on create.
- `--skip` leaves a file alone when the destination already holds exactly that entry. A match has to
  be *proven* from a per-entry CRC stored in the container, so it takes effect on ZIP and 7z only; on
  tar, RAR, ISO, a bare compressed file and `.cram` there is no stored CRC to compare against and
  every entry is re-extracted as normal.
- `--encrypt-names` (7z and `.cram` only) hides the file listing as well as the contents.
- `--overwrite`, alias `-y`, lets `cram a`, `cram conv` and `cram make-sfx` write over a file that
  already exists. Without it they refuse and exit non-zero, leaving the file as it was. `cram a` is
  spelled like 7-Zip's *add to archive* but creates a new one, which is the case the guard exists
  for.
- The format on create is chosen from the output extension (`.zip` / `.7z` / `.cram` /
  `.tar[.gz|.xz|.bz2|.lz4|.br|.zst]`).
- `cram l`, `cram x`, `cram t` and `cram conv` refuse an argument they do not recognise instead of
  ignoring it. `cram x bundle.zip src/a.txt` used to extract the whole archive, because selecting
  individual entries is not supported; it now fails and says why. An archive whose name starts with
  `-` is still read as the archive, and `--` before it states that explicitly.

### Photos: ~23% smaller, and still byte-for-byte the same files

Creating a `.cram` losslessly recompresses JPEGs. A photo's data is already entropy-coded, which is
why zip and 7z gain essentially nothing on a photo library. Measured 2026-08-04 on one folder of 34 phone photos
(26.1 MB, 8 and 12 megapixel JPEGs): ZIP and 7z both produced output *fractionally larger* than the
originals, `tar.xz` managed 2.7%, and the same folder as a `.cram` was **23.6% smaller** with every
file extracting byte-identical. That is a single folder, not a benchmark, and the harness is not in
this repository; `cram a` prints the real ratio for your own files.

That is one sample, not a benchmark. Expect roughly this range on ordinary photos, but the exact
figure depends on the images; `cram a` prints the real ratio for your own files.

Nothing is traded away for it. The JPEG's entropy coding is redone with a stronger coder, and
extraction reconstructs the **original file byte-for-byte**, same bytes, same EXIF, same checksum.
It is not "visually lossless"; it is the file you put in. Every candidate is verified to round-trip
*before* it is stored, and anything that fails verification is stored untouched, so the worst case is
that a file simply isn't shrunk.

It is on by default; `cram a --no-recompress` turns it off. Archives that use it declare format v2
(see [docs/CRAM_FORMAT.md](docs/CRAM_FORMAT.md)), which older readers refuse outright rather than
misread, and the standalone `cram-extract` recovery tool reverses it too, so a photo archive stays
recoverable with the small independent decoder.

### Finding duplicates across drives

`cram dedup` answers a different question from the rest of the tool: how much of this pile is the
same file twice. It is aimed at the case where a large collection has
accreted over years and drives, the same photo under a dozen random names, in folders nobody
remembers copying.

```sh
cram dedup D:\photos E:\backup F:\old-drive
```

**It only reports.** Nothing is deleted, moved, or linked. The output is duplicate sets, largest
saving first, and the total space you would get back by keeping one copy of each.

It is built to stay cheap on collections far too large to hash end to end. Three gates run in order:
a file whose **size** is unique in the whole set cannot have a byte-identical twin and is never read
at all; same-size files are separated by a **partial hash** of their first and last 64 KiB; only what
survives both is read in full and confirmed with **BLAKE3**. On a real pile the vast majority of bytes
are never touched, the run prints how much it actually had to read. Reads are also scheduled per
drive: every volume is worked at once, but with one sequential reader on a spinning disk and several
on an SSD, because parallel reads make an HDD slower rather than faster.

`--similar` additionally finds images that *look* the same without being byte-identical, a resized
copy, a re-save at lower quality, the version a messaging app recompressed. These are reported
**separately and are never counted as reclaimable space**, because a perceptual hash cannot tell a
redundant re-encode from two different frames of a burst. They are a shortlist to look through by
hand. `--similar-distance` tunes how alike is alike (0 = identical
hash, default 8); it needs a build with the `phash` feature. HEIC/HEIF and camera RAW are not decoded
for similarity (that needs a C library), though they are still covered by exact-duplicate detection,
which never decodes anything.

#### Reclaiming the space

Two actions turn the report into free space. Both **preview by default** and do nothing until you add
`--apply`, and neither ever deletes a file.

```sh
cram dedup D:\photos --link                       # preview
cram dedup D:\photos --link --apply               # do it
cram dedup D:\photos E:\backup --link --quarantine D:\dupes --apply
```

`--link` replaces a duplicate with a **hard link** to the copy being kept. Every filename and folder
stays exactly where it was, so nothing disappears from view, while the redundant copies stop taking
up room. Its one caveat: linked paths
are one file, so an editor that rewrites a photo *in place* changes it under every name; tools that
save a new file (almost all of them) are unaffected.

`--quarantine <dir>` **moves** duplicates into a folder instead, rebuilding their original path
underneath so it is obvious what came from where and putting one back is a plain move. Nothing is
freed until you delete that folder yourself. Hard links cannot span filesystems, so copies on a
different drive from the keeper need this; passing both flags links where it can and quarantines the
rest.

`--keep shortest|oldest|first` chooses which copy survives (default: copy-looking names like
`x (2).jpg` lose, then the shortest path wins). The keeper is printed for every action in the
preview, worth reading before `--apply`, since no rule can know which folder *you* consider the
canonical one.

Whatever the plan says, each pair is **re-hashed at the moment of action** and skipped if it no longer
matches, so a plan made hours earlier can never act on a file that changed in the meantime.

### The Explorer right-click menu (Windows)

```powershell
cram shell install      # cram shell status / cram shell uninstall
```

Right-clicking an archive then offers **Extract here**, **Extract to `<name>\`** and **Test
archive**; right-clicking anything else offers to add it to a `.cram` or a `.zip`. Everything sits
under one **Cram** submenu, and each verb runs the same `cram` command you would have typed.
A container document (`.docx`, `.jar`, `.epub`) gets both sets, being legitimately both. If Cram
Studio is installed beside the CLI, two further entries appear, **Open in Cram Studio** and **Add to
archive…**, which open Studio rather than running a `cram` command.

It registers under `HKCU` only, so there is no elevation prompt and nothing is changed for other
accounts. `cram shell uninstall` removes it, and `cram shell status` reports whether the handler is
registered and whether the DLL it points at still exists.

On **Windows 11 it appears under "Show more options"** (or Shift+F10), which is where the shell puts
every context-menu handler. Reaching the compact top-level menu needs a different mechanism
(`IExplorerCommand` in a signed package) that Cram does not currently ship.

### What a damaged archive does

Extraction is best-effort. A damaged entry does not abort the job: intact entries are written,
each failure is printed by entry name, and **the process exits non-zero**. `cram t` behaves the same
way: it reports how many entries were bad and exits non-zero.

A clean exit code means every entry Cram listed came out, written, or skipped under `--skip` as
already correct, and that each one's decoded length matched the length its container declared. Two
things sit outside that guarantee, and a script should know both:

- A **bare single-stream compressed file** (`foo.gz`, `foo.xz`, …) declares no uncompressed length,
  so there is no length to check against; the check is that the stream decoded cleanly.
- An entry whose stored name cannot be represented safely on Windows is **refused**: a `..`
  component, a `:` or a NUL byte in any component, or a path thousands of components deep. It is not
  listed, not tested and not extracted, and that on its own does not make the exit code non-zero.
  Archives written on other platforms can legitimately carry such names, so if it matters that
  nothing was left behind, compare `cram l` against the source.

In the CLI, any verb that reads a `.rar` runs the decode in a child process. If that child terminates
abnormally, the command reports it as an error and the shell it was launched from is unaffected.

---

## Design notes

These describe how Cram works, not a measured comparison against anything else. This repository
contains no benchmark harness, so nothing here is a performance claim.

- **Parallel extraction** on the formats with a random-access boundary, ZIP, ISO and `.cram`, where
  entries can be decoded independently. Sequential formats (7z, tar, RAR, bare streams) stream
  front-to-back through the same write machinery.
- **`.cram`** applies content-defined chunking, then global BLAKE3-keyed dedup across every input in
  one archive, then compressed packs and a footer index. Dedup is global: identical data anywhere in
  the inputs is stored once, with no dictionary-window limit. Optional encryption is Argon2id +
  AES-256-GCM. Unencrypted `.cram` output is byte-for-byte reproducible; encrypted output is
  not (a fresh random salt per archive).
- The format is **frozen and versioned**: v1, plus v2, which adds only the per-entry transform byte
  that carries lossless JPEG recompression. A writer emits v2 only when a transform was actually used,
  and a reader that meets a version it does not know refuses the archive rather than guessing. Both
  versions are specified normatively in
  [`docs/CRAM_FORMAT.md`](docs/CRAM_FORMAT.md), that document, not the code, is the contract. The
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
  `.cram`, or pair any archive with `cram sign` or `cram rec`; both cover the whole file.
- **Mount needs an optional Windows feature.** `Client-ProjFS` is off by default and must be enabled
  from an elevated PowerShell (see ‡ above); a restart may be required. Every other command works
  without it.
- **Mounting 7z / tar / RAR / a bare compressed file decodes the whole archive into RAM**, capped at
  2 GiB; above that the mount is refused. Only ZIP, ISO and `.cram` are projected lazily.
- **A mount is read-only, and its directory is removed on unmount.** Edits saved into a mounted
  folder never reach the archive and do not survive the unmount (see ‡ above).
- **A large RAR entry is written to a scratch file first.** The RAR decoder hands an entry back in
  one piece rather than in chunks, so anything too big to hold in memory is extracted by UnRAR
  straight to a scratch file beside the archive and streamed from there, then deleted. The threshold
  comes from free memory. It costs one extra write and read for those entries, and no entry is
  refused for its size.
- **`cram conv` cannot read a `.cram` entry larger than 512 MiB.** Conversion walks the source entry
  by entry and holds one whole entry in memory, so a `.cram` containing a single file above that
  limit fails to convert ("entry too large to buffer in memory") even though `cram x` extracts the
  same archive fine, extraction streams each entry to disk instead. Extract it and re-archive to
  move such a file into another container.
- **RAR is read-only** and always will be, see ✱ above.
- **Symlinks and other special files are skipped on create.** Classic-container creation covers
  regular files and directories.
- **Timestamps:** extraction restores a file's modification time when the source container records
  one (ZIP, tar, 7z, RAR). `.cram` stores no timestamps at all, by design, because the format is
  reproducible.
- **Nothing is code-signed.** There is no Authenticode certificate, so Windows SmartScreen warns on
  first run of the released binaries. (`cram sign` signs *archives*; that is unrelated to Windows
  executable trust.)
- **Platform support is not uniform.** Windows (`x86_64-pc-windows-gnu`), Linux
  (`x86_64-unknown-linux-gnu`) and macOS (`aarch64-apple-darwin`) each build and run the full test
  suite, clippy and the fuzz smoke tests on their own CI runner. What differs is **mount**, which is
  Windows-only because it is built on ProjFS; every other verb behaves the same on all three.

---

## Cram Studio

Cram Studio is a separate Windows desktop application built on this engine. Its source is not in
this repository, and nothing in this repository depends on it.

**Studio is proprietary and sold under its own EULA.** The MIT OR Apache-2.0 licence on this page
covers the engine and the CLI in this repository and nothing else. When the Studio installer is
published as an asset on this repository's Releases page, that installer is not covered by those
licences.

---

## Repository layout

`crates/cram-core` (engine, format backends and `.cram`), `crates/cram-cli` (the `cram` binary),
`crates/cram-mount` · `cram-recovery` · `cram-sign` (mount and the sidecar tools),
`crates/cram-extract` (standalone decoder / SFX stub), `crates/cram-shell` (the Explorer
context-menu handler), `crates/rdm-core` (the segmented-download
engine behind `cram dl`). [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) explains how they fit
together; every source file carries a module-level doc comment for the local detail.

## Security

Cram parses untrusted input. To report a vulnerability, follow
[SECURITY.md](SECURITY.md), please do not open a public issue for one.

## License

Dual-licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
Unless you state otherwise, any contribution you submit for inclusion shall be dual-licensed as
above, with no additional terms.

Cram links and redistributes third-party components, the UnRAR C++ engine, the MinGW winpthreads
runtime, and its Rust dependency graph; each under its own licence. See
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
