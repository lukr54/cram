# The `.cram` archive format

**Status: frozen, versions 1 and 2.** This document is the normative specification of the on-disk
`.cram` container as produced and consumed by Cram. Two version bytes are defined (§2): v1, and v2,
which adds the per-entry transform byte and nothing else (§6, §13). Frozen means the layout of a
defined version never changes; any future change bumps the version byte and is described in a new
revision of this document. A conforming reader **must** reject anything it does not understand rather
than guess (see §9).

The reference implementation lives in
[`crates/cram-core/src/formats/cram.rs`](../crates/cram-core/src/formats/cram.rs); where this
document and the code disagree, that is a bug in one of them; please file it.

- **Endianness:** every multi-byte integer is **little-endian**, unsigned, unless stated otherwise.
- **Notation:** `u8`/`u32`/`u64` are 1/4/8-byte little-endian unsigned integers. `bytes(n)` is a
  fixed run of `n` bytes. `A | B` is concatenation.
- **Offsets** are absolute byte positions from the start of the file.

---

## 1. File layout

```text
+----------------+  offset 0
|   header       |  8 bytes                     (§2)
+----------------+  offset 8
|   crypto block |  28 bytes, PRESENT ONLY IF   (§3)
|                |  the ENCRYPTED flag is set
+----------------+  offset = packs_start
|   packs region |  pack[0] | pack[1] | ...     (§4)
|                |  each pack is a blob whose
|                |  length is index-recorded
+----------------+  offset = index_offset
|   index        |  index_len bytes             (§5, §6)
+----------------+  offset = file_len - 22
|   trailer      |  22 bytes                    (§7)
+----------------+  offset = file_len
```

`packs_start` is `8` for an unencrypted archive and `8 + 28 = 36` for an encrypted one. The index
sits at the end of the packs region (immediately before the trailer) so the writer can stream packs
out in a single forward pass and only needs to seek once, never at all, in practice, since it
appends. The trailer records where the index begins, so a reader finds everything by reading the
fixed-size trailer first.

---

## 2. Header (8 bytes)

| field    | type      | value                                             |
|----------|-----------|---------------------------------------------------|
| magic    | bytes(6)  | `43 52 41 4D 1B 01` = ASCII `CRAM` then `0x1B 0x01` |
| version  | u8        | `0x01`, or `0x02` when any entry carries a transform (§6) |
| flags    | u8        | bit 0 = **ENCRYPTED**; bits 1–7 reserved, **must be 0** |

The same 6-byte magic appears again at the very end of the file (in the trailer), so the format is
recognizable from both ends.

A reader **must**:

- reject the file if the 6-byte magic does not match;
- reject the file if `version` is neither `0x01` nor `0x02` (a newer archive is not assumed to be
  backward-compatible);
- treat `version` as selecting the `EntryMeta` shape: v2 records carry a trailing `transform` byte,
  v1 records do not. A writer emits `0x02` **only** when it actually stored a transformed entry, so
  archives that use no transform stay readable by v1-only readers. This is why the version gate
  matters: a v1-only reader encountering a v2 archive refuses it outright rather than writing a
  transformed stream to disk as though it were the original file;
- reject the file if any reserved flag bit (`flags & 0xFE`) is set.

---

## 3. Crypto block (28 bytes, encrypted archives only)

Present **iff** the ENCRYPTED flag is set. Immediately follows the header.

| field   | type      | meaning                                       |
|---------|-----------|-----------------------------------------------|
| salt    | bytes(16) | random per-archive Argon2 salt                |
| m_cost  | u32       | Argon2 memory cost, in KiB                     |
| t_cost  | u32       | Argon2 time cost (iterations)                  |
| p_cost  | u32       | Argon2 parallelism (lanes)                     |

The cost parameters are stored so they stay tunable without a format change. Because they come from
an untrusted file, a reader **must** clamp them before use (§8): reject `m_cost > 1048576` (1 GiB),
`t_cost > 64`, or `p_cost > 16`. The reference writer emits `m_cost = 19456` (19 MiB), `t_cost = 2`,
`p_cost = 1`, the OWASP-recommended Argon2id floor.

---

## 4. Packs region

The packs region is the concatenation of every pack blob, back to back, with no padding or
separators. A pack is a group of unique file chunks (§10) compressed, and, if encrypted, sealed; 
as one unit. Nothing in the region is self-describing: a pack's location, on-disk length, codec, and
decompressed length all come from the pack table in the index (§6). A reader therefore never scans
the region; it seeks to a pack by its index entry.

A single pack's decompressed size is bounded to **64 MiB** (`MAX_PACK_RAW`); a reader must reject an
archive whose index declares a larger `raw_len` (§8).

---

## 5. Index framing

The index is a single byte string (§6). If the archive is unencrypted, it is stored verbatim at
`index_offset`. If the archive is encrypted, the bytes at `index_offset` are the **sealed** index
(§8): `nonce(12) | ciphertext | tag(16)`, and `index_len` counts those sealed bytes. The index's own
GCM authentication tag doubles as the password verifier, a wrong password fails the tag check on
open, cleanly, before any index byte is trusted.

---

## 6. Index body (plaintext)

After decryption (if any), the index is the following structure. All counts are `u32`. A reader
**must not** pre-allocate from a count (§9); grow as you parse so a bogus count runs out of input
instead of exhausting memory.

```text
index := pack_table | chunk_table | entry_table

pack_table  := pack_count:u32  then pack_count × PackLoc
PackLoc     := file_offset:u64 | comp_len:u64 | raw_len:u32 | codec:u8      (21 bytes)

chunk_table := chunk_count:u32 then chunk_count × ChunkLoc
ChunkLoc    := pack_id:u32 | offset:u32 | length:u32                        (12 bytes)

entry_table := entry_count:u32 then entry_count × EntryMeta
EntryMeta   := is_dir:u8 | name_len:u32 | name:bytes(name_len)
             | size:u64 | mode:u32 | chunk_id_count:u32
             | chunk_ids:u32 × chunk_id_count
             | transform:u8                                   (v2 archives only)
```

**transform** (v2 only), a reversible, *lossless* transform applied to the entry's bytes before
chunking. A reader that cannot reverse a transform it encounters **must** reject the archive.

| value | meaning |
|-------|---------|
| `0x00` | `NONE`, stored exactly as read. |
| `0x01` | `LEPTON`, a JPEG stored as a Lepton stream; extraction reconstructs the original file byte-for-byte. |

For a transformed entry, `size` is the **reconstructed** (original) length, so listings and extraction
report the file the user actually gets. Consequently `size == Σ chunk length` does **not** hold for
such entries (§9); the stored stream is smaller. A reader **must** instead verify that the
reconstruction is exactly `size` bytes long, and **must** bound `size` against the stored length
before trusting it in any budget calculation.

**PackLoc**, one pack:
- `file_offset`, absolute offset of the pack blob in the packs region.
- `comp_len`, the pack blob's **on-disk** length. For an encrypted archive this **includes** the
  12-byte nonce and 16-byte GCM tag (i.e. it is the length of the sealed blob, not of the
  compressed plaintext).
- `raw_len`, the pack's length **after** decompression (and after decryption, when encrypted).
- `codec`, how the pack plaintext is compressed: `0` = STORE (raw, uncompressed), `1` = XZ
  (an `.xz`/LZMA2 stream), `2` = ZSTD (a single zstd frame). See §11.

**ChunkLoc**, one unique chunk, addressed **within its pack's decompressed bytes**:
- `pack_id`, index into the pack table.
- `offset`, byte offset of the chunk inside pack `pack_id`'s decompressed bytes.
- `length`, chunk length in bytes. `offset + length` must be `≤` that pack's `raw_len`.

**EntryMeta**, one archive member:
- `is_dir`, `0` for a file, non-zero for a directory.
- `name`, the member path, UTF-8, forward-slash separated, no leading slash. Two distinct rules:
  - a name that is **not valid UTF-8** is corruption, the reference reader rejects the whole
    archive (§9 item 13);
  - a name that is valid UTF-8 but **unsafe as a path** (`..` traversal, drive letter / absolute
    path, NUL, a reserved device name) must be sanitized before use as a filesystem path. The
    reference reader **silently drops** such an entry from the listing (it is neither listed nor
    extracted) rather than rejecting the archive.
- `size`, the reconstructed file length in bytes. Invariant: `size == Σ length` over the entry's
  `chunk_ids` (§8). Directories have `size == 0` and no chunks.
- `mode`, Unix permission bits, or `0` if unknown/not applicable.
- `chunk_ids`, the ordered list of chunk-table indices whose bytes, concatenated in this order,
  reconstruct the file body. Ids may repeat (in-file dedup); the same id may appear in many entries
  (cross-file dedup).

---

## 7. Trailer (22 bytes)

The last 22 bytes of the file.

| field        | type      | meaning                                    |
|--------------|-----------|--------------------------------------------|
| index_offset | u64       | absolute offset where the index begins     |
| index_len    | u64       | length of the index bytes (sealed, if enc.) |
| magic        | bytes(6)  | `CRAM\x1b\x01` again, as an end marker      |

A reader locates the index by reading the trailer first (it is at a fixed distance from EOF), then
seeking to `index_offset`.

---

## 8. Cryptography (encrypted archives)

- **Key derivation:** `key = Argon2id(password, salt, m_cost, t_cost, p_cost)`, Argon2 version
  `0x13` (v1.3), 32-byte output. `salt` and the cost parameters come from the crypto block (§3).
- **Cipher:** AES-256-GCM. A sealed blob is `nonce(12) | ciphertext | tag(16)`. Each blob uses a
  fresh random 12-byte nonce.
- **Order:** on write, **compress then encrypt**; on read, **decrypt then decompress**.
- **Associated data (AAD)** binds each blob to its role, so blobs cannot be swapped or replayed:
  - a **pack** with pack id *N* uses the 4-byte little-endian encoding of *N* as AAD;
  - the **index** uses the ASCII bytes `cram-index` (`63 72 61 6D 2D 69 6E 64 65 78`) as AAD.
- **Password verification:** opening the index's GCM tag is the password check, no separate
  verifier is stored. A wrong password (or any tampering) surfaces as an authentication failure.

In v1, encryption is all-or-nothing: when the ENCRYPTED flag is set, **every** pack and the index
are sealed, so the file listing itself requires the password. There is no "encrypt contents but not
names" mode in v1.

---

## 9. Mandatory reader validation

A `.cram` file may be hostile. A conforming reader **must** enforce all of the following and treat
any violation as corruption (never a panic, never an unbounded allocation):

1. `file_len ≥ 8 + 22`, and `≥ 8 + 28 + 22` when the ENCRYPTED flag is set.
2. Header magic (§2), version, and reserved-flag checks.
3. Trailer magic (§7).
4. The index lies wholly within `[packs_start, file_len − 22)`. Check with subtraction, never with
   `index_offset + index_len` (that sum is attacker-controlled and can wrap `u64`).
5. Every pack lies wholly within `[packs_start, index_offset)`, checked by subtraction for the same
   reason, and `raw_len ≤ 64 MiB`.
6. Every `ChunkLoc`: `pack_id < pack_count` and `offset + length ≤ pack.raw_len`.
7. Every entry `chunk_id < chunk_count`, and `size == Σ length` over the entry's chunks.
8. On decompression, a pack must expand to **exactly** its declared `raw_len` (a codec that yields a
   different length is corruption).
9. Argon2 parameter caps (§3).
10. **Anti-amplification budget:** reconstructing one entry may decompress at most
    `max(256 MiB, 1000 × file_len)` bytes total. This bounds a hostile `chunk_ids` list that
    alternates packs to force repeated full-pack decompression (a bomb from a tiny file). The
    `size == Σ length` invariant (7) bounds output; this bounds work.
11. Counts (`pack_count`, `chunk_count`, `entry_count`, `chunk_id_count`, `name_len`) are never used
    to pre-size an allocation; parse incrementally so a bogus count fails on exhausted input.
12. Reject an unknown pack `codec`, only STORE (0), XZ (1), and ZSTD (2) are defined (§11).
13. Every entry `name` must be valid UTF-8; a non-UTF-8 name is corruption and rejects the archive.
    (A name that is valid UTF-8 but unsafe as a path is *not* corruption, see §6.)

---

## 10. Chunking and deduplication (informative)

This section describes how the reference **writer** fills packs. A reader does not need it, the
index fully determines reconstruction, but it explains why the format deduplicates.

- Each file body is split into content-defined chunks with **FastCDC v2020**, parameters
  `min = 16 KiB`, `avg = 64 KiB`, `max = 256 KiB`. Content-defined boundaries mean an insertion near
  the start of a file only re-chunks the region around it, so shared regions across files (and
  across versions of a file) produce identical chunks.
- A chunk's identity is its **BLAKE3** hash (256-bit). The writer keeps a hash→chunk-id table; a
  chunk whose hash is already present is not stored again, its id is simply referenced.
- Unique chunks accumulate into a pack buffer; when the buffer reaches ~8 MiB (`PACK_TARGET`) it is
  flushed as a pack (§4). A pack that compression does not shrink is stored with codec STORE so a
  pack never grows.

The dedup identity (BLAKE3) is a writer-side concern only; the format records chunk **locations**,
not hashes, so a reader neither computes nor trusts any hash.

---

## 11. Codec catalogue

| codec | id | meaning                                                                 |
|-------|----|-------------------------------------------------------------------------|
| STORE | 0  | pack plaintext is the raw bytes, uncompressed                            |
| XZ    | 1  | pack plaintext is an XZ (LZMA2) stream; `raw_len` is its decoded length   |
| ZSTD  | 2  | pack plaintext is a single zstd frame; `raw_len` is its decoded length    |

Every build can **decode** all three codecs (the XZ and zstd decoders are pure-Rust and always
present), so any `.cram` file is readable regardless of which build produced it. Which codec a
**writer** emits is a build/config choice (the default writer uses XZ; a `zstd-c` build may write
ZSTD), and does not affect readability.

---

## 12. Reproducibility

An **unencrypted** `.cram` is **deterministic**: building it twice from the same logical inputs
(the same top-level inputs, **given in the same order**, with the same file bytes and same relative
paths) with the same options and the same build of Cram yields a **byte-for-byte identical** file.
This is a guaranteed property, not an accident; it makes a `.cram` safe to content-address, cache by
hash, and verify against a published checksum.

It holds because nothing in the format or the writer depends on wall-clock time, absolute paths, or
run-to-run randomness:

- The format stores **no timestamps** and **no absolute paths** (entry names are relative, rooted at
  the input's base name).
- The create walk **sorts** each directory's children, so on-disk enumeration order does not leak in.
  (The **order of the top-level inputs** is preserved as given, so it is part of "same inputs"; the
  same files listed in a different order produce a different, though internally valid, archive.)
- Chunking (FastCDC), dedup identity (BLAKE3), pack assembly, and index serialization are pure
  functions of the input bytes.
- Pack compression is parallelized but **order-preserving**: each pack is compressed independently and
  written in pack-id order, so the thread/batch count never changes the bytes.

Two caveats:

- **Same build.** Which pack codec a writer emits (XZ vs ZSTD) and the exact compressed bytes depend
  on the compressor build/version. A different Cram build may produce a different (still valid) file;
  every build can still *read* it (§11).
- **Encrypted archives are intentionally NOT reproducible.** Each carries a fresh random Argon2 salt
  and a fresh AES-GCM nonce per sealed blob (reusing a GCM nonce would be catastrophic), so two
  encrypted builds of the same input necessarily differ. Reproducibility is a property of the
  *plaintext* format only.

## 13. Version history

- **v1**, initial frozen format: content-defined chunking + BLAKE3 dedup, solid packs
  (STORE/XZ/ZSTD), footer index, trailer, optional whole-archive AES-256-GCM with Argon2id.
- **v2**, adds the per-entry `transform` byte (§6) and, with it, `LEPTON`: lossless JPEG
  recompression. Photos are already entropy-coded, so a general-purpose compressor gains ~0% on them;
  redoing that coding is worth ~23% while still reconstructing the original file byte-for-byte. The
  writer emits v2 only when a transform was actually used, and verifies every candidate round-trips
  before storing it, a file that fails to verify is stored untransformed. Everything else is
  unchanged from v1.
