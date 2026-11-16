#!/usr/bin/env bash
# tools/conformance.sh: the SDK's plugin conformance runner (buwiz-plugin-test)
# against this plugin's binary, under a throwaway key and a per-run trust file.
# Nothing here touches the Keychain, a Bot's data or the release key.
#
# Usage: tools/conformance.sh [runner args, e.g. --json]
# Env:   BUWIZ_SDK_DIR  a buwiz-sdk checkout (https://github.com/buwiz-app/buwiz-sdk)
#                       at the revision this repo pins (crates/gws-plugin/Cargo.toml),
#                       with its dependencies installed (bun install)
#        BUWIZ_BROKER_BIN  a buwiz-broker binary; signs the test manifest in place
#                       of the SDK's sign-manifest.mjs (the signer a release uses)
#        GWS_TEST_KEY_ID  the test key id (default gws-test-01; never the release key id)
#        KEEP_TMP=1     keep the temp dir (it holds a test key)
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SDK="${BUWIZ_SDK_DIR:-}"
KEY_ID="${GWS_TEST_KEY_ID:-gws-test-01}"
PLUGIN_ID=app.buwiz.google-workspace
RUNNER="$SDK/packages/conformance/bin/buwiz-plugin-test.mjs"
SIGNER="$SDK/packages/conformance/src/v1/sign-manifest.mjs"

die() { echo "conformance: $*" >&2; exit 2; }
if [ -n "${BUWIZ_BROKER_BIN:-}" ]; then
  [ -x "$BUWIZ_BROKER_BIN" ] || die "BUWIZ_BROKER_BIN $BUWIZ_BROKER_BIN is not executable"
  SIGN=("$BUWIZ_BROKER_BIN" sign-manifest)
else
  SIGN=(node "$SIGNER")
fi
[ "$KEY_ID" != "$(sed -n 's/.*"key_id": *"\([^"]*\)".*/\1/p' "$REPO/package/plugin.json" | head -1)" ] || die "the test key id must not be the release key id"
[ -n "$SDK" ] || die "set BUWIZ_SDK_DIR to a buwiz-sdk checkout (git clone https://github.com/buwiz-app/buwiz-sdk; cd buwiz-sdk; bun install)"
for f in "$RUNNER" "$SIGNER"; do [ -f "$f" ] || die "$f not found: is BUWIZ_SDK_DIR a buwiz-sdk checkout?"; done
PIN="$("$REPO/tools/sdk-pin.sh")" || die "the buwiz-sdk pins are not one revision (tools/sdk-pin.sh says why)"
# the checkout must hold exactly the pinned files (a merge of the pin has another commit id)
SDK_TREE="$(git -C "$SDK" rev-parse 'HEAD^{tree}' 2>/dev/null)" || die "$SDK is not a git checkout of buwiz-sdk"
[ "$(git -C "$SDK" rev-parse "$PIN^{tree}" 2>/dev/null)" = "$SDK_TREE" ] ||
  die "the SDK checkout is not the pinned revision $PIN: git -C \"\$BUWIZ_SDK_DIR\" fetch && git -C \"\$BUWIZ_SDK_DIR\" checkout $PIN"
git -C "$SDK" diff --quiet HEAD -- packages/conformance || die "the SDK checkout has uncommitted changes under packages/conformance"
[ -z "$(git -C "$SDK" status --porcelain --untracked-files=all -- packages/conformance)" ] || die "the SDK checkout has untracked files under packages/conformance"
command -v node >/dev/null || die "node is needed to run the SDK's tools"
(cd "$SDK/packages/conformance" && node --input-type=module -e 'await import("esbuild"); await import("@modelcontextprotocol/sdk/client/index.js"); await import("@modelcontextprotocol/sdk/client/stdio.js"); await import("node:sqlite")') >/dev/null 2>&1 ||
  die "the SDK's runner cannot load its dependencies or node:sqlite: it needs Node 22.13 or newer (node --version), and bun install run in $SDK"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/gws-conformance.XXXXXX")"; chmod 700 "$TMP"
cleanup() { if [ "${KEEP_TMP:-0}" = 1 ]; then echo "kept $TMP" >&2; else rm -rf "$TMP"; fi; }
trap cleanup EXIT

(cd "$REPO" && CARGO_INCREMENTAL=0 cargo build --quiet -p buwiz-gws)
BIN="$(cd "$REPO" && cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/buwiz-gws"

node "$RUNNER" --keygen "$TMP/test.key" >"$TMP/keygen.txt"
PUB="$(sed -n 's/.*Public key: \([A-Za-z0-9_-]*\).*/\1/p' "$TMP/keygen.txt")"
[ -n "$PUB" ] || { echo "conformance: keygen printed no public key" >&2; exit 1; }
cat >"$TMP/trust.json" <<JSON
{ "v": 1, "about": "Per-run trust for conformance: a throwaway test key, never a release key.",
  "publishers": { "$KEY_ID": { "publisher": "app.buwiz", "alg": "ed25519", "public_key": "$PUB", "first_party": true } },
  "approved_plugins": [ { "id": "$PLUGIN_ID", "publisher_key_id": "$KEY_ID" } ] }
JSON
export BUWIZ_TRUST_FILE="$TMP/trust.json"
"$BIN" manifest --key-id "$KEY_ID" >"$TMP/manifest.json"
APP_ARGS=()
for id in $(node -e 'for (const a of JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).apps) if (!a.native) console.log(a.id)' "$TMP/manifest.json"); do
  "$BIN" app-source "$id" >"$TMP/app-$id.tsx"
  APP_ARGS+=(--app "$id=$TMP/app-$id.tsx")
done
"${SIGN[@]}" "$TMP/manifest.json" --key-file "$TMP/test.key" ${APP_ARGS[@]+"${APP_ARGS[@]}"} --out "$TMP/signed.json"
# not `exec`: the trap has to run, or the throwaway key stays on disk
node --disable-warning=ExperimentalWarning "$RUNNER" --command "$BIN" --args mcp \
  --manifest "$TMP/signed.json" --key-file "$TMP/test.key" --manifest-env BUWIZ_PLUGIN_MANIFEST \
  --env "GWS_DATA_DIR=$TMP/data" "$@"
