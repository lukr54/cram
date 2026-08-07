# cram-mount

Mounts an archive as a folder you can browse and open files from, without extracting it first.
Windows only: it is built on ProjFS, the projected file system that also backs VFS for Git.

**Published so [`cram-cli`](https://crates.io/crates/cram-cli) can be, not as a general-purpose
library.** The API is an implementation detail of the CLI. Pin an exact version if you use it.

The mount touches the engine only at its `RandomAccessReader` boundary, so a FUSE backend for Linux
and an FSKit one for macOS would slot in behind the same interface rather than needing a second
implementation of the archive side. Neither is written yet.

ZIP, ISO and `.cram` are projected on demand. 7z, tar, RAR and bare compressed streams have no
random-access point, so mounting one decodes the whole archive into memory up front and refuses
anything over 2 GiB — extract those instead.

Needs the optional Windows feature `Client-ProjFS`, which is off by default. Every other Cram command
works without it.

<https://github.com/lukr54/cram>

Licensed under MIT OR Apache-2.0.
