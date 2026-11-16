#!/usr/bin/env bash
# tools/check-all.sh: every gate that needs no Google account and no Bot: fmt,
# clippy, tests, then the SDK's conformance runner (tools/conformance.sh, which
# needs BUWIZ_SDK_DIR). SKIP_CONFORMANCE=1 skips conformance, and the run then
# says it did not pass. tools/bot/live.sh (a test Bot) is not part of it.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

step() { printf '\n== %s\n' "$*"; }

step "cargo fmt --check"
cargo fmt --all --check
step "cargo clippy"
cargo clippy --workspace --locked --all-targets --quiet -- -D warnings
step "cargo test"
cargo test --workspace --locked --quiet
step "cargo test with the stub"
cargo test -p buwiz-gws --features stub --locked --quiet

step "SDK doc links follow the pin"
PIN="$(sed -n 's/^buwiz-plugin = .*rev = "\([0-9a-f]\{40\}\)".*/\1/p' crates/gws-plugin/Cargo.toml)"
[ -n "$PIN" ] || { echo "no pinned buwiz-sdk revision in crates/gws-plugin/Cargo.toml" >&2; exit 1; }
STALE="$(grep -rhoE 'buwiz-sdk/blob/[^/)]+|checkout [0-9a-f]{40}' README.md | grep -vE "blob/$PIN\$|checkout $PIN\$" || true)"
[ -z "$STALE" ] || { printf 'SDK links name a revision other than the pinned %s:\n%s\n' "$PIN" "$STALE" >&2; exit 1; }

if [ "${SKIP_CONFORMANCE:-0}" = 1 ]; then
  printf '\nconformance SKIPPED: not every gate ran\n'
  exit 1
fi
step "conformance (the SDK's buwiz-plugin-test)"
tools/conformance.sh

printf '\nall gates passed\n'
