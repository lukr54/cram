# Third-party notices

Cram itself is licensed under **MIT OR Apache-2.0** (see `LICENSE-MIT` and `LICENSE-APACHE`). It also
links, bundles or redistributes the third-party components listed below, each under its own licence.
Several of those licences require their text to be reproduced in binary distributions, this file is
how Cram does that, and it is installed alongside the binaries.

Sections 1–4 cover components whose licence obliges us to reproduce a specific notice (UnRAR, the
Intel Slicing-by-8 acknowledgment, the winpthreads runtime DLL, and the bundled C Zstandard library).
Section 5 covers the Rust dependency graph, whose per-crate copyright and licence texts are reproduced
in full in the companion `THIRD-PARTY-LICENSES.md`.

---

## 1. UnRAR (RARLAB), RAR decoding

Cram reads RAR archives using the official UnRAR C++ engine (via the `unrar` / `unrar_sys` crates),
statically linked into `cram.exe`. It is not linked into `cram-extract.exe`, which depends on none of
the engine, nor into `cram_shell.dll`, which depends only on the Windows API.

**Cram never creates RAR archives, and never will**: clause 2 below forbids using this source to
develop a RAR-compatible archiver or to re-create the RAR compression algorithm. Cram's RAR support
is read-only (list, test, extract, convert-out) by design and by licence.

```
 ******    *****   ******   UnRAR - free utility for RAR archives
 **   **  **   **  **   **  ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
 ******   *******  ******    License for use and distribution of
 **   **  **   **  **   **   ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
 **   **  **   **  **   **         FREE portable version
                                   ~~~~~~~~~~~~~~~~~~~~~

      The source code of UnRAR utility is freeware. This means:

   1. All copyrights to RAR and the utility UnRAR are exclusively
      owned by the author - Alexander Roshal.

   2. UnRAR source code may be used in any software to handle
      RAR archives without limitations free of charge, but cannot be
      used to develop RAR (WinRAR) compatible archiver and to
      re-create RAR compression algorithm, which is proprietary.
      Distribution of modified UnRAR source code in separate form
      or as a part of other software is permitted, provided that
      full text of this paragraph, starting from "UnRAR source code"
      words, is included in license, or in documentation if license
      is not available, and in source code comments of resulting package.

   3. The UnRAR utility may be freely distributed. It is allowed
      to distribute UnRAR inside of other software packages.

   4. THE RAR ARCHIVER AND THE UnRAR UTILITY ARE DISTRIBUTED "AS IS".
      NO WARRANTY OF ANY KIND IS EXPRESSED OR IMPLIED.  YOU USE AT
      YOUR OWN RISK. THE AUTHOR WILL NOT BE LIABLE FOR DATA LOSS,
      DAMAGES, LOSS OF PROFITS OR ANY OTHER KIND OF LOSS WHILE USING
      OR MISUSING THIS SOFTWARE.

   5. Installing and using the UnRAR utility signifies acceptance of
      these terms and conditions of the license.

   6. If you don't agree with terms of the license you must remove
      UnRAR files from your storage devices and cease to use the
      utility.

      Thank you for your interest in RAR and UnRAR.


                                            Alexander L. Roshal
```

## 2. UnRAR acknowledgments (including the Intel Slicing-by-8 BSD notice)

Reproduced from the UnRAR source distribution's `acknow.txt`. The Intel Slicing-by-8 licence
explicitly requires that its notice be reproduced **in binary form**, in the documentation or other
materials provided with the distribution, which is what this section is.

```
                           ACKNOWLEDGMENTS

* We used "Screaming Fast Galois Field Arithmetic Using Intel
  SIMD Instructions" paper by James S. Plank, Kevin M. Greenan
  and Ethan L. Miller to improve Reed-Solomon coding performance.
  Also we are grateful to Artem Drobanov and Bulat Ziganshin
  for samples and ideas allowed to make Reed-Solomon coding
  more efficient.

* RAR4 text compression algorithm is based on Dmitry Shkarin PPMII
  and Dmitry Subbotin carryless rangecoder public domain source code.
  You can find it in ftp.elf.stuba.sk/pub/pc/pack.

* RAR encryption includes parts of public domain code
  from Szymon Stefanek AES and Steve Reid SHA-1 implementations.

* With exception of SFX modules, RAR uses CRC32 function based
  on Intel Slicing-by-8 algorithm. Original Intel Slicing-by-8 code
  is available here:

    https://sourceforge.net/projects/slicing-by-8/

  Original Intel Slicing-by-8 code is licensed under BSD License
  available at http://www.opensource.org/licenses/bsd-license.html

    Copyright (c) 2004-2006 Intel Corporation.
    All Rights Reserved

    Redistribution and use in source and binary forms, with or without
    modification, are permitted provided that the following conditions
    are met:

    Redistributions of source code must retain the above copyright notice,
    this list of conditions and the following disclaimer.

    Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer
    in the documentation and/or other materials provided with
    the distribution.

    THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
    "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
    LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS
    FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
    HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
    SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
    LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
    DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
    ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
    OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
    OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
    SUCH DAMAGE.

* RAR archives may optionally include BLAKE2sp hash ( https://blake2.net ),
  designed by Jean-Philippe Aumasson, Samuel Neves, Zooko Wilcox-O'Hearn
  and Christian Winnerlein.

* Useful hints provided by Alexander Khoroshev and Bulat Ziganshin allowed
  to significantly improve RAR compression and speed.
```

## 3. MinGW-w64 winpthreads, `libwinpthread-1.dll`

Cram is built with the MinGW-w64 (GNU) toolchain. The UnRAR C++ code pulls in the pthreads shim, and
that one runtime DLL is redistributed next to the executables (`libwinpthread-1.dll`); libstdc++ and
libgcc are linked statically and are not shipped.

It is `cram.exe` that needs it. `cram-extract.exe` and `cram_shell.dll` import only Windows' own
libraries and would run without it, though it ships in the same folder as all three.

```
Copyright (c) 2011 mingw-w64 project

Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the "Software"),
to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense,
and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.


Parts of this library are derived by:

Posix Threads library for Microsoft Windows

Use at own risk, there is no implied warranty to this code.
It uses undocumented features of Microsoft Windows that can change
at any time in the future.

(C) 2010 Lockless Inc.
All rights reserved.

Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice,
this list of conditions and the following disclaimer.
* Redistributions in binary form must reproduce the above copyright notice,
this list of conditions and the following disclaimer in the documentation
and/or other materials provided with the distribution.
* Neither the name of Lockless Inc. nor the names of its contributors may be
used to endorse or promote products derived from this software without
specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT,
INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE
OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED
OF THE POSSIBILITY OF SUCH DAMAGE.
```

## 4. Zstandard, the bundled C library

The release build enables the `zstd-c` feature, which compiles Meta's C Zstandard library into
`cram.exe` / `cram-extract.exe` (through the `zstd` / `zstd-sys` crates) as a fast `.cram` pack codec.
Its BSD-3-Clause licence requires the copyright notice, the conditions and the disclaimer to be
reproduced in binary distributions:

```
BSD License

For Zstandard software

Copyright (c) Meta Platforms, Inc. and affiliates. All rights reserved.

Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

 * Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

 * Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

 * Neither the name Facebook, nor Meta, nor the names of its contributors may
   be used to endorse or promote products derived from this software without
   specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

The pure-Rust default build does not link this library; it is compiled in only under the shipped
`zstd-c` feature. (The `zstd` / `zstd-safe` / `zstd-sys` Rust crates are themselves MIT-licensed and
appear, with their own notices, in `THIRD-PARTY-LICENSES.md`.)

## 5. Rust dependencies

Cram statically links a large graph of third-party Rust crates. **There is no GPL, AGPL, LGPL or MPL
anywhere in the graph**, every dependency is permissive. The licences the resolved graph actually uses,
with the crate count behind each, are MIT (247), Unicode-3.0 (19), Apache-2.0 (11), BSD-3-Clause (8),
BSD-2-Clause (3), ISC (3), CC0-1.0 (1), CDLA-Permissive-2.0 (1) and bzip2-1.0.6 (1). `about.toml`
additionally permits 0BSD, Unlicense, Zlib and Unicode-DFS-2016, all of which are permissive and any of
which a future dependency may resolve to; nothing outside that list can enter without failing
generation, which is what keeps the sentence above true rather than merely asserted.

The single CDLA-Permissive-2.0 crate is `webpki-roots`, Mozilla's CA certificate bundle. The Community
Data License Agreement is written for data rather than code, which is what that crate is. It is
permissive, carries no copyleft, and the obligation is to preserve the disclaimers.

The **full copyright notice and licence text for every one of these crates** is reproduced in the
companion [`THIRD-PARTY-LICENSES.md`](THIRD-PARTY-LICENSES.md), generated from each crate's own
`LICENSE` file and distributed alongside the binaries. That appendix, not a summary, is what discharges
the reproduction requirement the MIT, BSD-2/3-Clause, ISC, Unicode-3.0, CDLA and bzip2 licences place on
binary distributions. `ring` is the one to note: it descends from BoringSSL and therefore OpenSSL, so it
carries a bespoke composite licence with an explicit reproduction requirement, satisfied by its entry in
the appendix.

### The appendix covers all three shipped platforms, not just Windows

The dependency graph genuinely differs by platform, and for a while this appendix did not account for
that. On `x86_64-pc-windows-gnu` the downloader uses SChannel, so no TLS stack is statically linked. On
Linux and macOS there is no OS stack to borrow, so the build pulls in rustls, `ring`, `webpki-roots`,
`rustix` and the rest — twenty-five crates in total that a Windows-only appendix omits, including the
`security-framework` and `openssl` binding crates. Until 2026-08-02 `about.toml` pinned generation to
Windows while `ci.yml` copied the result verbatim into the Linux tarball and the macOS tarball, so the
paragraph above was false on two of the three platforms it was shipped with.

`about.toml` now resolves the **union** of all three shipped triples. One appendix, identical in every
archive. A Windows reader therefore receives notices for a few crates their binary does not contain,
which violates nothing; omitting one is the violation, and the asymmetry says err inclusive. The
`targets` list must stay in step with the release jobs in `ci.yml`, where macOS ships **aarch64**.

**Regenerating the appendix** after any dependency change (`about.toml` and `about.hbs` drive it):

```bash
cargo install cargo-about --locked --version 0.9.1 --features cli
cargo about generate -c about.toml about.hbs -o THIRD-PARTY-LICENSES.md \
  --workspace --features "download zstd-c phash mimalloc"
```

No `--target` flag: `about.toml` pins the triples itself, which is also why the check runs on any
runner. `--workspace` rather than `-m crates/cram-cli/Cargo.toml`: the zip ships three artifacts and
only one of them is the CLI. Resolving the whole workspace is the only single command that also covers
`cram-extract.exe` and `cram_shell.dll`. On Windows, convert the result to LF before committing
(`.gitattributes` stores it that way, and a handful of crates' own LICENSE files carry CRLF). CI
regenerates and diffs on every push, and the release job will not run unless that check passes, so an
appendix that has drifted from `Cargo.lock` fails the build instead of shipping.
