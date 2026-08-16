# cram-recovery

Reed–Solomon recovery sidecars for any file, not only `.cram`. Write a `.cramrec` alongside something
you care about, and a later corruption or truncation can be repaired from the parity rather than
needing the original back.

The bound is exact, and it is what to check before trusting a sidecar. The file is split into `N`
data shards, at most 200 of them, about 256 KiB each until the file passes ~51 MB and proportionally
larger after that. The sidecar holds `M = round(N × redundancy)` parity shards, at least one. Repair
reconstructs **at most `M` lost or damaged shards** out of the `N + M`; past that it fails and says
so. Redundancy is `cram rec create <file> -r <percent>` and defaults to 10%, so a default sidecar
costs about a tenth of the file's size and survives about a tenth of its shards going bad.

Each shard carries a BLAKE3 hash, so repair knows *which* shards are damaged rather than guessing.

**Published so [`cram-cli`](https://crates.io/crates/cram-cli) can be, not as a general-purpose
library.** The API is an implementation detail of the CLI. Pin an exact version if you use it.

The parsing side reads untrusted input — a `.cramrec` you did not create is as hostile as any other
file — so it is in scope for the project's security policy:
<https://github.com/lukr54/cram/blob/main/SECURITY.md>

Licensed under MIT OR Apache-2.0.
