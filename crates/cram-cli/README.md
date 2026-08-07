# cram-cli

The `cram` command line: a multi-format archiver in Rust.

```sh
cargo install cram-cli   # installs a binary called `cram`
```

The crate is `cram-cli` because `cram` on crates.io has been held since 2019 by an unrelated,
abandoned crate. The binary it installs is `cram`.

Creates and extracts ZIP, 7z, tar (with gzip, xz, zstd, bzip2, lz4, brotli), ISO 9660 and its own
`.cram` format; reads RAR. Also does deduplication, encryption, Reed–Solomon recovery sidecars,
ed25519 signing, self-extracting archives, segmented downloads, and mounting an archive as a folder.

On a public 42,151-file corpus (2.80 GB, 15% duplicate), creating an archive takes 6.93 s against
65.46 s for `7z -mx=5` and 84.09 s for `rar -m3`, at a better ratio than 7-Zip and the same ratio as
RAR. The corpus and the method are published so you can check that rather than take it on trust.

Optional features, all off by default: `download` (the `cram dl` verb), `zstd-c` (C libzstd instead
of the pure-Rust encoder), `phash` (perceptual hashing for `cram dedup --similar`).

**Full documentation, benchmarks and the format specification:**
<https://github.com/lukr54/cram>

Licensed under MIT OR Apache-2.0.
