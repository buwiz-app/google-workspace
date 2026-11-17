#!/usr/bin/env bash
# tools/sdk-pin.sh: print the buwiz-sdk revision this repo pins. The one place
# that reads it.
#
# It asks Cargo, not the text of a manifest: `cargo metadata --locked` fails
# when Cargo.lock no longer matches the manifests, and its answer names the
# revision every buwiz-sdk crate was resolved at, however the dependency is
# written. Every buwiz-sdk crate must come from one revision, asked for in
# full (rev = "<40 hex>"): a branch, a tag or a short revision is refused.
# Only the root Cargo.toml may name the SDK's repository, so a manifest
# outside the workspace cannot carry a pin of its own.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || { echo "sdk-pin: cargo is not on PATH" >&2; exit 1; }
ERR="$(mktemp)"
trap 'rm -f "$ERR"' EXIT
META="$(cargo metadata --locked --format-version 1 --manifest-path "$REPO/Cargo.toml" 2>"$ERR")" || {
  echo "sdk-pin: cargo could not read the workspace with its lockfile as it is:" >&2
  tail -5 "$ERR" | sed 's/^/  /' >&2
  exit 1
}
# each buwiz-sdk crate's source, whatever the URL's scheme: git+...buwiz-app/buwiz-sdk...#<resolved>
FOUND="$(printf '%s' "$META" | grep -oiE 'git\+[^"]*github\.com[:/]buwiz-app/buwiz-sdk[^"]*#[0-9a-f]{40}' | sort -u || true)"
[ -n "$FOUND" ] || { echo "sdk-pin: this workspace depends on no buwiz-sdk crate by git revision" >&2; exit 1; }
REVS="$(printf '%s\n' "$FOUND" | sed -E 's/.*#([0-9a-f]{40})$/\1/' | sort -u)"
# a source is good when it asks, by rev, for the full revision it resolved to
BAD="$(printf '%s\n' "$FOUND" | awk '{
  resolved = $0; sub(/.*#/, "", resolved)
  asked = ""
  if (match($0, /[?&]rev=[0-9a-f]+/)) { asked = substr($0, RSTART, RLENGTH); sub(/^[?&]rev=/, "", asked) }
  if (asked != resolved) print
}')"
if [ "$(printf '%s\n' "$REVS" | wc -l | tr -d ' ')" != 1 ] || [ -n "$BAD" ]; then
  echo "sdk-pin: every buwiz-sdk crate must be pinned to one revision, written in full (rev = \"<40 hex>\"); found:" >&2
  printf '%s\n' "$FOUND" | sed 's/^/  /' >&2
  exit 1
fi
STRAY="$(cd "$REPO" && find . -name Cargo.toml -not -path ./Cargo.toml -not -path '*/target/*' -not -path '*/node_modules/*' -exec grep -liE 'buwiz-app/buwiz-sdk' {} + 2>/dev/null || true)"
if [ -n "$STRAY" ]; then
  echo "sdk-pin: only the root Cargo.toml may name the buwiz-sdk repository (members inherit it with workspace = true); also named in:" >&2
  printf '%s\n' "$STRAY" | sed 's/^/  /' >&2
  exit 1
fi
printf '%s\n' "$REVS"
