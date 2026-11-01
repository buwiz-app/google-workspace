#!/usr/bin/env bash
# Builds buwiz-gws for this machine, signs its manifest and the release
# index with the publisher key from the login Keychain (macOS asks you),
# and writes a GitHub release's assets to dist/release/:
#   buwiz-gws-<os>-<arch>    the binary (os, arch as Rust names them)
#   manifest.signed.json     the signed manifest it serves
#   buwiz-release.json       the signed index Buwiz Bot's app_install reads:
#                            id, version, each asset's sha256, how to run
#                            it (tools/release/release.json) and the
#                            environment the user supplies at install
# With --publish it then creates the release v<version> on GitHub (gh), or
# adds this platform's binary to it when it exists.
#   BUWIZ_BROKER_BIN  rox-broker from buwiz-bot/buwiz-bot (its sign-manifest)
#   KEY_ID            default: the manifest's publisher.key_id
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$REPO_DIR/dist/release"
GH_REPO=buwiz-app/google-workspace
PUBLISH=0
[ "${1:-}" = "--publish" ] && PUBLISH=1
: "${BUWIZ_BROKER_BIN:?set BUWIZ_BROKER_BIN to a rox-broker build (buwiz-bot/buwiz-bot broker/)}"

case "$(uname -s)" in Darwin) OS=macos ;; Linux) OS=linux ;; *) echo "unsupported OS $(uname -s)" >&2; exit 1 ;; esac
case "$(uname -m)" in arm64|aarch64) ARCH=aarch64 ;; x86_64) ARCH=x86_64 ;; *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;; esac
PLATFORM="$OS-$ARCH"
BIN_ASSET="buwiz-gws-$PLATFORM"

cd "$REPO_DIR"
if [ -n "$(git status --porcelain)" ]; then echo "the tree has changes: release a commit" >&2; exit 1; fi
COMMIT="$(git rev-parse HEAD)"
RUSTC_WRAPPER='' cargo build --release --locked >&2
rm -rf "$OUT" && mkdir -p "$OUT"
cp target/release/buwiz-gws "$OUT/$BIN_ASSET"
"$OUT/$BIN_ASSET" manifest > "$OUT/manifest.json"
VERSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$OUT/manifest.json")"
KEY_ID="${KEY_ID:-$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["publisher"]["key_id"])' "$OUT/manifest.json")}"

echo "signing the manifest with $KEY_ID (approve the Keychain prompt)" >&2
"$BUWIZ_BROKER_BIN" sign-manifest "$OUT/manifest.json" --keychain "$KEY_ID" --out "$OUT/manifest.signed.json"
rm "$OUT/manifest.json"

python3 - "$OUT" "$BIN_ASSET" "$PLATFORM" "$KEY_ID" tools/release/release.json <<'PY'
import hashlib, json, sys
out, bin_asset, platform, key_id, tpl = sys.argv[1:]
sha = lambda p: hashlib.sha256(open(p, "rb").read()).hexdigest()
m = json.load(open(f"{out}/manifest.signed.json"))
t = json.load(open(tpl))
index = {
    "v": 1,
    "id": m["id"],
    "version": m["version"],
    "publisher": {"key_id": key_id},
    "manifest": {"asset": "manifest.signed.json", "sha256": sha(f"{out}/manifest.signed.json")},
    "binaries": {platform: {"asset": bin_asset, "sha256": sha(f"{out}/{bin_asset}")}},
    "run": t["run"],
    "needs_env": t["needs_env"],
}
json.dump(index, open(f"{out}/buwiz-release.unsigned.json", "w"), indent=2)
PY
echo "signing the release index with $KEY_ID" >&2
"$BUWIZ_BROKER_BIN" sign-manifest "$OUT/buwiz-release.unsigned.json" --keychain "$KEY_ID" --out "$OUT/buwiz-release.json"
rm "$OUT/buwiz-release.unsigned.json"
echo "dist/release: v$VERSION for $PLATFORM at $COMMIT" >&2

[ "$PUBLISH" = 1 ] || { echo "dry run: --publish creates the release" >&2; exit 0; }
TAG="v$VERSION"
if gh release view "$TAG" -R "$GH_REPO" >/dev/null 2>&1; then
  # another platform's run: its binary, and an index listing every platform
  gh release download "$TAG" -R "$GH_REPO" -p buwiz-release.json -O "$OUT/published.json" --clobber
  echo "$TAG exists: merging $PLATFORM into its index" >&2
  python3 - "$OUT" <<'PY'
import json, sys
out = sys.argv[1]
old, new = json.load(open(f"{out}/published.json")), json.load(open(f"{out}/buwiz-release.json"))
if old["manifest"]["sha256"] != new["manifest"]["sha256"]:
    sys.exit("the published manifest differs from this build's: release a new version")
old["binaries"].update(new["binaries"])
old.pop("signature", None)
json.dump(old, open(f"{out}/buwiz-release.unsigned.json", "w"), indent=2)
PY
  "$BUWIZ_BROKER_BIN" sign-manifest "$OUT/buwiz-release.unsigned.json" --keychain "$KEY_ID" --out "$OUT/buwiz-release.json"
  rm "$OUT/buwiz-release.unsigned.json" "$OUT/published.json"
  gh release upload "$TAG" -R "$GH_REPO" --clobber "$OUT/$BIN_ASSET" "$OUT/buwiz-release.json"
else
  gh release create "$TAG" -R "$GH_REPO" --target "$COMMIT" --title "buwiz-gws $VERSION" \
    --notes "Google Workspace plugin $VERSION (app.buwiz.google-workspace). Install it in Buwiz Bot by asking the agent to install https://github.com/$GH_REPO" \
    "$OUT/$BIN_ASSET" "$OUT/manifest.signed.json" "$OUT/buwiz-release.json"
fi
echo "published $TAG" >&2
