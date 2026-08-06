# Roadmap

**Nothing on this page exists yet.** Everything Cram does today is in
[`README.md`](README.md) and [`CHANGELOG.md`](CHANGELOG.md); this is what is intended, in roughly
the order it is intended. There are no dates, because a one-person project that publishes dates
publishes fiction.

Items move off this page by shipping or by being dropped, and a dropped one goes to
[Not planned](#not-planned) with the reason rather than quietly disappearing.

## Next

**Code signing.** Nothing is signed today, so Windows SmartScreen warns on first run of `cram.exe`
and of the Studio installer, and macOS keeps a downloaded binary quarantined until the flag is
cleared. It is the first thing anybody downloading Cram encounters, and it is a certificate and a
build-pipeline change rather than a code problem.

**Decode each pack once.** `.cram` extraction currently decompresses 1.48 packs for every pack it
needs, measured on the 42,151-file benchmark corpus against a floor of 1.0. Two workers that miss
the cache on the same pack both decode it and one result is thrown away, which is roughly 1 GiB of
wasted decompression on a 3.3 GB archive. The fix is single-flight: the second miss waits on the
first decode instead of repeating it. It costs CPU, not correctness.

**Windows benchmark numbers.** Every figure in [`BENCHMARKS.md`](BENCHMARKS.md) was measured on
Linux, and Windows is the platform Cram is built for first. The file-open path is known to differ
sharply between the two (see [`docs/PERFORMANCE_FINDINGS.md`](docs/PERFORMANCE_FINDINGS.md) §7), so
the Linux numbers should not be assumed to carry over.

## Considered

**Mounting on Linux and macOS.** `cram mount` is built on ProjFS and is therefore Windows-only.
FUSE would cover Linux and FSKit macOS, behind the same `RandomAccessReader` boundary the ProjFS
implementation already uses. This is a large piece of work and is not started.

**Cram Studio beyond Windows.** The installer targets NSIS only. The engine already builds and
tests on Linux and macOS; the GUI shell does not ship there.

**Symlinks in `.cram`.** They are skipped on create and every one is reported, because the `.cram`
index has no field for a link target. Storing them is a **format change**, not a code change, so it
waits for a version of the format that can carry them. Dereferencing instead is not on the table:
on one kernel tree that silently duplicated 8,011 files behind twelve directory symlinks, and it
turns a link cycle into an unbounded walk.

**Lazy mounts for 7z, tar and RAR.** Mounting one of these decodes the whole archive into memory
and refuses anything over 2 GiB, because none of them offers a random-access boundary the way ZIP,
ISO and `.cram` do. ZIP, ISO and `.cram` are already projected on demand.

**Streaming `cram conv`.** Conversion holds one whole entry in memory, so a `.cram` containing a
single file over 512 MiB fails to convert even though `cram x` extracts it fine. Extraction streams
each entry to disk; conversion should too.

**Corpora larger than memory.** Every published measurement fits in the benchmark machine's page
cache, so none of them is bounded by re-reading source data from disk. A 200 GB backup is a
different measurement and has not been made.

## Not planned

**Creating RAR archives.** The UnRAR licence forbids using its source to build a compressor. This
is permanent and is not a matter of effort. Cram reads RAR and always will.

**Telemetry, in any form.** Cram contacts no server while archiving, extracting or browsing. The
diagnostics feature writes a text file to your own disk and never sends it; `cram update` talks to
GitHub's releases endpoint only when you run it. There is no plan for a version of this that
reports anything, and the absence is a feature rather than an oversight.

**An account, a licence check, or a network call in the CLI.** The command line is MIT OR
Apache-2.0 and works offline forever. Cram Studio has a paid tier; the command line does not and
will not.

## Asking for something

Open an issue. A use case that is not on this page is more useful than a vote on something that is,
and "this is slower than X on my data" is the most useful of all if it comes with a
`cram <command> --diag-report` file attached.
