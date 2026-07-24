# Security policy

Cram parses files that come from other people, downloads, mail attachments, a colleague's USB stick.
This document says where to report a problem, what counts as one, and which protections exist.

It covers the code in this repository: the `cram-core` engine, the `cram` CLI, the `cram-sign`,
`cram-recovery` and `cram-mount` sidecars, and the standalone `cram-extract` decoder. The **In scope**
table below is the specific list.

---

## Supported versions

`1.0.x` is supported. It is the first public release, so there is nothing earlier and nothing to
backport to. Fixes land on `main` and ship in the next release.

---

## Reporting a vulnerability

**Use GitHub's private security advisory form:**
<https://github.com/lukr54/cram/security/advisories/new>
(repository → **Security** → **Report a vulnerability**).

That is the channel to use. It keeps the report private until a fix exists and lets us add you to the
discussion.

**Please do not open a public issue** for anything that lets a crafted file escape the output
directory, corrupt memory, run code, or forge a signature. Ordinary crashes and wrong-output bugs with
no security consequence are fine as normal public issues.

A useful report contains:

- the file that triggers it (or a short script that builds it),
- the exact command, e.g. `cram x evil.zip -o out`,
- **which binary**: `cram.exe` or `cram-extract.exe`,
- the version (`cram --version`) and your Windows build.

### What to expect

Cram is maintained by Nexalit, a small company. There is no 24/7 security team. Our targets:

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
| **Path-traversal guard** | [`model.rs`](crates/cram-core/src/model.rs) (`EntryPath::from_raw`), and the independent `sanitize` in [`cram-extract/src/main.rs`](crates/cram-extract/src/main.rs) | any entry name that causes a write **outside** the chosen output directory, or onto a Windows device |
| **Extraction path** | [`engine/`](crates/cram-core/src/engine/) | a partial or wrong extraction reported as a **success**; overwriting files the user did not select |
| **Crypto** | [`formats/cram.rs`](crates/cram-core/src/formats/cram.rs), [`cram-sign`](crates/cram-sign/src/lib.rs) | `.cram` Argon2id + AES-256-GCM, ZIP/7z AES-256, and `cram verify` accepting a `.cramsig` that a given key never produced |
| **Recovery sidecar reader** | [`crates/cram-recovery`](crates/cram-recovery/src/lib.rs) | a hostile `.cramrec` that crashes or forces an absurd allocation |
| **Standalone decoder** | [`crates/cram-extract`](crates/cram-extract/src/main.rs) | it is shipped to people who may have no other tool to hand; it gets the same scrutiny as the engine |

## Out of scope

- **Resource exhaustion from a deliberately huge archive.** A legitimate 500 GB archive and a
  malicious one look identical, and extracting either will use your CPU, RAM and disk. "I made a
  100 GB bomb and it filled my disk" is expected behaviour. A crafted archive that **bypasses a
  bound**, unbounded allocation, a decode that never terminates, RAM use unrelated to what it
  writes, *is* in scope.
- **Attacks that already require code execution as you**, or write access to Cram's install
  directory, its state directory, or your signing keys.
- **Missing hardening with no demonstrated impact.** Very welcome as a normal issue or PR, just not
  as an advisory.
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

**A damaged archive fails honestly.** Extraction is best-effort: intact entries are recovered and
damaged ones are listed by name, and the process still exits **non-zero**, so a script chaining on
`&&` cannot mistake a partial extraction for a clean one.

**RAR is isolated in the CLI.** RAR is decoded by the UnRAR C++ engine, the one non-Rust component in
a default build, and a crafted RAR can fault the process rather than raise a catchable Rust error.
When a `cram` command names a RAR file, the CLI re-runs itself in a child process, so a fault kills
only that child and the parent reports an error. Everything else in a default build is Rust. Two
optional features add more C: `zstd-c` links C libzstd and `libdeflate` links C libdeflate. Neither is
on by default, and `cram --version` prints which of them a binary was built with; worth including in
a report.

These are the protections that are built. They are not a claim that Cram handles hostile input better
than any other tool, treat an archive from a stranger as hostile whatever you open it with.

---

## Integrity limits worth knowing

`cram test` cannot detect every bit flip. A **silent bit flip inside a file's content** is guaranteed
to be caught only for ZIP, for 7z entries that carry a stored CRC, and for compressed or encrypted
`.cram`. (ZIP stores a CRC-32 for every entry except an AES entry written in AE-2 form, which stores
none because the AES authentication already proves the content is intact; such an entry is verified by
that authentication instead.)

Everywhere else, an **unencrypted, stored `.cram`**, **`tar` / `.tar.zst`**, **ISO 9660**, **RAR**,
and a **7z entry that carries no stored CRC**, Cram has no per-entry content checksum to compare
against. A pass there means every entry decoded cleanly and its decoded length matched the declared
size, plus whatever the underlying decoder rejects; it does not prove the bytes are the original ones.
Truncation and structural damage *are* caught.

For guaranteed content integrity on those, pair the archive with `cram sign` or `cram rec` (both cover
the whole file), or use a format that carries per-entry integrity. The README's **Limitations** section
states this per format.

**Released binaries are not Authenticode-signed.** There is no code-signing certificate, so Windows
cannot vouch for a downloaded `cram.exe` or `cram-extract.exe` and SmartScreen warns on first run.
`cram sign` signs *archives*; it has nothing to do with Windows executable trust.
