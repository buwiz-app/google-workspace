#!/usr/bin/env python3
"""Fills package/plugin.json for a release: buwiz.files (the signed manifest)
and buwiz.binaries (this platform's binary), each with its sha256, and
writes <out>/plugin.unsigned.json for `buwiz-broker sign-manifest --block buwiz`.

usage: plugin-json.py <template> <out dir> <platform> <binary asset> [<published plugin.json>]

<out dir> holds manifest.signed.json and the binary asset. With a published
plugin.json (another platform's run of the same version), its binaries are
kept and this platform's is added; the manifest must be the same one.
"""
import hashlib
import json
import sys

MANIFEST = "manifest.signed.json"
BINARY_PATH = "bin/buwiz-gws"


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main(template, out, platform, bin_asset, published=None):
    p = json.load(open(template))
    b = p["buwiz"]
    m = json.load(open(f"{out}/{MANIFEST}"))
    for k in ("id", "short", "version"):
        if b[k] != m[k]:
            sys.exit(f"plugin.json's buwiz.{k} is {b[k]}; the served manifest's is {m[k]}")
    if p["version"] != b["version"]:
        sys.exit(f"plugin.json's version is {p['version']}; buwiz.version is {b['version']}")
    if b["publisher"]["key_id"] != m["publisher"]["key_id"]:
        sys.exit("plugin.json and the manifest name different publisher keys")
    b["files"] = {MANIFEST: {"asset": MANIFEST, "sha256": sha(f"{out}/{MANIFEST}")}}
    binaries = {}
    if published:
        old = json.load(open(published))["buwiz"]
        if old["version"] != b["version"]:
            sys.exit(f"the published plugin.json is {old['version']}, this build is {b['version']}")
        if old["files"] != b["files"]:
            sys.exit("the published manifest differs from this build's: release a new version")
        binaries.update(old["binaries"])
    binaries[platform] = {"path": BINARY_PATH, "asset": bin_asset, "sha256": sha(f"{out}/{bin_asset}")}
    b["binaries"] = dict(sorted(binaries.items()))
    b.pop("signature", None)
    with open(f"{out}/plugin.unsigned.json", "w") as f:
        json.dump(p, f, indent=2)
        f.write("\n")


if __name__ == "__main__":
    if len(sys.argv) not in (5, 6):
        sys.exit(__doc__.strip())
    main(*sys.argv[1:])
