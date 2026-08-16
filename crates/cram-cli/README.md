# cram-cli

The `cram` command line: a multi-format archiver in Rust.

```sh
cargo install cram-cli   # installs a binary called `cram`
```

The crate is `cram-cli` because `cram` on crates.io has been held since 2019 by an unrelated,
abandoned crate. The binary it installs is `cram`.

Creates and extracts ZIP, 7z, tar (with gzip, xz, zstd, bzip2, lz4, brotli) and its own `.cram`
format; reads RAR and ISO 9660. Also does deduplication, encryption, Reed–Solomon recovery sidecars,
ed25519 signing, self-extracting archives, segmented downloads, and mounting an archive as a folder
(Windows only, and only once the OS's own optional "Client-ProjFS" feature is turned on; checked at
runtime, nothing extra to build).

On a public 42,151-file corpus (2.80 GB, 15% duplicate), creating an archive takes 6.95 s against
68.25 s for `7z -mx=5` (9.8x faster, 13.4% smaller) and, for RAR, 53.25 s for `rar -m3` (RAR's own
default: 7.7x faster, 16.3% smaller) or 86.78 s for `rar -m5 -s` (ratio-matched to 7-Zip: 12.5x
faster, ratio 0.12% apart, which counts as a tie). The corpus and the method are published so you can
check that rather than take it on trust.

Optional features, all off by default: `download` (the `cram dl` verb), `zstd-c` (C libzstd instead
of the pure-Rust encoder), `phash` (perceptual hashing for `cram dedup --similar`), `mimalloc`
(replaces the global allocator; the shipped release binaries build with it).

**Full documentation, benchmarks and the format specification:**
<https://github.com/lukr54/cram>

Licensed under MIT OR Apache-2.0.
