#!/usr/bin/env bash
# tools/bot/live.sh: the plugin on a real test Bot, against the stub Google.
#
#   tools/bot/live.sh up        build, sign with a throwaway key, start the stub,
#                               a test broker and a test Bot (AeroSpace workspace 3),
#                               install the package, sign in
#   tools/bot/live.sh down      stop them
#   tools/bot/live.sh host <METHOD> <path> [json]   call the test broker's host API
#   tools/bot/live.sh agent <gpui-agent args…>      drive the test Bot as the person
#
# Env: BUWIZ_BOT_DIR  a Bot worktree holding the build (default ../buwiz-bot)
#      GWS_INST       where everything lives (default ../inst)
#      GWS_PORT       the test broker's port (default 8821; never 8787)
#      GWS_STUB_PORT  the stub's (default 8822); GWS_AGENT_PORT gpui-agent's (default 17481)
#      GWS_MAILS      how many fake mails (default 200)
# Everything is fake: a throwaway key, never gc-2026-01; a file keystore, never
# the Keychain; the stub Google, never a real account.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BOT="${BUWIZ_BOT_DIR:-$REPO/../buwiz-bot}"
INST="${GWS_INST:-$REPO/../inst}"
PORT="${GWS_PORT:-8821}"; STUB_PORT="${GWS_STUB_PORT:-8822}"; AGENT_PORT="${GWS_AGENT_PORT:-17481}"
KEY_ID=gws-test-01
PLUGIN=app.buwiz.google-workspace
[ "$PORT" != 8787 ] || { echo "live: 8787 is the owner's broker" >&2; exit 2; }
mkdir -p "$INST"; INST="$(cd "$INST" && pwd)"; BOT="$(cd "$BOT" && pwd)"
target() { (cd "$1" && cargo metadata --no-deps --format-version 1 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])'); }

host() {
  local method="$1" path="$2" body="${3:-}"
  local args=(-s -X "$method" -H "X-Buwiz-Host: $(cat "$INST/broker/host-token")" -H 'content-type: application/json')
  [ -z "$body" ] || args+=(--data "$body")
  curl "${args[@]}" "http://127.0.0.1:$PORT$path"
}

keys() {
  [ -f "$INST/test.key" ] && return
  umask 077
  node -e '
    const c=require("crypto"),fs=require("fs");const {publicKey,privateKey}=c.generateKeyPairSync("ed25519");
    const j=privateKey.export({format:"jwk"});fs.writeFileSync(process.argv[1],j.d+"\n");fs.writeFileSync(process.argv[2],j.x+"\n");' "$INST/test.key" "$INST/test.pub"
  python3 - "$INST" "$KEY_ID" "$PLUGIN" <<'PY'
import json,sys
inst,key,plugin=sys.argv[1:]
pub=open(f"{inst}/test.pub").read().strip()
json.dump({"v":1,"about":"Throwaway trust for a test Bot; never gc-2026-01.",
  "publishers":{key:{"publisher":"app.buwiz","alg":"ed25519","public_key":pub,"first_party":True}},
  "approved_plugins":[{"id":plugin,"publisher_key_id":key}]},open(f"{inst}/trust.json","w"))
PY
}

package() {
  local bin broker pkg="$INST/pkg" out="$INST/pkg-out"
  (cd "$REPO" && CARGO_INCREMENTAL=0 cargo build -p buwiz-gws --features stub -p gws-fake-google >&2)
  bin="$(target "$REPO")/debug/buwiz-gws"; broker="$(target "$BOT/broker")/debug/buwiz-broker"
  rm -rf "$pkg" "$out"; mkdir -p "$pkg/bin" "$out"
  "$bin" manifest --key-id "$KEY_ID" > "$out/manifest.json"
  "$broker" sign-manifest "$out/manifest.json" --key-file "$INST/test.key" --out "$out/manifest.signed.json" >&2
  cp "$bin" "$out/buwiz-gws-macos-aarch64"
  python3 - "$REPO/package/plugin.json" "$out/plugin.template.json" "$KEY_ID" <<'PY'
import json,sys
src,dst,key=sys.argv[1:]
p=json.load(open(src)); p["buwiz"]["publisher"]["key_id"]=key; json.dump(p,open(dst,"w"))
PY
  python3 "$REPO/tools/release/plugin-json.py" "$out/plugin.template.json" "$out" macos-aarch64 buwiz-gws-macos-aarch64 >&2
  "$broker" sign-manifest "$out/plugin.unsigned.json" --block buwiz --key-file "$INST/test.key" --out "$pkg/plugin.json" >&2
  cp "$out/manifest.signed.json" "$pkg/"; cp "$bin" "$pkg/bin/buwiz-gws"; chmod 755 "$pkg/bin/buwiz-gws"
}

up() {
  keys; package
  local broker app stub
  broker="$(target "$BOT/broker")/debug/buwiz-broker"; app="$(target "$BOT/app")/debug/buwiz-bot"; stub="$(target "$REPO")/debug/gws-fake-google"
  nohup "$stub" --port "$STUB_PORT" --mails "${GWS_MAILS:-200}" > "$INST/stub.log" 2>&1 & echo $! > "$INST/stub.pid"
  mkdir -p "$INST/broker" "$INST/gws-data"
  # no real model: the mock answers, every model URL is a dead loopback port,
  # and no key rides in from this shell. Unset, the broker reaches the local
  # opencodex proxy on 8080, which holds the owner's credentials.
  env -u GPUI_AGENT -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u OPENROUTER_API_KEY -u OPENAI_API_KEY -u XAI_API_KEY \
    MOCK=1 OPENCODEX_URL=http://127.0.0.1:1 ANTHROPIC_BASE_URL=http://127.0.0.1:1 GEN_BASE_URL=http://127.0.0.1:1 \
    BUWIZ_BROKER_HUB=1 BUWIZ_BROKER_V1_HOST=1 BUWIZ_BROKER_MCP_HOST=1 BUWIZ_BROKER_DATA_DIR="$INST/broker" \
    BUWIZ_TRUST_FILE="$INST/trust.json" BUWIZ_KEYSTORE="file:$INST/keys" BUWIZ_TEST_GOOGLE="http://127.0.0.1:$STUB_PORT" \
    nohup "$broker" --port "$PORT" --data "$INST/broker" --no-node > "$INST/broker.log" 2>&1 & echo $! > "$INST/broker.pid"
  for _ in $(seq 1 100); do [ -s "$INST/broker/host-token" ] && curl -s "http://127.0.0.1:$PORT/health" >/dev/null && break; sleep 0.1; done
  install
  signin
  "$BOT/scripts/buwiz-test" --name gws "$app" "$INST" "$AGENT_PORT" "BUWIZ_BACKEND=http://127.0.0.1:$PORT" "BUWIZ_HOST_TOKEN_FILE=$INST/broker/host-token"
}

# The reviewed hash from an inspect, then the install, as the Bot's card does it.
install() {
  local body sha
  body="$(python3 - "$INST" "$STUB_PORT" <<'PY'
import json,sys
inst,stub=sys.argv[1:]
print(json.dumps({"package":f"{inst}/pkg","env":{"GWS_STUB_BASE":f"http://127.0.0.1:{stub}","GWS_DATA_DIR":f"{inst}/gws-data"}}))
PY
)"
  sha="$(host POST /v1/host/plugins/inspect "$body" | python3 -c 'import json,sys; a=json.load(sys.stdin); print(a.get("manifest_sha256") or a.get("review",{}).get("manifest_sha256") or sys.exit("inspect: "+json.dumps(a)[:400]))')"
  host POST /v1/host/plugins/install "$(python3 -c 'import json,sys; b=json.loads(sys.argv[1]); b["manifest_sha256"]=sys.argv[2]; b["approve_permissions"]=True; print(json.dumps(b))' "$body" "$sha")" | cut -c1-200; echo
}

# The fake client in Setup, then a sign-in the stub says yes to at once.
signin() {
  host POST "/v1/host/plugins/$PLUGIN/setup" '{"values":{"GOOGLE_CLIENT_ID":"123456-fake.apps.googleusercontent.com","GOOGLE_CLIENT_SECRET":"GOCSPX-fake"}}' | cut -c1-200; echo
  host POST "/v1/host/plugins/$PLUGIN/accounts/google/signin" '{"label":"default"}' | cut -c1-160; echo
}

# gpui-agent's gestures activate the app, which takes the keyboard from
# whatever the owner is typing in, and can move the visible workspace. So each
# one gives both straight back. A read asks nothing of the app and needs neither.
agent() {
  case "${1:-}" in snapshot|hello|wait|assert|events) "$BOT/scripts/test-agent" "$INST" "$@"; return ;; esac
  local window space status=0
  window="$(aerospace list-windows --focused --format '%{window-id}' 2>/dev/null || true)"
  space="$(aerospace list-workspaces --focused 2>/dev/null || true)"
  "$BOT/scripts/test-agent" "$INST" "$@" || status=$?
  [ -z "$space" ] || aerospace workspace "$space" 2>/dev/null || true
  [ -z "$window" ] || aerospace focus --window-id "$window" 2>/dev/null || true
  return "$status"
}

down() {
  for f in pid broker.pid stub.pid; do [ -f "$INST/$f" ] && kill "$(cat "$INST/$f")" 2>/dev/null || true; rm -f "$INST/$f"; done
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  package) keys; package ;;
  signin) signin ;;
  host) shift; host "$@"; echo ;;
  agent) shift; agent "$@" ;;
  *) sed -n 2,16p "$0"; exit 2 ;;
esac
