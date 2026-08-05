# Cram, architecture

How the engine and the `cram` CLI are put together, for someone reading or changing the code. The
normative `.cram` container spec is [`CRAM_FORMAT.md`](CRAM_FORMAT.md); what the tool does from a
user's seat is [`README.md`](../README.md); how to build and test is
[`CONTRIBUTING.md`](../CONTRIBUTING.md). Every source file carries a module-level doc comment
(`//! …`) explaining that piece; this document maps them.

---

## 1. The core

Cram reads and writes many formats, but almost none of the interesting code is format-specific. A
backend only knows how to yield entry **metadata** and entry **bytes**. Everything else lives once in
the engine: safe output paths, overwrite and skip policy, progress, cancellation, the parallel
scheduler. So every format inherits all of it, and adding a format means implementing two small
traits rather than re-plumbing the world.

The traits are in [`cram-core/src/reader.rs`](../crates/cram-core/src/reader.rs) and
[`writer.rs`](../crates/cram-core/src/writer.rs):

```
ArchiveReader          the read core (every readable container)
  ├ format()           the detected Container × Codec
  ├ entries()          the full member list (a cheap header/central-directory scan)
  ├ next_entry()       pull the next member as a streamed (metadata, body)   ← sequential path
  └ as_random_access() Some(..) for seekable containers (ZIP, .cram, ISO)    ← unlocks the parallel path

RandomAccessReader     the per-entry capability (Send + Sync)
  ├ entries()
  ├ copy_entry(i, w)   decode entry i straight into a writer, from its own handle (safe to call
  │                    concurrently), the boundary the parallel extractor fans out over
  └ read_range(i,o,l)  a byte-range of an entry's uncompressed stream; the mount / on-access primitive

ArchiveWriter          the write core (every creatable container, never RAR)
  ├ add_file(entry, body, hint)
  ├ add_dir(entry)
  └ finish() -> CreateReport
```

`copy_entry` takes a `&mut dyn Write` rather than returning a `Box<dyn Read>` because the `zip`
crate's per-entry reader borrows its archive: handing back an owned reader would force either
buffering a whole entry in RAM (bad for a few-huge-entries archive × N workers) or a self-referential
wrapper. Injecting the writer streams instead, and keeps file creation, overwrite policy and progress
in the engine.

---

## 2. Crate map

| Crate | What it is |
|---|---|
| **`cram-core`** | the engine, every read/write backend, and the `.cram` format. The library everything else builds on. |
| **`cram-cli`** | the single `cram` binary. Three dispatch mechanisms: it parses the archive verbs itself (calling `cram-core`), delegates the sidecar/mount tools to their crates' `cli::main`, and **execs** the co-located `cram-extract` stub for `make-sfx` rather than linking it, which is what preserves the no-shared-code rule of §6. It also owns `cram update` (`update.rs`): a GitHub release fetch whose URL is constructed locally rather than echoed from the API, a mandatory SHA-256 check before anything is written, and move-aside replacement of the running binary. `CRAM_UPDATE_REPO` overrides the repository for tests. |
| **`cram-mount`** | ProjFS mount, present an archive as a browsable virtual folder. Its own crate so the Windows/ProjFS specifics stay contained. |
| **`cram-recovery`** | Reed-Solomon parity **sidecar** (`.cramrec`): store parity, later repair bit-rot or truncation. Works on any file. |
| **`cram-sign`** | detached **ed25519** signature sidecar (`.cramsig`), authorship plus integrity. Works on any file. |
| **`cram-extract`** | a standalone, dependency-minimal `.cram` decoder, which also serves as the self-extractor (SFX) stub. Shares no code with `cram-core` by design (§6). |
| **`cram-shell`** | the Explorer right-click menu: a COM `IContextMenu` handler built as a cdylib and loaded **into explorer.exe** (§9). Depends only on the `windows` crate, not on the engine. |
| **`rdm-core`** | the segmented, resumable, multi-source **download engine** (library, no GUI) behind `cram dl` and extract-while-download. Vendored in-tree. |

`cram-extract`'s whole manifest is five decode-only, pure-Rust crates: `lzma-rust2` (XZ), `ruzstd`
(zstd), `aes-gcm`, `argon2` and `lepton_jpeg`, the last so it can reverse the writer's lossless JPEG
recompression, since a photo archive the recovery tool cannot read would defeat the point of having
one. No C or C++ anywhere in that graph. What it has none of is *shared code* with `cram-core`, no
writer, chunker, hasher or thread pool, because a recovery reader needs none of that.

`rdm-core` is vendored rather than referenced by an out-of-tree path because Cargo loads a path
dependency's manifest during workspace resolution **even when the dependency is optional and its
feature is off**, so an external path breaks every crate in the workspace on a fresh clone.

`cram-mount`, `cram-recovery` and `cram-sign` are libraries whose command-line logic lives in a
`cli::main` the unified `cram` binary calls. The workspace's other binaries are `cram-extract` and
`calibrate`, an internal hardware-measurement tool in `cram-core`. `cram-shell` is neither a library
for the CLI nor a binary: it is a cdylib the operating system loads, and the only thing `cram-cli`
does with it is write the registry keys that point at it.

---

## 3. The read path

```
sniff::sniff_path(path)          magic bytes → Format (extension only as a tiebreaker)
  → formats::open(path, fmt, pw) → Box<dyn ArchiveReader>
    → engine::extract(...)
        as_random_access()? ─ Some → engine::parallel   (rayon pool, per-entry own handle, largest-first)
                             └ None → engine::sequential (one entry at a time via next_entry)
```

[`sniff.rs`](../crates/cram-core/src/sniff.rs) detects by **magic bytes**; the extension is only a
tiebreaker, and is what distinguishes a `.tar.gz` from a bare `.gz` (identical magic). Magic always
wins, so a `.zip` that is really a RAR is handled as a RAR.
[`formats/mod.rs`](../crates/cram-core/src/formats/mod.rs) is the single dispatch point from a
`Format` to a concrete reader or writer.

**Random-access formats, ZIP, `.cram`, ISO 9660; take the parallel path**
([`engine/parallel.rs`](../crates/cram-core/src/engine/parallel.rs)): a rayon pool, largest-group-first
scheduling to keep it balanced (a group's weight is the sum of its entries, since a group runs in
sequence), and every worker opening its own handle via `copy_entry`. Those three
containers can address an entry without reading what precedes it, which is what makes independent
per-entry workers possible.

**Everything else, tar, 7z, RAR, a bare compressed stream; takes the sequential path**
([`engine/sequential.rs`](../crates/cram-core/src/engine/sequential.rs)), one entry at a time. These
are front-to-back streams with no seek interface: entry *n* can only be decoded by decoding what precedes
it, so there is nothing to fan out over.

The two paths share the write machinery in
[`engine/mod.rs`](../crates/cram-core/src/engine/mod.rs), `restore_mtime`, the `ProgressWriter` that
reports bytes and aborts on cancellation, the `skip` check for a destination that already matches; 
plus one output-path resolver, `EntryPath::join_under` in
[`model.rs`](../crates/cram-core/src/model.rs). Directories are the piece each path does for itself:
sequential creates parents as entries arrive, parallel materializes every directory up front so empty
ones survive. Both defer directory mtimes to a final pass, because writing a child bumps its parent's
mtime.

The parallel path folds entries by **destination path** before scheduling, since two entries can map
to one on-disk file, duplicate names (legal in ZIP), case-variants on NTFS, Win32 trailing-dot/space
normalization. A colliding group is **serialised onto one task in archive order** and every member is written; the
last writer owns the file. Nothing is dropped and nothing counts as skipped.

Keeping only the last entry per folded key was tried and removed: it destroyed files whenever the
fold was harsher than the target filesystem's own rule, and a four-pair archive lost three files
while exiting 0.

### Worker count

[`hw.rs`](../crates/cram-core/src/hw.rs) supplies the pool size. `HwProfile::detect_for(dest)`
profiles the **destination** drive, the disk actually being written to; for cores, RAM, and that
drive's media (SSD/HDD via seek penalty) and bus, so extracting across drives plans for the target
volume rather than for wherever the process happens to be running. Codec rates and a measured
sequential write ceiling come from a **one-time calibration**
cached in `%APPDATA%\cram\profile.toml`, keyed by a schema version and a machine fingerprint so a
roaming profile is re-measured rather than misapplied. The write-ceiling probe is bounded and is
skipped when the destination is short on free space, a calibration must never be what fills someone's
disk; absence in the profile means "not measured", never `0`. `hw::derive_plan` turns those inputs
into a `Plan`, and `engine::parallel` sizes its pool from `plan.workers`. The `calibrate` binary runs
the same measurements standalone.

### `cram test`

[`engine/verify.rs`](../crates/cram-core/src/engine/verify.rs) mirrors the same dispatch: it decodes
every entry, writing nothing to disk, and checks what the container makes checkable. It must use
`copy_entry` for random-access formats, `next_entry` for `.cram` materializes a whole entry body in
memory and caps its size, so verifying a large healthy `.cram` entry through it would wrongly fail.

What "verified" means is per format, and the difference matters:

- **ZIP / 7z**, where the entry carries a stored CRC-32 it is recomputed over the decoded bytes and
  compared: real content integrity. Two kinds of entry carry none. A WinZip **AES** entry written in
  **AE-2** form stores `0` in place of the CRC and omits it deliberately, because the AES
  authentication code already proves the data is intact and a plaintext CRC would leak information
  about a short entry (Cram's ZIP writer emits AE-2 for entries under 20 bytes); an encrypted entry
  whose stored CRC is `0` and whose size is non-zero is therefore read as carrying *no* checksum, and
  is verified by that authentication instead, a mismatch fails the decode itself. 7z's per-entry CRC
  is optional in the format, and an entry stored without one gets only the decode-plus-declared-size
  check.
- **tar** (including `.tar.zst` and the other wrapped forms), no per-entry checksum exists in the
  format, so the check is a clean full decode plus a declared-size match. That catches truncation and
  a broken codec stream, not an arbitrary bit flip.
- **ISO 9660**, the format carries no per-file checksum and file data is a plain copy of the extent,
  with no codec framing to fail either, so `cram test` confirms the structure and the declared sizes
  and **cannot** detect a bit flip inside a file. For content integrity on an ISO, pair with
  `cram sign` or `cram rec`.
- **`.cram`**, every pack must decode cleanly. Encrypted packs are authenticated by their AES-GCM
  tag and compressed packs by their codec framing, but an **unencrypted, stored** pack (what
  incompressible media compresses to) carries no per-chunk checksum in the frozen format, so
  `cram test` confirms it decodes structurally and cannot detect an in-place bit flip inside it. For
  guaranteed content integrity on such archives, pair with `cram sign` or `cram rec`, which cover the
  whole file's bytes.

---

## 4. The write path

```
engine::create walks the source tree → a member list (dirs before children, sorted for determinism)
  → probe::classify_file per entry (store-vs-compress) when the level is Auto
    → formats::create(path, fmt, opts) → Box<dyn ArchiveWriter>
      → add_dir / add_file(entry, body, hint) per member → finish() -> CreateReport
```

The adaptive **probe** ([`probe.rs`](../crates/cram-core/src/probe.rs)) classifies each file in two
tiers, cheapest first: an extension list for formats that are essentially always incompressible (or
reliably compressible), then, for unknown extensions, a small content sample measured with a fast
DEFLATE pass and a Shannon-entropy short-circuit. Already-compressed media is stored verbatim rather
than re-crushed for no gain. Backends that cannot vary the method per entry (tar) ignore the per-entry
hint and use the aggregate summary to pick one level for the whole stream.

Creation writes to a sibling `<dest>.cram-partial` and renames on success. Writers `File::create`
their target immediately, so staging beside the destination is what keeps a pre-existing archive
intact until the new one is complete, and what makes a failed create leave the old file untouched.
Same directory means same volume, so the rename is atomic.

**Symlinks and other special files are skipped on create**; only regular files and directories are
archived. They are skipped **silently**, nothing counts or names them in the `CreateReport` or in
the CLI's `created …` line, so an archive of a tree containing symlinks is quietly missing those
members and there is no runtime signal of it.

`convert` ([`engine/convert.rs`](../crates/cram-core/src/engine/convert.rs)) is the read and write
spines composed: read any source front-to-back and stream each entry into a destination writer, so
every readable × writable pair works without per-pair code. One limit: a **bare single-stream** source
of unknown length must be buffered to learn its size before a size-trusting destination will accept
it, and convert refuses above **2 GiB** rather than hold that much in RAM. Encryption is not
inherited: converting an encrypted source produces a plaintext archive unless the caller supplies
`--encrypt` for the destination.

`dedup` ([`engine/dedup.rs`](../crates/cram-core/src/engine/dedup.rs)) works on loose files rather
than archives: it finds the same file in several places across folders and drives. Three gates run in
order so most of the data is never read, a unique file size rules out a byte-identical twin outright,
same-size files are separated by a partial hash of their first and last 64 KiB, and only the
survivors are read in full and confirmed with BLAKE3. Candidates are grouped by the volume they live
on and every volume is worked at once, but the reader count within a volume comes from the same
[`hw`](../crates/cram-core/src/hw.rs) media detection the extractor uses: one sequential reader on a
spinning disk, several on an SSD.

`reclaim` ([`engine/reclaim.rs`](../crates/cram-core/src/engine/reclaim.rs)) is the acting half, kept
in its own module so the read-only scan cannot accidentally reach a destructive path. `plan` filters
on `GroupKind::Exact`, which is what keeps perceptual "similar" findings structurally unreachable from
any action, and `apply` re-hashes each pair at the moment of action rather than trusting a plan that
may be hours old. Replacing a file with a hard link is transactional: the link is built under a
temporary name first, then swapped by an atomic rename on Unix, while Windows (which will not rename
onto an existing file) moves the original aside and removes it only once the link is in place,
rolling back on any failure. A cross-device quarantine copies, verifies the copy, and only then
removes the original.

`.cram` also applies one reversible **per-entry transform**: a JPEG is stored as a Lepton stream and
reconstructed byte-for-byte on the way out. The transform runs before chunking, so two copies of one
photo still dedup to a single stored copy, and the entry's recorded size stays the *original*
length so listings and extraction report the file the user gets. Because a Lepton stream is a single
arithmetic-coded unit that cannot be seeked into, `read_range` (the mount primitive) reconstructs the
whole entry and slices the result. Archives using a transform declare format v2; see
[`CRAM_FORMAT.md`](CRAM_FORMAT.md).

---

## 5. Formats

Read: ZIP, 7z, tar (with gzip / xz / zstd / bzip2 / lz4 / brotli), ISO 9660, RAR, bare single-stream
compressed files, and `.cram`. Write: ZIP, 7z, tar, `.cram`.

**RAR is read-only and always will be**, creating RAR archives is forbidden by the UnRAR license.
`Format::is_writable` returns false for it and `formats::create` rejects it before any backend is
constructed.

---

## 6. The `.cram` format

`.cram` is the one container Cram defines rather than merely interoperates with. The normative
byte-level spec is [`CRAM_FORMAT.md`](CRAM_FORMAT.md); the code is
[`formats/cram.rs`](../crates/cram-core/src/formats/cram.rs). At a glance:

- **Content-defined chunking** (FastCDC v2020) splits every input into variable-length chunks.
- **Global dedup**: each chunk is identified by its BLAKE3 hash and stored once, so an identical chunk
  *anywhere* across all inputs costs nothing further, dedup with no dictionary-window limit, unlike
  classic solid compression.
- Surviving chunks are grouped into **packs** (~8 MiB), each compressed as a unit (stored / XZ /
  zstd), and a **footer index** maps entries to chunk lists and chunks to (pack, offset, length). The
  index sits at EOF so the writer can stream packs out in a single pass. Pack granularity is also the
  mount's seek unit, there is no separate mount format.
- **Encryption** (optional): the password is stretched with **Argon2id** over a random per-archive
  salt, and every pack and the index are sealed with **AES-256-GCM**; compress-then-encrypt, a fresh
  nonce per blob, the pack id or index role as AAD. The index's own tag doubles as the password
  verifier, so a wrong password fails cleanly on open.
- **No timestamps**: the format stores no mtimes and no absolute paths, by design. Extracting a
  `.cram` restores contents and layout, not modification times.
- **Reproducible**: for one binary at one level, an unencrypted `.cram` built from the same logical
  inputs is byte-for-byte identical, so it can be content-addressed and checksum-verified. The
  qualifier is load-bearing, the pack codec depends on build features and level, so a `zstd-c` build
  writes zstd packs where a default build writes XZ ones. Pinned within one build by
  [`tests/reproducible.rs`](../crates/cram-core/tests/reproducible.rs). **Encrypted** archives are
  *not* reproducible (fresh salt, fresh nonces), and that test asserts the difference so
  "reproducible" is never misread as "encryption is deterministic".

The format is **frozen and versioned**: v1, and v2, which adds only the per-entry transform byte. Any
layout change bumps the version byte, and a conforming reader must reject what it does not understand
rather than guess. Every build can *decode* zstd packs via the always-present pure-Rust decoder, so
archives stay readable across build configurations.

`cram-extract` is an independent implementation of this spec. It proves the document is implementable
on its own and gives users a small, auditable tool that can recover their data without the main
build, which is why it shares no code with `cram-core`.

---

## 7. Sidecars

`.cramrec` (recovery) and `.cramsig` (signature) are computed *over* an archive's bytes and stored
separately, so they add no coupling to, and never change; the frozen `.cram` format, and they work
on any file at all.

- **Recovery** splits the file into Reed-Solomon data shards and stores only the parity shards plus a
  BLAKE3 hash of every shard, so the sidecar costs about `M/N` of the file size and can reconstruct up
  to `M` damaged or missing shards. Byte layout is documented in
  [`cram-recovery/src/lib.rs`](../crates/cram-recovery/src/lib.rs).
- **Signing** is a detached ed25519 signature over a domain-separated BLAKE3 hash of the file. The
  domain separation means a `.cramsig` can never be replayed as a signature for another protocol that
  signs raw hashes.

---

## 8. The mount

[`cram-mount`](../crates/cram-mount) projects an archive into the filesystem via Windows ProjFS,
read-only: directory enumeration from the entry list, placeholder metadata from the entry, file data
from `read_range` on demand. ProjFS invokes callbacks on its own threads, so the reader is shared by
`&` (it is `Send + Sync`) and the active-enumeration map sits behind a `Mutex`. Two tiers back it, via
`formats::open_random_access`:

- **Natively seekable**, `.cram`, ZIP and ISO 9660 serve a byte range without extracting the archive
  first. They do not all pay the same price: ISO seeks straight to the extent, `.cram` decompresses
  only the packs the range touches, and ZIP re-opens the entry and decodes forward from its start,
  discarding the leading bytes through a 64 KiB scratch buffer, bounded memory, but work proportional
  to the offset.
- **Sequential, staged to RAM**; tar, 7z, RAR and bare compressed streams have no seek hand-off point, so
  [`formats/seqcache.rs`](../crates/cram-core/src/formats/seqcache.rs) decodes the whole archive into
  memory when the mount opens and serves ranges from those buffers. The cache is capped at **2 GiB
  uncompressed** (entry metadata counts against the same cap, so millions of tiny entries cannot slip
  past it); above the cap the mount is refused with an "extract it instead" error. Staging to RAM is
  also what makes those readers `Send + Sync`, including RAR, whose native handle is neither; which
  is what lets the ProjFS callbacks fan out over them.

**ProjFS is bound lazily, at run time**
([`projfs_api.rs`](../crates/cram-mount/src/projfs_api.rs)), and this is the design decision most
worth recording. ProjFS ships in the **optional** Windows feature `Client-ProjFS`, which is **off by
default**: on a stock install `ProjectedFSLib.dll` is staged in WinSxS but never projected into
`System32`. A load-time import of a DLL that is not there aborts the process at startup with
`STATUS_DLL_NOT_FOUND` (0xC0000135), before `main` runs, so a load-time binding would make the
whole binary unlaunchable on any machine without the feature, over a capability only the `mount` verb
needs. The DLL is instead `LoadLibraryW`'d on first use behind a `OnceLock`, resolving either every
entry point or none (a partial table would turn a missing export into a crash at some arbitrary later
moment). Its absence is an ordinary error from `mount` carrying the exact elevated command
(`Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS`, restart possibly required), and
every other command works without it. Type definitions still come from the `windows` crate, types
carry no linkage; only the function bindings do.

---

## 9. The Explorer menu

[`cram-shell`](../crates/cram-shell) is a COM `IContextMenu` handler. Explorer loads it in-process,
calls `Initialize` with the selection, `QueryContextMenu` to add items, and `InvokeCommand` when one
is chosen, at which point it spawns `cram.exe` and returns.

**Why COM rather than registry verbs.** Plain `HKCR\<type>\shell\<verb>` entries are far simpler and
were tried first, in this project's predecessor: they did not render. A COM handler does, and it is
the same mechanism WinRAR and 7-Zip use. The price is that this code runs inside somebody else's
process, which sets the constraints:

- **Never block.** `InvokeCommand` spawns and returns; it does not wait for an extraction.
- **Never panic across the boundary.** Every entry point returns an `HRESULT`, and unwinding into
  Explorer's C++ frames is undefined behaviour, so the panic-prone work is kept out rather than
  caught.
- **Do almost nothing in `QueryContextMenu`.** It runs on every right-click, so it reads file
  *extensions* and never opens a file to sniff it.
- **Take no dependency that pulls in a runtime DLL.** `cram_shell.dll` currently imports only OS
  libraries, unlike `cram.exe`, which needs `libwinpthread-1.dll`. That is what lets Explorer load
  it wherever it happens to be installed; a dependency that reintroduced a runtime DLL would make
  the menu silently fail to appear.

The menu is one submenu whose contents depend on the selection: extract verbs when everything
selected is an archive, create verbs otherwise. The two entries that open Cram Studio appear only
when `cram-studio.exe` is actually beside the DLL, so a CLI-only install never offers to launch
something that is not installed.

Registration is `cram shell install` ([`cram-cli/src/shell.rs`](../crates/cram-cli/src/shell.rs)),
which writes three `HKCU` keys through `reg.exe`: the class id, its `InprocServer32` pointing at the
DLL with `ThreadingModel=Apartment`, and the handler entry under `*` and `Directory`. HKCU only, so
no elevation and nothing changed for other accounts.

---

## 10. Safety model

Archives are untrusted input, so hardening is centralized rather than sprinkled through the backends.

- **Path traversal (zip-slip)**: every backend funnels entry names through `EntryPath::from_raw`
  ([`model.rs`](../crates/cram-core/src/model.rs)), the single place that strips or rejects absolute
  paths (a POSIX-style leading `/` is dropped, so `/etc/hosts` becomes `etc/hosts` under the
  destination; a drive letter is rejected outright), rejects `..`, alternate data streams (any
  component containing `:`), NUL and pathologically deep names. Windows device names (`NUL`, `CON`, …)
  survive `from_raw` **verbatim**, so the archive still lists what it contains; they are mangled in
  `join_under`, on the way to a real path, so a Unix-authored file named `NUL` is kept rather than
  silently written to the null device. The result is always relative, so `join_under` cannot leave
  the output directory.
- **Decompression bombs**: bodies stream through bounded buffers and are size-checked against the
  container's declared sizes, so a crafted huge size cannot force an unbounded allocation. `cram test`
  streams bodies through a hashing sink, so even a bombed entry is counted and discarded rather than
  buffered whole.
- **Hostile metadata**: the pure-Rust parsers (ZIP, 7z, tar, ISO, `.cram`) have a smoke-fuzz test
  ([`tests/fuzz_parsers.rs`](../crates/cram-core/tests/fuzz_parsers.rs)) that feeds random and
  mutated-from-valid bytes through `formats::open`, the entry list and a bounded body drain. It
  asserts exactly one property: that none of it **panics**, on the test thread or on a decode worker
  thread. A typed `Err` is a pass, and so is a parser that accepts the garbage and returns `Ok`; a
  no-panic gate, not a rejects-bad-input gate. Timestamp conversions are range-bounded so a crafted
  FILETIME or DOS date cannot overflow into a panic.
- **RAR runs in a sacrificial child process.** The UnRAR decoder is C++ and can fault the whole
  process on a crafted archive (which is why the fuzz test excludes it). When a `cram` verb would read
  a `.rar`, the CLI re-runs the command in a child with `CRAM_RAR_WORKER=1` set, so a fault kills only
  the child and the parent reports a clean error
  ([`cram-cli/src/main.rs`](../crates/cram-cli/src/main.rs)). A normal child exit in `0..=255` passes
  through unchanged; a Unix signal or a Windows structured exception (which arrives as an out-of-range
  `i32`) is reported as a crash and returns 70, rather than being clamped into a false success.
  Isolation applies only when an argument actually names an existing RAR file, so every other archive
  keeps the in-process path. **This isolation belongs to the CLI**, a program linking `cram-core`
  directly decodes RAR in-process. UnRAR's safe API has no per-chunk hook, so an entry is either read
  whole into RAM or handed to UnRAR's own extract-to-file call. Which one is decided per entry
  against a threshold derived from free memory (`CRAM_RAR_INMEM` overrides it): below it the
  allocation is cheaper, above it the allocation is what fails. The scratch copy goes beside the
  archive rather than in the system temp directory, which on Linux is frequently a tmpfs, where a
  multi-gigabyte scratch file would land right back in the RAM it exists to avoid.
- **Damage is contained, not hidden.** A damaged entry does not abort the job: intact entries are
  extracted, each damaged one is collected in the `Report` by name, and the process exits non-zero so
  a script can tell. On the sequential path a stream that cannot advance (truncation, a broken header)
  stops the read there and keeps everything already written. `Report::is_ok()` is false whenever any
  failure was recorded, so a partial recovery can never be mistaken for a clean run.

---

## 11. Build shape

- **Pure Rust apart from the pieces named here.**
  - **UnRAR is C++ and is not optional.** `unrar` is a plain dependency of `cram-core`, so every build
    of `cram-core` links the UnRAR C++ engine (`cram-extract` does not depend on `cram-core` and links
    none of it). RAR is read-only and there is no pure-Rust RAR decoder to swap in. Everything else on
    the default read path is pure Rust: DEFLATE via miniz_oxide, XZ/LZMA via `lzma-rust2`, zstd decode
    via `ruzstd`, bzip2 via the pure-Rust `libbz2-rs-sys` backend, plus lz4 and brotli.
  - **`zstd-c`** (off by default) adds the C libzstd encoder, which gives `.cram` packs the full zstd
    level range; `ruzstd` only encodes at its fastest setting. Any build can *decode* zstd packs, so
    enabling it does not fork the format.
  - **`download`** (off by default) pulls in `rdm-core` and its async/HTTP dependencies for `cram dl`
    and extract-while-download.
  - **`phash`** (off by default in the engine, **on** in the shipped CLI) adds perceptual image
    hashing. It gates `cram dedup --similar`, which on a default build prints "rebuild with
    --features phash" and does nothing. §4's perceptual "similar" groups depend on it.
  - `cram --version` prints which of these the binary was built with.
- **ProjFS binding is clean-room and lazy.** The mount uses the MIT/Apache `windows` crate's ProjFS
  *type* definitions rather than the GPL `windows-projfs` crate, and lives in its own crate. The
  *function* bindings are ours and resolve at first use (§8).
- **Three targets, Windows-first.** `x86_64-pc-windows-gnu` (WinLibs mingw), `x86_64-unknown-linux-gnu`
  and `aarch64-apple-darwin` all build, test and publish. UnRAR needs the link tweaks in
  [`.cargo/config.toml`](../.cargo/config.toml) on Windows and a system g++ on Linux. **Only the mount
  is Windows-only** (it is ProjFS); non-Windows targets get a stub `mount` that errors. `hw.rs` is a
  three-platform module with a per-OS detection backend: Win32, sysfs on Linux, sysctl on macOS.
  macOS binaries are unsigned, so a download is Gatekeeper-quarantined.
- **Binaries are not code-signed.** Windows SmartScreen will warn on first run.
