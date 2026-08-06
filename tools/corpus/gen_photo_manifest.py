#!/usr/bin/env python3
"""Pin the photo half of the benchmark corpus to an exact, checkable list.

Run once. The output is committed and is what everybody afterwards downloads from, so that two
people building the corpus a year apart get the same 202 files rather than whatever Commons
happens to feature that week. Every row carries the size and the SHA-1 Commons publishes, so the
builder can verify each download instead of trusting the network.

Selection is deliberately boring and stated in full, because a corpus assembled by taste is a
corpus somebody can accuse of being assembled for the result:

  * seven subject categories with a fixed quota each, so the photo half is not 200 landscapes;
  * JPEG only, 1.5-14 MB, so no single file dominates and every file is a real photograph;
  * licences limited to CC0, public domain, CC BY and CC BY-SA. GFDL and the Free Art Licence are
    free too but drag in obligations heavier than a benchmark corpus should carry;
  * ordered by the file's own SHA-1. Ordering by title would select by alphabet, which on Commons
    means selecting by language and country.
"""
import json
import urllib.parse
import urllib.request
import sys
import time

UA = "cram-bench/1.0 (https://github.com/lukr54/cram; benchmark corpus builder)"
API = "https://commons.wikimedia.org/w/api.php"

QUOTAS = [
    ("Featured pictures of landscapes", 45),
    ("Featured pictures of buildings", 45),
    ("Featured pictures of people", 35),
    ("Featured pictures of plants", 30),
    ("Quality images of food", 30),
    ("Featured pictures of vehicles", 12),
    ("Quality images of animals", 5),
]

MIN_BYTES = 1_500_000
MAX_BYTES = 14_000_000
OK_LICENCES = ("cc0", "public domain", "cc by")   # matched case-insensitively on the prefix


def licence_ok(short):
    s = (short or "").strip().lower()
    return any(s.startswith(p) for p in OK_LICENCES)


def fetch_category(cat, want):
    """Every usable file in one category, following continuation until Commons runs out."""
    out = {}
    cont = {}
    for _ in range(40):                      # hard stop; no category needs more than 40 pages here
        params = {
            "action": "query", "format": "json",
            "generator": "categorymembers",
            "gcmtitle": "Category:" + cat,
            "gcmtype": "file", "gcmlimit": "500",
            "prop": "imageinfo",
            "iiprop": "url|size|sha1|mime|extmetadata",
        }
        params.update(cont)
        url = API + "?" + urllib.parse.urlencode(params)
        req = urllib.request.Request(url, headers={"User-Agent": UA})
        with urllib.request.urlopen(req, timeout=90) as r:
            d = json.load(r)
        for p in d.get("query", {}).get("pages", {}).values():
            ii = (p.get("imageinfo") or [{}])[0]
            if ii.get("mime") != "image/jpeg":
                continue
            size = ii.get("size", 0)
            if not (MIN_BYTES <= size <= MAX_BYTES):
                continue
            em = ii.get("extmetadata", {})
            lic = em.get("LicenseShortName", {}).get("value", "")
            if not licence_ok(lic):
                continue
            artist = em.get("Artist", {}).get("value", "") or "unknown"
            # Artist arrives as HTML. Flatten it: the manifest is a text file people read.
            artist = _strip_html(artist)
            out[ii["sha1"]] = {
                "title": p["title"],
                "url": ii["url"].split("?")[0],
                "sha1": ii["sha1"],
                "size": size,
                "licence": lic,
                "artist": artist,
                "descurl": ii.get("descriptionurl", ""),
            }
        if "continue" not in d:
            break
        cont = d["continue"]
        time.sleep(0.4)                      # Commons is a donation-funded host; do not hammer it
    picked = sorted(out.values(), key=lambda r: r["sha1"])[:want]
    return picked


def _strip_html(s):
    out, depth = [], 0
    for ch in s:
        if ch == "<":
            depth += 1
        elif ch == ">":
            depth = max(0, depth - 1)
        elif depth == 0:
            out.append(ch)
    return " ".join("".join(out).split()).replace("\t", " ")


rows = []
for cat, want in QUOTAS:
    got = fetch_category(cat, want)
    sys.stderr.write(f"{cat:38s} {len(got):3d}/{want}  {sum(r['size'] for r in got)/1e6:7.1f} MB\n")
    for r in got:
        r["category"] = cat
    rows.extend(got)

# One global order, again by content hash, so the numbering does not encode the category order.
rows.sort(key=lambda r: r["sha1"])
print("seq\tname\tsize\tsha1\tlicence\tartist\turl\tdescurl\tcategory")
for i, r in enumerate(rows, 1):
    print("\t".join([
        f"{i:04d}", f"photo_{i:04d}.jpg", str(r["size"]), r["sha1"],
        r["licence"], r["artist"], r["url"], r["descurl"], r["category"],
    ]))
sys.stderr.write(f"\ntotal {len(rows)} photos, {sum(r['size'] for r in rows)/1e6:.1f} MB\n")
