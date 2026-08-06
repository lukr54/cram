#!/usr/bin/env python3
"""Build the cram benchmark corpus from public sources.

The corpus this replaces was assembled from a private photo library, which made every number
measured on it unreproducible by anybody else. This one is built entirely from material that can be
redistributed, and it is pinned hard enough that two people building it on different continents in
different years get byte-identical trees:

  * every download is checked against a hash recorded here, and a mismatch stops the build rather
    than quietly producing a different corpus;
  * photos come from a committed manifest, not from whatever Commons features this week;
  * every file's mtime is set to one fixed timestamp, because archivers store mtimes and a corpus
    whose timestamps drift cannot produce a byte-identical archive;
  * symlinks are excluded. cram skips them and reports each one, 7-Zip and RAR dereference them,
    and on a kernel tree that difference silently duplicates thousands of files. Removing them is
    what makes all three tools archive an identical file set.

Needs Python 3.8+ and about 8 GB of free disk. Nothing else -- no curl, no unzip, no git.

    python3 make-corpus.py --out ./cram-corpus-1.0

What it builds, and why each part is there:

    src/        kernel subsystems          many small, highly compressible files
    docs/       kernel Documentation/      prose, the file class source trees do not have
    photos/     202 Commons JPEGs          incompressible, and the JPEG recompression path
    video/      one Blender open movie     incompressible, one large file
    projects/   source with photos in it   the adversarial case for packing: a pack that is a run
                                           of the walk sees text and JPEG in the same pack, which
                                           never happens if media lives in its own folder
    dup/        exact copies of the above  the whole point. Delete this directory and rebuild the
                                           measurement to see the corpus without the assumption.
"""
import argparse
import hashlib
import io
import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.request
import zipfile

UA = "cram-bench/1.0 (https://github.com/lukr54/cram; benchmark corpus builder)"

# One fixed timestamp for every file in the corpus: 2026-01-01T00:00:00Z. Arbitrary, and it has to
# be *something* fixed or two builds produce archives that differ only in metadata.
MTIME = 1767225600

KERNEL_URL = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.tar.xz"
KERNEL_SHA256 = "b1a2562be56e42afb3f8489d4c2a7ac472ac23098f1ef1c1e40da601f54625eb"

VIDEO_URL = "https://download.blender.org/demo/movies/BBB/bbb_sunflower_1080p_30fps_normal.mp4.zip"
VIDEO_SHA256 = "e320fef389ec749117d0c1583945039266a40f25483881c2ff0d33207e62b362"
VIDEO_NAME = "big_buck_bunny_1080p30.mp4"

# Kernel subsystems for src/. Chosen to be a broad slice of real driver-adjacent and core code
# rather than the whole tree, which at 1.4 GB would swamp everything else.
SRC_DIRS = ["fs", "net", "sound", "include", "kernel", "lib", "crypto", "security", "block", "mm"]

# The twelve largest subdirectories of drivers/, skipping gpu/ (554 MB) and net/ (151 MB) because
# both are large enough to unbalance every project folder they land in.
PROJECT_DIRS = ["media", "scsi", "clk", "usb", "staging", "pinctrl",
                "accel", "infiniband", "iio", "video", "crypto", "input"]

PHOTOS_PER_PROJECT = 5
DUP_PROJECTS = 3      # projects/proj-01..03 are copied into dup/
DUP_PHOTOS = 26       # and the first 26 of photos/


def log(msg):
    print(msg, flush=True)


def fetch(url, dest, expect_sha256=None, expect_sha1=None, expect_size=None, tries=6):
    """Download once, verify, and never leave a half file behind for the next run to trust.

    Wikimedia serves the photo half and is donation-funded. It answers 429 when a client is being
    greedy, and the only correct response to that is to wait a long time -- retrying a 429 briskly
    is the thing it is asking you to stop doing. Partial progress is kept, so a build that gets
    throttled to a halt resumes where it stopped instead of starting the 1.6 GB again.
    """
    if os.path.exists(dest) and _verify(dest, expect_sha256, expect_sha1, expect_size):
        return dest
    tmp = dest + ".part"
    last = None
    for attempt in range(1, tries + 1):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": UA})
            with urllib.request.urlopen(req, timeout=120) as r, open(tmp, "wb") as f:
                shutil.copyfileobj(r, f, 1 << 20)
            if not _verify(tmp, expect_sha256, expect_sha1, expect_size):
                raise ValueError("checksum or size mismatch")
            os.replace(tmp, dest)
            return dest
        except urllib.error.HTTPError as e:
            last = e
            if os.path.exists(tmp):
                os.remove(tmp)
            if e.code in (429, 503) and attempt < tries:
                wait = _retry_after(e, default=45 * attempt)
                log(f"      throttled ({e.code}); waiting {wait}s")
                time.sleep(wait)
            elif attempt < tries:
                time.sleep(3 * attempt)
        except Exception as e:                       # noqa: BLE001 - report and retry, then give up
            last = e
            if os.path.exists(tmp):
                os.remove(tmp)
            if attempt < tries:
                time.sleep(3 * attempt)
    raise SystemExit(
        f"failed to fetch {url}: {last}\n"
        "Downloads already finished are cached -- re-run the same command to resume."
    )


def _retry_after(err, default):
    raw = err.headers.get("Retry-After") if err.headers else None
    try:
        return max(int(raw), 5)
    except (TypeError, ValueError):
        return default


def _verify(path, sha256=None, sha1=None, size=None):
    if size is not None and os.path.getsize(path) != size:
        return False
    if sha256 is None and sha1 is None:
        return True
    h = hashlib.sha256() if sha256 else hashlib.sha1()
    with open(path, "rb") as f:
        for blk in iter(lambda: f.read(1 << 20), b""):
            h.update(blk)
    return h.hexdigest() == (sha256 or sha1)


def copy_tree_no_links(src, dst):
    """Copy a directory, dropping symlinks and anything that is not a regular file.

    `shutil.copytree(symlinks=False)` would *follow* them, which is exactly the duplication this is
    here to avoid.
    """
    n = 0
    for root, dirs, files in os.walk(src, followlinks=False):
        dirs[:] = sorted(d for d in dirs if not os.path.islink(os.path.join(root, d)))
        rel = os.path.relpath(root, src)
        out = os.path.join(dst, rel) if rel != "." else dst
        os.makedirs(out, exist_ok=True)
        for name in sorted(files):
            s = os.path.join(root, name)
            if os.path.islink(s) or not os.path.isfile(s):
                continue
            shutil.copyfile(s, os.path.join(out, name))
            n += 1
    return n


def stamp(root):
    """One mtime for everything, deepest first so directories keep theirs."""
    for base, dirs, files in os.walk(root, topdown=False):
        for name in files:
            p = os.path.join(base, name)
            if not os.path.islink(p):
                os.utime(p, (MTIME, MTIME))
        for name in dirs:
            p = os.path.join(base, name)
            if not os.path.islink(p):
                os.utime(p, (MTIME, MTIME))
    os.utime(root, (MTIME, MTIME))


def tree_stats(root):
    n = 0
    total = 0
    for base, _dirs, files in os.walk(root):
        for name in files:
            p = os.path.join(base, name)
            if not os.path.islink(p):
                n += 1
                total += os.path.getsize(p)
    return n, total


def read_manifest(path):
    rows = []
    with open(path, encoding="utf-8") as f:
        header = f.readline().rstrip("\n").split("\t")
        for line in f:
            if not line.strip():
                continue
            rows.append(dict(zip(header, line.rstrip("\n").split("\t"))))
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="cram-corpus-1.0", help="directory to build into")
    ap.add_argument("--cache", default=".corpus-cache", help="where downloads are kept")
    ap.add_argument("--manifest", default="photos.tsv", help="pinned photo manifest")
    ap.add_argument("--delay", type=float, default=1.2,
                    help="seconds between photo downloads. Wikimedia throttles below about 1s, "
                         "and being throttled costs far more time than waiting does")
    args = ap.parse_args()

    out = os.path.abspath(args.out)
    cache = os.path.abspath(args.cache)
    os.makedirs(cache, exist_ok=True)
    if os.path.exists(out):
        raise SystemExit(f"{out} already exists; remove it or pass --out elsewhere")

    photos = read_manifest(args.manifest)
    log(f"photo manifest: {len(photos)} files, "
        f"{sum(int(p['size']) for p in photos) / 1e6:.1f} MB")

    # ---- sources -------------------------------------------------------------------------------
    log("\n[1/6] kernel tree")
    ktar = fetch(KERNEL_URL, os.path.join(cache, "linux-6.12.tar.xz"), expect_sha256=KERNEL_SHA256)
    kdir = os.path.join(cache, "linux-6.12")
    if not os.path.isdir(kdir):
        log("      extracting")
        import tarfile
        with tarfile.open(ktar) as t:
            t.extractall(cache)
    log(f"      {kdir}")

    log("\n[2/6] video")
    vzip = fetch(VIDEO_URL, os.path.join(cache, "bbb.zip"), expect_sha256=VIDEO_SHA256)
    vmp4 = os.path.join(cache, VIDEO_NAME)
    if not os.path.exists(vmp4):
        with zipfile.ZipFile(vzip) as z:
            inner = [n for n in z.namelist() if n.lower().endswith(".mp4")]
            if len(inner) != 1:
                raise SystemExit(f"expected one mp4 in {vzip}, found {inner}")
            with z.open(inner[0]) as src, open(vmp4, "wb") as dst:
                shutil.copyfileobj(src, dst, 1 << 20)
    log(f"      {vmp4} ({os.path.getsize(vmp4) / 1e6:.1f} MB)")

    log(f"\n[3/6] photos ({len(photos)} from Wikimedia Commons)")
    pdir = os.path.join(cache, "photos")
    os.makedirs(pdir, exist_ok=True)
    for i, p in enumerate(photos, 1):
        dest = os.path.join(pdir, p["name"])
        if os.path.exists(dest) and _verify(dest, sha1=p["sha1"], size=int(p["size"])):
            continue
        fetch(p["url"], dest, expect_sha1=p["sha1"], expect_size=int(p["size"]))
        time.sleep(args.delay)
        if i % 25 == 0:
            log(f"      {i}/{len(photos)}")
    log(f"      {len(photos)}/{len(photos)}")

    # ---- assemble ------------------------------------------------------------------------------
    log("\n[4/6] assembling")
    os.makedirs(out)

    n = 0
    for d in SRC_DIRS:
        n += copy_tree_no_links(os.path.join(kdir, d), os.path.join(out, "src", d))
    log(f"      src/       {n} files")

    n = copy_tree_no_links(os.path.join(kdir, "Documentation"), os.path.join(out, "docs"))
    log(f"      docs/      {n} files")

    os.makedirs(os.path.join(out, "video"))
    shutil.copyfile(vmp4, os.path.join(out, "video", VIDEO_NAME))
    log("      video/     1 file")

    # Projects: source and photographs interleaved in one walk, which is the case a per-folder
    # media layout never produces and real project directories always do.
    used = 0
    for idx, d in enumerate(PROJECT_DIRS, 1):
        proj = os.path.join(out, "projects", f"proj-{idx:02d}")
        copy_tree_no_links(os.path.join(kdir, "drivers", d), os.path.join(proj, "src"))
        os.makedirs(os.path.join(proj, "assets"), exist_ok=True)
        for k in range(PHOTOS_PER_PROJECT):
            p = photos[used]
            shutil.copyfile(os.path.join(pdir, p["name"]),
                            os.path.join(proj, "assets", f"shot_{k + 1:02d}.jpg"))
            used += 1
        # A text file first in the folder, so the very start of the walk compresses well.
        readme = os.path.join(kdir, "drivers", d, "Kconfig")
        if os.path.isfile(readme):
            shutil.copyfile(readme, os.path.join(proj, "NOTES.txt"))
    log(f"      projects/  {len(PROJECT_DIRS)} folders, {used} photos used")

    os.makedirs(os.path.join(out, "photos"))
    rest = photos[used:]
    for p in rest:
        shutil.copyfile(os.path.join(pdir, p["name"]), os.path.join(out, "photos", p["name"]))
    log(f"      photos/    {len(rest)} files")

    # The duplicate set. Named, contained in one directory, and removable, so the assumption it
    # encodes can be tested by deleting it rather than argued about.
    dup = os.path.join(out, "dup")
    os.makedirs(dup)
    for idx in range(1, DUP_PROJECTS + 1):
        shutil.copytree(os.path.join(out, "projects", f"proj-{idx:02d}"),
                        os.path.join(dup, f"proj-{idx:02d}-backup"))
    os.makedirs(os.path.join(dup, "photos-backup"))
    for p in rest[:DUP_PHOTOS]:
        shutil.copyfile(os.path.join(out, "photos", p["name"]),
                        os.path.join(dup, "photos-backup", p["name"]))
    dn, db = tree_stats(dup)
    log(f"      dup/       {dn} files, {db / 1e6:.1f} MB")

    # ---- paperwork -----------------------------------------------------------------------------
    log("\n[5/6] attribution and manifest")
    write_attribution(out, photos)
    stamp(out)
    write_checksums(out)

    log("\n[6/6] done")
    files, total = tree_stats(out)
    log(f"      {out}")
    log(f"      {files} files, {total} bytes ({total / 1e9:.2f} GB)")
    log(f"      duplicate set {db} bytes -- {100.0 * db / total:.1f}% of the corpus")
    for part in ["src", "docs", "photos", "video", "projects", "dup"]:
        pn, pb = tree_stats(os.path.join(out, part))
        log(f"        {part:10s} {pb / 1e6:8.1f} MB  {pn:6d} files")


def write_attribution(out, photos):
    lines = [
        "# Attribution",
        "",
        "Everything in this corpus is redistributable. Sources and licences:",
        "",
        "## Source code and documentation",
        "",
        "`src/`, `docs/` and `projects/*/src/` are the Linux kernel 6.12 source tree,",
        f"<{KERNEL_URL}>, GPL-2.0-only. Symlinks removed; otherwise unmodified.",
        "",
        "## Video",
        "",
        f"`video/{VIDEO_NAME}` is *Big Buck Bunny*, (c) 2008 Blender Foundation,",
        "<https://peach.blender.org>, Creative Commons Attribution 3.0.",
        "",
        "## Photographs",
        "",
        "All from Wikimedia Commons. Renamed to `photo_NNNN.jpg` so the corpus builds identically",
        "on every filesystem; the original title is the description-page link below. Files in",
        "`projects/*/assets/` are the first 60 of this list, renamed again per project.",
        "",
        "| file | author | licence | source |",
        "|---|---|---|---|",
    ]
    for p in photos:
        artist = p["artist"].replace("|", "/")
        lines.append(f"| {p['name']} | {artist} | {p['licence']} | <{p['descurl']}> |")
    lines.append("")
    with open(os.path.join(out, "ATTRIBUTION.md"), "w", encoding="utf-8") as f:
        f.write("\n".join(lines))


def write_checksums(out):
    """One sha256 per file, plus a single hash over that list -- the number to quote."""
    rows = []
    for base, dirs, files in os.walk(out):
        dirs[:] = sorted(dirs)
        for name in sorted(files):
            p = os.path.join(base, name)
            rel = os.path.relpath(p, out).replace(os.sep, "/")
            if rel in ("MANIFEST.sha256", "CORPUS.id"):
                continue
            h = hashlib.sha256()
            with open(p, "rb") as f:
                for blk in iter(lambda: f.read(1 << 20), b""):
                    h.update(blk)
            rows.append((rel, h.hexdigest()))
    rows.sort()
    body = "".join(f"{d}  {r}\n" for r, d in rows)
    with open(os.path.join(out, "MANIFEST.sha256"), "w", encoding="utf-8") as f:
        f.write(body)
    corpus_id = hashlib.sha256(body.encode()).hexdigest()
    with open(os.path.join(out, "CORPUS.id"), "w", encoding="utf-8") as f:
        f.write(corpus_id + "\n")
    log(f"      corpus id {corpus_id}")
    os.utime(os.path.join(out, "MANIFEST.sha256"), (MTIME, MTIME))
    os.utime(os.path.join(out, "CORPUS.id"), (MTIME, MTIME))


if __name__ == "__main__":
    main()
