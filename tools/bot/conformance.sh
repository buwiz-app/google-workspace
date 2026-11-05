#!/usr/bin/env bash
# tools/bot/conformance.sh: the Bot's plugin conformance runner against this
# plugin's binary, under a throwaway key and a per-run trust file. Nothing here
# touches the Keychain, a Bot's data or gc-2026-01.
#
# Env: BUWIZ_BOT_DIR (default ../buwiz-bot): a Bot checkout with server/node_modules
#      BUWIZ_BROKER_BIN: a buwiz-broker build, to sign (else the Bot's sign-manifest.mjs)
#      KEEP_TMP=1 keeps the temp dir (it holds a test key)
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BOT_DIR="${BUWIZ_BOT_DIR:-$REPO/../buwiz-bot}"
KEY_ID=gws-test-01
PLUGIN_ID=app.buwiz.google-workspace
RUNNER="$BOT_DIR/server/sdk/mini-app/bin/buwiz-plugin-test.mjs"
[ -f "$RUNNER" ] || { echo "conformance: $RUNNER not found (set BUWIZ_BOT_DIR)" >&2; exit 2; }
[ -d "$BOT_DIR/server/node_modules/@modelcontextprotocol/sdk" ] || { echo "conformance: $BOT_DIR/server/node_modules is missing" >&2; exit 2; }
if [ -n "${BUWIZ_BROKER_BIN:-}" ]; then SIGN=("$BUWIZ_BROKER_BIN" sign-manifest); else SIGN=(node "$BOT_DIR/server/src/v1/sign-manifest.mjs"); fi
TMP="$(mktemp -d "${TMPDIR:-/tmp}/gws-conformance.XXXXXX")"; chmod 700 "$TMP"
cleanup() { if [ "${KEEP_TMP:-0}" = 1 ]; then echo "kept $TMP" >&2; else rm -rf "$TMP"; fi; }
trap cleanup EXIT

(cd "$REPO" && CARGO_INCREMENTAL=0 cargo build --quiet -p buwiz-gws)
BIN="$(cd "$REPO" && cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/buwiz-gws"

node "$RUNNER" --keygen "$TMP/test.key" >"$TMP/keygen.txt"
PUB="$(sed -n 's/.*Public key: \([A-Za-z0-9_-]*\).*/\1/p' "$TMP/keygen.txt")"
[ -n "$PUB" ] || { echo "conformance: keygen printed no public key" >&2; exit 1; }
cat >"$TMP/trust.json" <<JSON
{ "v": 1, "about": "Per-run trust for conformance; never gc-2026-01.",
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
exec node --disable-warning=ExperimentalWarning "$RUNNER" --command "$BIN" --args mcp \
  --manifest "$TMP/signed.json" --key-file "$TMP/test.key" --manifest-env BUWIZ_PLUGIN_MANIFEST \
  --env "GWS_DATA_DIR=$TMP/data" "$@"
