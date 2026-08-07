# rdm-core

A segmented, resumable, multi-source download engine. Library only, no interface.

Splits a download across several connections, resumes from a sidecar after an interruption, fails
over past a broken mirror, and can ramp the connection count while watching throughput. Reads
Metalink (`.meta4`/`.metalink`) and RFC 6249 `Link` headers to discover mirrors and checksums.

**Published so [`cram-cli`](https://crates.io/crates/cram-cli) can be, not as a general-purpose
library.** It backs the `cram dl` verb and Cram's browser hand-off. The API is an implementation
detail of those; pin an exact version if you use it.

It parses Metalink XML and HTTP headers straight off the network, ahead of any user decision, so it
is in scope for the project's security policy — a redirect, header or manifest that writes outside
the chosen directory, fetches from a host nobody named, or exhausts memory:
<https://github.com/lukr54/cram/blob/main/SECURITY.md>

<https://github.com/lukr54/cram>

Licensed under MIT OR Apache-2.0.
