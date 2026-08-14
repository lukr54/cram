# Roadmap

**Nothing in the [Next](#next) or [Planned](#planned) sections exists yet.** Everything Cram does
today is in [`README.md`](README.md) and [`CHANGELOG.md`](CHANGELOG.md); this page is what is
intended, in roughly the order it is intended.

There are no dates. Dates on unbuilt software are guesses presented as commitments, and the useful
version of that promise is [What holds whatever happens](#what-holds-whatever-happens) below, which
does not depend on anything shipping on time.

Items leave this page by shipping or by being dropped, and a dropped one moves to
[Not planned](#not-planned) with the reason rather than quietly disappearing.

## What holds whatever happens

This is the part that matters if you are deciding whether to trust an archive format. None of it is
a promise about the future; it is all true today and checkable now.

**The `.cram` format is frozen and specified.** [`docs/CRAM_FORMAT.md`](docs/CRAM_FORMAT.md) is the
normative specification, and frozen means the layout of a defined version never changes. Any future
change bumps the version byte and is written up as a new version rather than altering an existing
one. An archive written today is readable by every later version.

**There is a second, independent decoder.** `cram-extract` reads any `.cram` and **shares no code
with the engine by design**. Its entire dependency list is five decode-only pure-Rust crates. If the
main engine ever grew a bug that ate archives, the recovery path is not the same code that wrote
them; and if this project stopped, that decoder is small enough for someone else to keep alive.

**Both are MIT OR Apache-2.0.** The engine, the CLI and the standalone decoder can be forked,
vendored, or continued by anyone, with no permission needed from us. The format specification is in
the same repository under the same terms, so a clean-room reader can be written from it.

**Every release publishes checksums.** `SHA256SUMS` per platform, `install.sh` verifies before
installing, and `cram update` refuses an artifact whose checksum does not match.

The short version: if Cram stopped being developed tomorrow, your archives stay readable, the tools
that read them stay buildable, and the specification to write a new reader is published.

## Support

`1.0.x` is the supported line. A fix ships in the next patch release; between releases it is on
`main` and available by building from source.

Security reports go through [GitHub's private advisory
form](https://github.com/lukr54/cram/security/advisories/new), and
[`SECURITY.md`](SECURITY.md) states what to expect: acknowledgement within 7 days, an assessment
within 30, and credit in the release notes unless you would rather stay anonymous. It also lists
what is in scope and what is not, so a report is not wasted effort.

Ordinary bugs go to [issues](https://github.com/lukr54/cram/issues). A report carrying a
`cram <command> --diag-report` file is worth several without one: it records the machine profile
that decides Cram's thread and pack sizing, the archive's structure, and which entry failed, with
file and folder names described by shape rather than included.

## Why it keeps going

Cram Studio has a paid tier, and it pays for the work on the engine underneath it. That is the whole
commercial model: no telemetry, no data collection, nothing sold on the side.

The command line is MIT OR Apache-2.0 and free, with every format, every effort level, encryption,
mounting, deduplication, recovery sidecars and signing in it. That does not change. Studio is a
Windows desktop app for people who would rather not use a terminal, free to download with a paid Pro
upgrade, and if the terminal suits you there is no reason to buy anything.

## Next

**Code signing.** Nothing is signed today, so Windows SmartScreen warns on first run of `cram.exe`
and of the Studio installer, and macOS keeps a downloaded binary quarantined until the flag is
cleared. It is the first thing anybody downloading Cram encounters. This is a certificate and a
build-pipeline change rather than a code problem, which is why it is first.

**Decode each pack once.** `.cram` extraction currently decompresses 1.48 packs for every pack it
needs, measured on the 42,151-file benchmark corpus against a floor of 1.0. Two workers that miss
the cache on the same pack both decode it and one result is thrown away, which is roughly 1 GiB of
wasted decompression on a 3.3 GB archive. The fix is single-flight: the second miss waits on the
first decode instead of repeating it. It costs CPU, not correctness.

**Windows benchmark numbers.** Every figure in [`BENCHMARKS.md`](BENCHMARKS.md) was measured on
Linux, and Windows is the platform Cram is built for first. The file-open path is known to differ
sharply between the two (see [`docs/PERFORMANCE_FINDINGS.md`](docs/PERFORMANCE_FINDINGS.md) §7), so
the Linux numbers should not be assumed to carry over.

## Planned

**Mounting on Linux and macOS.** `cram mount` is built on ProjFS and is therefore Windows-only. The
mount only ever touches the `RandomAccessReader` boundary, so a FUSE backend for Linux and an FSKit
one for macOS slot in behind the same interface rather than needing a second implementation of the
archive side. This is the largest item on the page and it is honest to say it has not been started.

**Cram Studio on Linux and macOS.** The engine already builds and passes its full test suite on
both, on their own CI runners. What is Windows-only is the GUI shell and its NSIS installer. Tauri
targets both platforms, so this is packaging and platform integration work rather than a port of the
engine.

## Considered

**Symlinks in `.cram`.** They are skipped on create and every one is reported, because the index has
no field for a link target. Storing them is a format version bump, not a code change, so it waits
for a version that can carry them. Dereferencing instead is not on the table: on one kernel tree
that silently duplicated 8,011 files behind twelve directory symlinks, and it turns a link cycle
into an unbounded walk.

**Lazy mounts for 7z, tar and RAR.** Mounting one of these decodes the whole archive into memory and
refuses anything over 2 GiB. tar and RAR have no random-access boundary at all, the way ZIP, ISO and
`.cram` do; those three are already projected on demand.

7z is no longer in that category and the entry is kept here because the work is now worth doing
rather than impossible. Extraction addresses a 7z by solid block, and by LZMA2 segment within a
block where the archive carries dictionary resets, so a ranged read costs decoding from the nearest
segment start — on the benchmark corpus 128 MiB rather than the whole 2.8 GB archive. What is
missing is teaching `read_range` to start there instead of at the block, and deciding what a mount
should do with an archive whose segments are large enough that even that is slow.

The shape for it now exists: `RandomAccessReader::entry_splits` reports where one entry may be cut
into independently-decodable pieces, and `.cram` implements it against its pack boundaries. The 7z
version is the same question asked of segments, which makes this smaller than it was.

**Streaming `cram conv`.** Conversion holds one whole entry in memory, so a `.cram` containing a
single file over 512 MiB fails to convert even though `cram x` extracts it fine. Extraction streams
each entry to disk; conversion should too.

**Corpora larger than memory.** Every published measurement fits in the benchmark machine's page
cache, so none is bounded by re-reading source data from disk. A 200 GB backup is a different
measurement and has not been made.

## Not planned

**Creating RAR archives.** The UnRAR licence forbids using its source to build a compressor. This is
permanent and is not a matter of effort. Cram reads RAR and always will.

**Telemetry, in any form.** Cram contacts no server while archiving, extracting or browsing. The
diagnostics feature writes a text file to your own disk and never sends it; `cram update` talks to
GitHub's releases endpoint only when you run it. There is no plan for a version of this that reports
anything, and the absence is a feature rather than an oversight.

**An account, a licence check, or a network call in the CLI.** The command line works offline
forever. Cram Studio has a paid tier; the command line does not and will not.

## Asking for something

Open an [issue](https://github.com/lukr54/cram/issues). A use case that is not on this page is more
useful than a vote on something that is, and "this is slower than X on my data" is the most useful of
all when it comes with a `--diag-report` file attached.
