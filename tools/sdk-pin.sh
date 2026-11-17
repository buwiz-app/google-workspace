#!/usr/bin/env bash
# tools/sdk-pin.sh: print the buwiz-sdk revision this repo pins. The one place
# that reads it.
#
# It asks Cargo, not the text of a manifest: `cargo metadata --locked` fails
# when Cargo.lock no longer matches the manifests, and its answer names the
# revision every buwiz-sdk crate was resolved at, however the dependency is
# written. There must be exactly one, named in full.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
META="$(cargo metadata --locked --format-version 1 --manifest-path "$REPO/Cargo.toml" 2>&1)" || {
  echo "sdk-pin: cargo could not read the workspace with its lockfile as it is:" >&2
  printf '%s\n' "$META" | tail -5 | sed 's/^/  /' >&2
  exit 1
}
# each buwiz-sdk crate's source: ...buwiz-sdk?rev=<asked for>#<resolved>
FOUND="$(printf '%s' "$META" | grep -oE 'git\+https://github\.com/buwiz-app/buwiz-sdk[^"]*#[0-9a-f]{40}' | sort -u || true)"
[ -n "$FOUND" ] || { echo "sdk-pin: this workspace depends on no buwiz-sdk crate by git revision" >&2; exit 1; }
REVS="$(printf '%s\n' "$FOUND" | sed -E 's/.*#([0-9a-f]{40})$/\1/' | sort -u)"
ASKED="$(printf '%s\n' "$FOUND" | sed -nE 's/.*[?&]rev=([0-9a-f]+)#.*/\1/p' | sort -u)"
if [ "$(printf '%s\n' "$REVS" | wc -l | tr -d ' ')" != 1 ] || [ "$ASKED" != "$REVS" ]; then
  echo "sdk-pin: every buwiz-sdk crate must be pinned to one revision, written in full (rev = \"<40 hex>\"); found:" >&2
  printf '%s\n' "$FOUND" | sed 's/^/  /' >&2
  exit 1
fi
printf '%s\n' "$REVS"
