# Security policy

Cram parses files that come from other people. This document says where to report a problem and
which protections exist.

It covers the code in this repository: the `cram-core` engine, the `cram` CLI, the `cram-sign`,
`cram-recovery` and `cram-mount` sidecars, the standalone `cram-extract` decoder, the `cram-shell`
Explorer handler, and the `rdm-core` download engine. The **In scope** table below is the specific
list. Windows, Linux and macOS all count; the handler and the mount are Windows-only because the
features are.

---

## Supported versions

| version | supported |
|---|---|
| 1.1.x | yes |
| 1.0.x | no — upgrade to 1.1.x |
| < 1.0 | there is nothing earlier |

**1.0.0 was the first release** (6 August 2026) and 1.1.0 is current (12 August 2026). Only the
newest minor line is supported: there is one maintainer and no backport branch, so a fix ships in
the next release rather than being carried backwards. Between releases it is on `main` and available
by building from source, as soon as it is ready and pushed.

`cram --version` reports the version and which optional features the binary carries. That matters
in a report: a `zstd-c` build writes different `.cram` bytes than the pure-Rust default, so the two
are not interchangeable when reproducing something.

---

## Reporting a vulnerability

**Use GitHub's private security advisory form:**
<https://github.com/lukr54/cram/security/advisories/new>
(repository → **Security** → **Report a vulnerability**).

That is the channel to use. It keeps the report private until a fix exists and lets us add you to the
discussion.

No GitHub account? Email <cram-support@nexalit.fr> instead. Say in the subject that it is a security
report and we will move it to a private advisory.

**Please do not open a public issue** for anything that lets a crafted file escape the output
directory, corrupt memory, run code, or forge a signature. Ordinary crashes and wrong-output bugs with
no security consequence are fine as normal public issues.

A useful report contains:

- the file that triggers it (or a short script that builds it),
- the exact command, e.g. `cram x evil.zip -o out`,
- **which binary**: `cram` or `cram-extract`,
- the version (`cram --version`), and your OS and its version.

`cram diag report` writes all of the above except the file itself, plus the machine profile that
decides Cram's thread and pack sizing, into one text file you can attach. It is written locally and
sent nowhere. File and folder names are described by shape rather than included, so a report is safe
to attach to a public issue; pass `--full-paths` only if we ask.

### What to expect

Cram is maintained by Nexal IT (Ulysses Horkan EI). There is no 24/7 security team. Our targets:

- **acknowledgement within 7 days**,
- **an assessment within 30 days**, in scope or not, our severity read, and whether a fix is planned,
- **credit in the release notes**, unless you would rather stay anonymous.

We ask for **coordinated disclosure**: please give us 90 days before publishing, or less if a fix
ships sooner. There is **no bug bounty**, just credit and genuine thanks.

---

## In scope

Anything below, reachable by feeding Cram a file you control:

| Area | Where it lives | What counts |
|---|---|---|
| **Format parsers** | [`crates/cram-core/src/formats/`](crates/cram-core/src/formats/) | ZIP, 7z, tar (+gzip/xz/zstd/bz2/lz4/brotli), ISO 9660, RAR, `.cram`, a crafted archive that panics, corrupts memory, hangs unboundedly, or executes code |
| **LZMA2 segment walker** | [`formats/lzma2seg.rs`](crates/cram-core/src/formats/lzma2seg.rs) | it reads attacker-controlled chunk framing to decide where a decoder may start. A wrong cut hands a worker a byte range that decodes to plausible garbage, so it refuses anything it does not recognise rather than guessing; a hostile length is bounded by a chunk ceiling and by the pack stream's own extent, and the walk seeks over payload rather than reading it, so it allocates nothing an archive can influence |
| **Multi-stream scanner** | [`codec/multi.rs`](crates/cram-core/src/codec/multi.rs) | it scans attacker-controlled `.tar.bz2`/`.tar.xz` bytes for the stream-boundary magic that decides where the parallel decoder may split, reachable from `cram l`; the scan is bounded (`SCAN_WIN` windows, giving up after 64 MiB with no second stream found) rather than reading the whole file, and `CRAM_PARALLEL_DECODE=0` disables the path entirely |
| **Path-traversal guard** | [`model.rs`](crates/cram-core/src/model.rs) (`EntryPath::from_raw`), and the independent `sanitize` in [`cram-extract/src/main.rs`](crates/cram-extract/src/main.rs) | any entry name that causes a write **outside** the chosen output directory, or onto a Windows device |
| **Extraction path** | [`engine/`](crates/cram-core/src/engine/) | a partial or wrong extraction reported as a **success**; overwriting files the user did not select |
| **Crypto** | [`formats/cram.rs`](crates/cram-core/src/formats/cram.rs), [`cram-sign`](crates/cram-sign/src/lib.rs) | `.cram` Argon2id + AES-256-GCM, ZIP/7z AES-256, and `cram verify` accepting a `.cramsig` that a given key never produced |
| **Recovery sidecar reader** | [`crates/cram-recovery`](crates/cram-recovery/src/lib.rs) | a hostile `.cramrec` that crashes or forces an absurd allocation |
| **Standalone decoder** | [`crates/cram-extract`](crates/cram-extract/src/main.rs) | it is shipped to people who may have no other tool to hand; it gets the same scrutiny as the engine |
| **Explorer handler** | [`crates/cram-shell`](crates/cram-shell/src/lib.rs) | it runs **inside explorer.exe**: anything that lets a crafted file name reach a command line unquoted, crash Explorer, or make a menu verb act on a path the user did not select |
| **Self-update** | [`cram-cli/src/update.rs`](crates/cram-cli/src/update.rs) | it replaces the running binary: anything that lets an unverified, wrong-version or attacker-chosen payload be installed |
| **Install script** | [`install.sh`](install.sh) | it is a piped shell install, running before the user has any binary to inspect: anything that makes it fetch or execute a payload it did not verify against `SHA256SUMS` |
| **Download engine** | [`crates/rdm-core`](crates/rdm-core/src/) | it parses Metalink XML and HTTP `Link` headers straight off the network, ahead of any user decision: a redirect, header or manifest that makes `cram dl` write outside the chosen directory, fetch from a host the user never named, or exhaust memory |

## Out of scope

- **Resource exhaustion from a huge archive.** A legitimate 500 GB archive and a
  malicious one look identical, and extracting either will use your CPU, RAM and disk. "I made a
  100 GB bomb and it filled my disk" is expected behaviour. A crafted archive that **bypasses a
  bound**, unbounded allocation, a decode that never terminates, RAM use unrelated to what it
  writes, *is* in scope.
- **Attacks that already require code execution as you**, or write access to Cram's install
  directory, its state directory, or your signing keys.
- **Missing hardening with no demonstrated impact.** Very welcome as a normal issue or PR, just not
  as an advisory.
- **Cram Studio**, the Windows GUI, and the **Firefox hand-off add-on**
  (`cram-handoff@nexalit.fr`). Both are published on this repository's Releases page, but their
  source is in a private repository that cannot take a report. Send findings to
  <cram-support@nexalit.fr>, and say in the subject that it is a security report. The add-on holds
  `cookies` and `<all_urls>`, so a way to make it read cookies for a site the user is not downloading
  from, or to reach the native-messaging host from a page, is worth reporting even though it is out
  of scope for an advisory here.
- **The ProjFS requirement.** `cram mount` needs the optional Windows feature `Client-ProjFS`, which
  is off by default and takes an elevated `Enable-WindowsOptionalFeature` to turn on. That is
  Windows' design, not a Cram flaw. Every other command works without it.

---

## Protections that exist

Readable in the source, not aspirational.

**Centralized path-traversal guard.** Every backend funnels entry names through one function,
`EntryPath::from_raw` ([`model.rs`](crates/cram-core/src/model.rs)). Traversal components, names
carrying a drive letter or an NTFS alternate data stream, embedded NUL, and pathologically deep names
are rejected. A leading separator is *stripped* rather than rejected, so a rooted name like
`/etc/passwd` becomes relative and still lands under the chosen output directory. Windows reserved
device names are *mangled* rather than opened, because creating `out\NUL` would send the bytes to the
null device while the extractor reported success. `cram-extract` shares no code with the engine by
design, so it carries its own equivalent guard with its own tests.

**A short decode is a failure, not a success.** A crafted archive can declare a large entry and then
supply a handful of bytes that decode cleanly to EOF. The extractor compares what actually decoded
against what the header declared, deletes the partial file, and reports the entry as an error.

**Decompression-bomb bounds.** Sizes, key-derivation cost parameters and buffered allocations read
from an untrusted archive are checked against fixed bounds *before* the work starts, so a small
crafted file cannot force an unbounded allocation or an unbounded decode.

**A failed decode is never read again.** Solid formats hand one stream to several entries in turn, so
a reader that has already reported corrupt input gets asked for more bytes: once by the code that
turns a failed entry into a reported failure and carries on, and again by the drain that advances to
the next entry. An LZMA2 stream does not return from that second read. Every read of a 7z block or
segment therefore goes through a guard that stops touching the source after its first failure, and
the unit is failed rather than continued, since nothing after a fault in a solid stream is
recoverable anyway. Found by the fuzz harness
([`crates/cram-core/tests/fuzz_parsers.rs`](crates/cram-core/tests/fuzz_parsers.rs)) on a 2,208-byte
archive that made `cram t` spin forever;
[`crates/cram-core/tests/data/hostile-7z-read-after-error.7z`](crates/cram-core/tests/data/hostile-7z-read-after-error.7z)
is that archive, and the regression test asserts termination rather than any particular error.

**A damaged archive fails honestly.** Extraction is best-effort: intact entries are recovered and
damaged ones are listed by name, and the process still exits **non-zero**, so a script chaining on
`&&` cannot mistake a partial extraction for a clean one.

**RAR is isolated in the CLI.** RAR is decoded by the UnRAR C++ engine, the one non-Rust component in
a default build, and a crafted RAR can fault the process rather than raise a catchable Rust error.
When a `cram` command names a RAR file, the CLI re-runs itself in a child process, so a fault kills
only that child and the parent reports an error. Everything else in a default build is Rust. Two
optional features add more C: `zstd-c` links C libzstd, and `mimalloc` replaces the global allocator
that every allocation in the process goes through. Both are off by default but **on in the shipped
binary**, and `cram --version` prints which optional features a binary was built with; worth including
in a report.

**`cram update` refuses what it cannot verify.** The update path fetches the checksum the release
publishes *before* it downloads anything, and a checksum that is missing, unreadable or unmatched is
a refusal rather than a warning. The download URL is built locally from a character-checked tag and
this build's own target triple, never taken from the API response, so whoever answers the request
does not get to choose what is installed. The replacement is a move-aside followed by a rename, so a
failure leaves the previous binary in place rather than a half-written one.

**The Explorer handler stays small on purpose.** `cram-shell` runs inside `explorer.exe`, so it never
blocks, never lets a panic cross the FFI boundary, and does no file I/O while building the menu, only
extension matching. Selected paths are quoted before they reach a command line, including the
embedded-quote case. It registers under `HKCU` alone, so installing or removing the menu needs no
elevation and changes nothing for other accounts.

These are the protections that are built. They are not a claim that Cram handles hostile input better
than any other tool, treat an archive from a stranger as hostile whatever you open it with.

---

## Integrity limits worth knowing

`cram test` cannot detect every bit flip. A **silent bit flip inside a file's content** is guaranteed
to be caught only for ZIP, for 7z entries that carry a stored CRC, and for compressed or encrypted
`.cram`. (ZIP stores a CRC-32 for every entry except an AES entry written in AE-2 form, which stores
none because the AES authentication already proves the content is intact; such an entry is verified by
that authentication instead.)

Everywhere else Cram has no per-entry content checksum to compare against: an **unencrypted, stored
`.cram`**, **`tar` / `.tar.zst`**, **ISO 9660**, **RAR**, and a **7z entry that carries no stored
CRC**. A pass there means every entry decoded cleanly and its decoded length matched the declared
size, plus whatever the underlying decoder rejects; it does not prove the bytes are the original ones.
Truncation and structural damage *are* caught.

For guaranteed content integrity on those, pair the archive with `cram sign` or `cram rec` (both cover
the whole file), or use a format that carries per-entry integrity. The README's **Limitations** section
states this per format.

**Released binaries are not Authenticode-signed or notarised.** There is no code-signing
certificate, so Windows cannot vouch for a downloaded `cram.exe`, `cram-extract.exe` or the Cram
Studio installer, and SmartScreen warns on first run; macOS keeps a downloaded binary quarantined
until the flag is cleared. What you *can* verify is the download itself: every release publishes a
`SHA256SUMS` per platform, `install.sh` checks it before installing anything, and `cram update`
refuses to install an artifact whose checksum does not match. That authenticates the bytes against
the release, which is not the same as an OS-level trust decision about the publisher.

`cram sign` signs *archives*; it has nothing to do with Windows or macOS executable trust.
