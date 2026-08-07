# cram-core

The engine behind the [`cram`](https://crates.io/crates/cram-cli) archiver: format detection, the
codec layer, the parallel extract and create paths, and the `.cram` format itself.

**This is published so `cram-cli` can be, not as a general-purpose library.** The API is an
implementation detail of the CLI and changes when the CLI needs it to. It is versioned honestly —
a breaking change is a major version bump — but nothing here is designed around outside users, and
there is no deprecation policy. If you build on it, pin an exact version.

If you want to *use* Cram, install the command line:

```sh
cargo install cram-cli
```

If you want to *read* a `.cram` file with a minimal dependency footprint, use
[`cram-extract`](https://crates.io/crates/cram-extract) instead. It shares no code with this crate
by design, so a bug in here cannot take the recovery path down with it.

<https://github.com/lukr54/cram>

Licensed under MIT OR Apache-2.0.
