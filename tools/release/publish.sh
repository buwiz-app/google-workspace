#!/usr/bin/env bash
# Builds buwiz-gws for this machine and writes a Buwiz mini app protocol 1.0
# GitHub release's assets to dist/release/ (flat names, as GitHub's are):
#   buwiz-gws-<os>-<arch>    the binary (os, arch as Rust names them);
#                            installed at bin/buwiz-gws
#   manifest.signed.json     the signed manifest it serves (buwiz://manifest)
#   plugin.json              package/plugin.json with buwiz.files and
#                            buwiz.binaries filled in (each sha256), its
#                            buwiz block signed
# Both are signed with the publisher key from the login Keychain: macOS
# asks twice (the manifest, then plugin.json).
#
# With --publish it then creates the release v<version> on GitHub (gh), or,
# when it exists (another platform's run), adds this platform's binary to
# its plugin.json, re-signs that and uploads both.
#
#   --prerelease       mark a new release as a pre-release: Buwiz Bot's
#                      install from the repo URL reads the latest release,
#                      which skips pre-releases (for a coordinated switch)
#   --key-file <path>  sign with a seed file instead (a throwaway key for
#                      local checks; never with --publish)
#   --trust <file>     check the result as Buwiz Bot does (`buwiz-broker
#                      manifest verify` and `manifest package`) against
#                      this trust store
#   BUWIZ_BROKER_BIN   buwiz-broker from buwiz-bot/buwiz-bot (its
#                      sign-manifest and manifest checks)
#   KEY_ID             default: the manifest's publisher.key_id
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$REPO_DIR/dist/release"
GH_REPO=buwiz-app/google-workspace
PUBLISH=0
PRERELEASE=()
KEY_FILE=
TRUST=
while [ $# -gt 0 ]; do
  case "$1" in
    --publish) PUBLISH=1 ;;
    --prerelease) PRERELEASE=(--prerelease) ;;
    --key-file) KEY_FILE="${2:?--key-file needs a path}"; shift ;;
    --trust) TRUST="${2:?--trust needs a file}"; shift ;;
    *) echo "usage: publish.sh [--publish [--prerelease]] [--key-file <seed file>] [--trust <trust.json>]" >&2; exit 2 ;;
  esac
  shift
done
: "${BUWIZ_BROKER_BIN:?set BUWIZ_BROKER_BIN to a buwiz-broker build (buwiz-bot/buwiz-bot broker/)}"
if [ "$PUBLISH" = 1 ] && [ -n "$KEY_FILE" ]; then echo "--publish signs with the Keychain key only" >&2; exit 2; fi

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
TAG="v$VERSION"
if [ -n "$KEY_FILE" ]; then SIGN=(--key-file "$KEY_FILE"); HOW="(a key file)"; else SIGN=(--keychain "$KEY_ID"); HOW="(approve the Keychain prompt)"; fi

echo "signing the manifest with $KEY_ID $HOW" >&2
"$BUWIZ_BROKER_BIN" sign-manifest "$OUT/manifest.json" "${SIGN[@]}" --out "$OUT/manifest.signed.json"
rm "$OUT/manifest.json"

PUBLISHED=()
EXISTS=0
if [ "$PUBLISH" = 1 ] && gh release view "$TAG" -R "$GH_REPO" >/dev/null 2>&1; then
  # another platform's run: keep the platforms the release already has
  EXISTS=1
  gh release download "$TAG" -R "$GH_REPO" -p plugin.json -O "$OUT/published.plugin.json" --clobber
  PUBLISHED=("$OUT/published.plugin.json")
  echo "$TAG exists: adding $PLATFORM to its plugin.json" >&2
fi
python3 tools/release/plugin-json.py package/plugin.json "$OUT" "$PLATFORM" "$BIN_ASSET" "${PUBLISHED[@]+"${PUBLISHED[@]}"}"
rm -f "$OUT/published.plugin.json"

echo "signing plugin.json's buwiz block with $KEY_ID $HOW" >&2
"$BUWIZ_BROKER_BIN" sign-manifest "$OUT/plugin.unsigned.json" --block buwiz "${SIGN[@]}" --out "$OUT/plugin.json"
rm "$OUT/plugin.unsigned.json"

if [ -n "$TRUST" ]; then
  # as Buwiz Bot's install checks them: the served manifest's signature,
  # then plugin.json's block for this platform
  python3 - "$OUT" "$TRUST" "$PLATFORM" "$BUWIZ_BROKER_BIN" <<'PY'
import json, subprocess, sys
out, trust_file, platform, broker = sys.argv[1:]
trust = json.load(open(trust_file))
def op(name, req):
    r = subprocess.run([broker, "manifest", name], input=json.dumps(req), capture_output=True, text=True)
    a = json.loads(r.stdout or "{}")
    if not a.get("ok"):
        sys.exit(f"buwiz-broker manifest {name}: {r.stdout.strip() or r.stderr.strip()}")
    return a["value"]
served = op("verify", {"text": open(f"{out}/manifest.signed.json").read(), "trust": trust})["manifest"]
p = op("package", {"text": open(f"{out}/plugin.json").read(), "trust": trust, "platform": platform})
for k in ("id", "short", "version"):
    if served[k] != p[k]:
        sys.exit(f"the served manifest's {k} is {served[k]}; plugin.json's is {p[k]}")
print(f"checked: {p['id']} {p['version']} for {platform}, accounts {[a['id'] for a in p['accounts']]}, binary {p['binary']['asset']}", file=sys.stderr)
PY
fi
echo "dist/release: $TAG for $PLATFORM at $COMMIT" >&2

[ "$PUBLISH" = 1 ] || { echo "dry run: --publish creates the release" >&2; exit 0; }
if [ "$EXISTS" = 1 ]; then
  gh release upload "$TAG" -R "$GH_REPO" --clobber "$OUT/$BIN_ASSET" "$OUT/plugin.json"
else
  gh release create "$TAG" -R "$GH_REPO" --target "$COMMIT" ${PRERELEASE[@]+"${PRERELEASE[@]}"} --title "buwiz-gws $VERSION" \
    --notes "Google Workspace plugin $VERSION (app.buwiz.google-workspace), Buwiz mini app protocol 1.0: Gmail, Calendar and Drive with the Google account you sign in to in Buwiz Bot. Install it by asking Buwiz Bot's agent to install https://github.com/$GH_REPO" \
    "$OUT/$BIN_ASSET" "$OUT/manifest.signed.json" "$OUT/plugin.json"
fi
echo "published $TAG" >&2
