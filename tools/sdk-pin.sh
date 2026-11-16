#!/usr/bin/env bash
# tools/sdk-pin.sh: print the buwiz-sdk revision this repo pins, the one place
# that reads it. Every buwiz-sdk git revision named in any Cargo.toml of the
# workspace and in Cargo.lock must be the same one; otherwise this lists them
# and fails.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
SDK_URL='github.com/buwiz-app/buwiz-sdk'
FOUND="$(
  {
    # Cargo.toml: a dependency on the SDK must carry a full revision
    find . -name Cargo.toml -not -path './target/*' -not -path './.git/*' | sort | while read -r f; do
      grep -F "$SDK_URL" "$f" | while read -r line; do
        rev="$(printf '%s\n' "$line" | sed -n 's/.*rev *= *"\([0-9a-f]\{40\}\)".*/\1/p')"
        printf '%s %s\n' "${rev:-NONE}" "${f#./}"
      done
    done
    # Cargo.lock: the requested rev and the resolved commit
    if [ -f Cargo.lock ]; then
      grep -F "git+https://$SDK_URL" Cargo.lock | sed -n 's/.*?rev=\([0-9a-f]\{40\}\)#\([0-9a-f]\{40\}\).*/\1 Cargo.lock\n\2 Cargo.lock/p'
    fi
  } | sort -u
)"
[ -n "$FOUND" ] || { echo "sdk-pin: no buwiz-sdk revision found in any Cargo.toml" >&2; exit 1; }
REVS="$(printf '%s\n' "$FOUND" | cut -d' ' -f1 | sort -u)"
if [ "$(printf '%s\n' "$REVS" | wc -l)" -ne 1 ] || [ "$REVS" = NONE ]; then
  echo "sdk-pin: the buwiz-sdk pins disagree (or one has no full revision):" >&2
  printf '%s\n' "$FOUND" | sed 's/^/  /' >&2
  exit 1
fi
echo "$REVS"
