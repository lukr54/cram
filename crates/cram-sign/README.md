# cram-sign

Detached ed25519 signatures for any file, not only `.cram`.

A checksum proves a file is intact. A signature proves who produced it. `cram test` will tell you an
archive decompresses; it cannot tell you the archive is the one its author made. This covers the
second question.

**Published so [`cram-cli`](https://crates.io/crates/cram-cli) can be, not as a general-purpose
library.** The API is an implementation detail of the CLI. Pin an exact version if you use it. For
general signing work, prefer [`ed25519-dalek`](https://crates.io/crates/ed25519-dalek) directly —
this crate is a thin file-oriented layer over it with a sidecar format attached.

Signature verification is in scope for the project's security policy, specifically `cram verify`
accepting a `.cramsig` that a given key never produced:
<https://github.com/lukr54/cram/blob/main/SECURITY.md>

This signs *archives*. It has nothing to do with Windows Authenticode or macOS notarisation.

Licensed under MIT OR Apache-2.0.
