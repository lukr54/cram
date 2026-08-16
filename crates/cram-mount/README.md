# cram-mount

Mounts an archive as a folder you can browse and open files from, without extracting it first.
Windows only: it is built on ProjFS, the projected file system that also backs VFS for Git.

**Published so [`cram-cli`](https://crates.io/crates/cram-cli) can be, not as a general-purpose
library.** The API is an implementation detail of the CLI. Pin an exact version if you use it.

The mount touches the engine only at its `RandomAccessReader` boundary, so a FUSE backend for Linux
and an FSKit one for macOS would slot in behind the same interface rather than needing a second
implementation of the archive side. Neither is written yet.

ZIP, ISO and `.cram` are projected on demand. 7z, tar, RAR and bare compressed streams are decoded
whole into memory up front and refuse anything over 2 GiB — extract those instead. tar, RAR and bare
streams have no random-access point at all; 7z has one that extraction uses, but serving a small
ranged read through it still means decoding from the start of a solid block or LZMA2 segment, so a
mount would not be the on-demand thing the name promises. That one is on the roadmap.

Read-only by default. `--writable` keeps whatever is written into the mount folder as a layer over
the archive: the archive stays the immutable base, the folder holds everything that has diverged from
it, and nothing is written back into the archive. Deleting the folder resets to a pristine archive.
ProjFS makes every mount writable whether or not anyone asked, because a modified placeholder becomes
a real file on disk and outlives the mount; the read-only mode throws those files away with the
folder rather than preventing them.

A mount does not survive a reboot. `--remember` records one in a plain-text list at
`%APPDATA%\cram\mounts.txt`, `cram mount --restore` brings the recorded ones back, `--list` prints
them and `--forget <dir>` drops one. Nothing is remembered unless asked for, and an encrypted archive
is not remembered at all, because its password cannot be stored.

Needs the optional Windows feature `Client-ProjFS`, which is off by default. Every other Cram command
works without it.

<https://github.com/lukr54/cram>

Licensed under MIT OR Apache-2.0.
