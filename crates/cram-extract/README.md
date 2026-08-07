# cram-extract

A standalone decoder for `.cram` archives, and the stub used for self-extracting archives.

```sh
cargo install cram-extract
```

**It shares no code with the Cram engine, on purpose.** Its whole dependency list is five decode-only
pure-Rust crates. The point is that the thing which reads your archives back is not the thing that
wrote them: if the main engine ever grew a bug that ate archives, the recovery path is independent of
it. It is also small enough for someone else to keep alive if this project stopped.

It only reads. To create archives, or to handle ZIP, 7z, tar, ISO and RAR, use
[`cram-cli`](https://crates.io/crates/cram-cli).

The `.cram` format is specified at
<https://github.com/lukr54/cram/blob/main/docs/CRAM_FORMAT.md>, so a clean-room reader can be written
from the specification rather than from this source.

Licensed under MIT OR Apache-2.0.
