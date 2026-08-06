# The Cram benchmark corpus

A 2.8 GB, 42,151-file corpus built entirely from material anyone can download and redistribute, so
the numbers measured on it can be checked by someone who is not us.

```sh
python3 make-corpus.py --out ./cram-corpus-1.0
./bench-corpus.sh ./cram-corpus-1.0 /tmp/bench 3
```

Needs Python 3.8+ and about 8 GB of free disk. Nothing else — no curl, no unzip, no git.

## What is in it, and why

| | | |
|---|---|---|
| `src/` | 204 MB, 15,380 files | Linux 6.12 subsystems: many small, highly compressible files |
| `docs/` | 50 MB, 10,121 files | the kernel's `Documentation/`: prose, a file class source trees do not have |
| `photos/` | 1,158 MB, 142 files | Wikimedia Commons photographs: incompressible, and the JPEG path |
| `video/` | 276 MB, 1 file | *Big Buck Bunny*: incompressible, one large file |
| `projects/` | 688 MB, 11,456 files | source with photographs interleaved among it |
| `dup/` | 421 MB, 5,048 files | exact copies of some of the above |

`projects/` is the adversarial case for packing. A `.cram` pack is a run of the walk, so it decides
store-vs-compress for text and JPEG together whenever a folder holds both. Keep media in its own
directory and the walk separates the two for free, which is not what real project folders look like.

`dup/` is **15.0% of the corpus**, and it is the assumption the whole thing rests on: deduplication
finds nothing in a corpus that never repeats itself. It is one directory, named, and deletable —
`rm -rf dup/` and re-run to see the corpus without it. That number is chosen to be defensible for a
working drive, not tuned; if you think it is generous, delete it and measure both.

## Reproducibility

Built twice into different directories on the same machine, byte-identical:

```
corpus id  deb5f932d27a913ad6da2b994be7e66bffd03d6bf8546abd3de8ca7344efe599
```

`CORPUS.id` is a SHA-256 over `MANIFEST.sha256`, which lists every file. If yours matches, you have
the same corpus. Three things make that hold:

- every download is checked against a digest pinned in `make-corpus.py`, and a mismatch **stops the
  build** rather than quietly producing a different corpus;
- photographs come from `photos.tsv`, a committed manifest of 202 exact files with their sizes and
  the SHA-1s Commons publishes, rather than from whatever Commons happens to feature this week;
- every mtime is set to one fixed timestamp, because archivers store mtimes and a corpus whose
  timestamps drift cannot produce a byte-identical archive.

Symlinks are excluded. Cram skips them and reports each one, 7-Zip and RAR dereference them, and on
a kernel tree that difference silently duplicates thousands of files. Removing them is what makes
all three tools archive an identical file set.

## Licensing

Everything is redistributable. `ATTRIBUTION.md` is generated into the corpus with the author,
licence and source URL of every photograph.

- Kernel source and documentation — GPL-2.0-only, from cdn.kernel.org
- *Big Buck Bunny* — CC BY 3.0, © 2008 Blender Foundation
- Photographs — CC0, public domain, CC BY or CC BY-SA, from Wikimedia Commons

GFDL and Free Art Licence files were deliberately excluded when the manifest was generated. Both are
free, and both carry obligations heavier than a benchmark corpus should ask of the people using it.

## Regenerating the photo manifest

`photos.tsv` is committed and should not normally be rebuilt: doing so changes which 202 photographs
the corpus contains, and therefore every number ever measured on it. It exists for the day a source
file is deleted from Commons.

```sh
python3 gen_photo_manifest.py > photos.tsv
```

Selection is stated in full at the top of that script rather than left to taste, because a corpus
assembled by taste is one somebody can accuse of having been assembled for the result.

## Running the benchmark

`bench-corpus.sh` takes a corpus directory, a work directory and a round count. Every rule it
follows is written down in its header; the ones worth knowing before reading a result:

- RAR gets `-s`. 7-Zip is solid by default and RAR is not, and measuring RAR without it costs it
  about 10% of its ratio for a reason that has nothing to do with RAR.
- Both competitors are given every thread explicitly.
- Every tool is measured at both ends of its own range. Comparing our maximum against their default
  is the standard way these tables mislead.
- `sync` is inside the timed region for extraction. Without it an extraction stops the clock with
  gigabytes still in the page cache and whatever runs next pays for the writes.
- Every tool runs under a memory cap with swap denied, so one that wants more than the machine has
  is killed and recorded as such instead of taking the host down with it.
- Extraction is measured to disk *and* to tmpfs. The first is the real-world number; the second
  removes the write wall and leaves the decoder. They answer different questions.
