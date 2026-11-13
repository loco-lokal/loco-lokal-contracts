#!/usr/bin/env bash
# Seeds the Rails reference app (compose.rust-dev.yml) for the API parity checks.
# Prints PARITY_TOKEN and PARITY_TEMPLATE_ID; `eval "$(rust/scripts/parity/seed.sh)"`.
# `seed.sh combined` seeds the separate account combined.py uses.
set -euo pipefail

root="$(cd "$(dirname "$0")/../../.." && pwd)"
compose=(docker compose -f "$root/compose.rust-dev.yml")
container="$("${compose[@]}" ps -q rails)"

docker cp "$root/rust/scripts/parity/seed.rb" "$container:/tmp/parity_seed.rb" >/dev/null
docker cp "$root/spec/fixtures/sample-document.pdf" "$container:/tmp/sample-document.pdf" >/dev/null
"${compose[@]}" exec -T -w /app rails bin/rails runner /tmp/parity_seed.rb /tmp/sample-document.pdf "$@" \
  | grep '^PARITY_'
