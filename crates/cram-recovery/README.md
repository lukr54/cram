# cram-recovery

Reed–Solomon recovery sidecars for any file, not only `.cram`. Write a `.cramrec` alongside something
you care about, and a later corruption or truncation can be repaired from the parity rather than
needing the original back.

Each shard carries a BLAKE3 hash, so repair knows *which* shards are damaged rather than guessing.

**Published so [`cram-cli`](https://crates.io/crates/cram-cli) can be, not as a general-purpose
library.** The API is an implementation detail of the CLI. Pin an exact version if you use it.

The parsing side reads untrusted input — a `.cramrec` you did not create is as hostile as any other
file — so it is in scope for the project's security policy:
<https://github.com/lukr54/cram/blob/main/SECURITY.md>

Licensed under MIT OR Apache-2.0.
