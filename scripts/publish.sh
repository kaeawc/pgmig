#!/usr/bin/env bash
# Publish every publishable workspace crate whose current version is not yet on crates.io.
# Safe to rerun after a partial failure (rate limit, network): crates already uploaded are skipped.
#   scripts/publish.sh            # publish what is missing
#   scripts/publish.sh --dry-run  # package + verify what is missing, upload nothing
set -euo pipefail
cd "$(dirname "$0")/.."

# crates.io sparse index path for a crate name (https://doc.rust-lang.org/cargo/reference/registry-index.html).
index_path() {
  local n="$1"
  case ${#n} in
    1) echo "1/$n" ;;
    2) echo "2/$n" ;;
    3) echo "3/${n:0:1}/$n" ;;
    *) echo "${n:0:2}/${n:2:2}/$n" ;;
  esac
}

# Prints "name version" for each workspace crate that is publishable (publish != []).
crates="$(cargo metadata --no-deps --format-version 1 --locked | python3 -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    if p["publish"] is None or p["publish"]:
        print(p["name"], p["version"])
')"

args=()
while read -r name version; do
  if curl -fsS "https://index.crates.io/$(index_path "$name")" 2>/dev/null \
      | python3 -c 'import json,sys; v=sys.argv[1]; sys.exit(0 if any(json.loads(l)["vers"]==v for l in sys.stdin if l.strip()) else 1)' "$version"; then
    echo "skip    $name $version (already on crates.io)"
  else
    echo "publish $name $version"
    args+=(-p "$name")
  fi
done <<< "$crates"

if [ ${#args[@]} -eq 0 ]; then
  echo "nothing to publish"
  exit 0
fi
# With several -p, cargo publishes in dependency order and waits for each to reach the index.
cargo publish --locked "${args[@]}" "$@"
