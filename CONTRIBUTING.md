# Contributing to Cram

Thanks for looking. Cram is a small, opinionated codebase: a thin core, dumb format backends, one
smart engine. Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) first, it is the map, and the
module-level `//!` comments in each file are the streets.

**Found a security problem? Do not open a public issue**, see [`SECURITY.md`](SECURITY.md).

---

## Building

Cram targets **`x86_64-pc-windows-gnu`** (WinLibs mingw / GCC). The [README](README.md) carries the
authoritative toolchain setup; the short form:

```sh
cargo build --release
```

At the workspace root that builds every member with default features: the engine, the CLI, the
sidecar and mount libraries, the standalone decoder, the Explorer shell handler, and the vendored
`rdm-core` download engine. For just the two binaries you actually run:

```sh
cargo build --release -p cram-cli -p cram-extract
```

That produces `target/release/cram.exe` and `target/release/cram-extract.exe`. Build both:
`cram-extract` is a separate package that `cram-cli` does not depend on, and `cram make-sfx` shells
out to it, so without it beside `cram.exe` `make-sfx` fails with *"cram-extract stub not found next
to this binary"*.

**About "no C dependencies":** the default feature set is pure Rust **apart from UnRAR**, which is
C++ (read-only RAR) and is always compiled in. It needs the linker flags already configured in
[`.cargo/config.toml`](.cargo/config.toml), and `libwinpthread-1.dll` beside the binary on deploy
(it also lives on the mingw PATH). No MSVC toolchain is used or required. `cram-extract` has no C or
C++ code at all, so it needs no such DLL.

Optional features are opt-in so the base build always compiles:

| Feature | Effect |
|---|---|
| `zstd-c` | full-range zstd encoder (C libzstd). **The shipped binary is built with this**, it is not a pure-Rust build. |
| `download` | `cram dl` segmented downloader, and `cram update`. Opens no listening socket. |
| `phash` | perceptual image hashing, so `cram dedup --similar` can flag visually-alike photos. Pure Rust, but a large dependency tree. |

The release CLI is built as:

```sh
cargo build --release -p cram-cli --features download,zstd-c,phash --bin cram
```

The Explorer menu is a separate cdylib and has to be built too, or `cram shell install` has nothing
to register:

```sh
cargo build --release -p cram-shell
```

That produces `target/release/cram_shell.dll`, which belongs beside `cram.exe`. Keep its dependency
list to the `windows` crate: it is loaded into `explorer.exe`, and it currently imports only OS
libraries. A dependency that pulled in a runtime DLL the way `cram.exe` pulls in
`libwinpthread-1.dll` would make the menu silently stop appearing.

`cram --version` prints which of these are compiled in, worth checking before you report a bug,
since a `zstd-c` build writes different `.cram` bytes than the pure-Rust default.

### Mounting

`cram mount` additionally needs the optional Windows feature `Client-ProjFS`, which is off by
default:

```powershell
# elevated PowerShell; a restart may be required
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
```

Nothing else in Cram needs it, the DLL is bound lazily at run time, so the binaries start fine
without the feature and every other command works.

---

## Running the tests

```sh
cargo test                      # the whole workspace
cargo test -p cram-core         # one crate
cargo test -- --ignored         # runs ONLY the ignored (heavy) tests
```

On default features that is **174 passing tests, 0 failures**, and **185** with the features the
release is built with (`download,zstd-c,phash`), which compile code the default build leaves out.

One test is marked `#[ignore]` on default features: it pushes more than 16 MiB through the pure-Rust
XZ compressor and is skipped for time, not because it fails. A `download` build has a second, named
`sleeper`, which is not a test at all; it is the child process that
`a_running_binary_can_still_be_replaced` starts so it has a genuinely running executable to replace.

[`crates/cram-core/tests/fuzz_parsers.rs`](crates/cram-core/tests/fuzz_parsers.rs) runs as part of
that suite: a bounded smoke-fuzz of every pure-Rust parser (150 iterations each by default). Raise it
when you touch a parser:

```powershell
$env:CRAM_FUZZ_ITERS = 20000; cargo test -p cram-core --test fuzz_parsers
```

A parser change without a test that feeds it the malformed input you fixed is not finished.

If you change a performance claim in the docs, the number must come from a run anyone can repeat; 
**never a number you did not watch get produced.**

---

## Code style

- `cargo fmt --all`, default rustfmt, no custom config. Run it before you push.
- `cargo clippy --workspace --all-targets -- -D warnings`, this is what CI runs, and it must come
  back clean. If a lint is wrong, `#[allow]` it *with a comment saying why*.
- **Every file gets a module doc comment** (`//! …`) explaining what that piece is and why it exists.
  This codebase leans hard on them; a new file without one will be asked for one.
- Comments explain **why**, not what. If a line looks strange, the reason it is that way is the
  valuable part.
- Errors are typed (`ArchiveError`) and user-facing messages are plain sentences. No `unwrap()` on
  anything derived from archive bytes.

---

## Things that will not be merged

- **RAR creation.** Not ever, and not a matter of effort or taste: the UnRAR licence forbids using
  its source to build a RAR compressor. Cram reads RAR and will only ever read RAR.
- **A new C or C++ dependency in the default feature set.** The base build must stay compilable on a
  bare mingw toolchain. Put it behind an opt-in feature like `zstd-c`.
- **"De-duplicating" `cram-extract` against `cram-core`.** They share no code *on purpose*:
  `cram-extract` is an independent implementation of the `.cram` spec, which is what proves the spec
  is implementable and gives users a tiny auditable recovery tool.
- **Removing a documented bound** (see [`SECURITY.md`](SECURITY.md)) without an equivalent
  replacement, or bypassing `EntryPath::from_raw` in a new backend. Entry names go through the one
  guard, always.
- **A change to a frozen `.cram` layout.** v1 and v2 are both defined and both frozen. Any layout
  change bumps the version byte and updates [`docs/CRAM_FORMAT.md`](docs/CRAM_FORMAT.md) normatively,
  it does not quietly redefine a version that archives already in the wild claim to be.
- **Marketing voice in the docs.** Understated and verifiable beats impressive and unbacked.

---

## Licence of contributions

Cram is dual-licensed **MIT OR Apache-2.0**. Unless you state otherwise, any contribution you submit
for inclusion is dual-licensed the same way, with no additional terms.

That is the whole legal step: **no CLA, no copyright assignment, and no DCO sign-off required.**
Opening the pull request is your agreement.
