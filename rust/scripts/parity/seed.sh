#!/usr/bin/env bash
# Seeds the Rails reference app (compose.rust-dev.yml) for the API parity checks.
# Prints PARITY_TOKEN and PARITY_TEMPLATE_ID; `eval "$(rust/scripts/parity/seed.sh)"`.
set -euo pipefail

root="$(cd "$(dirname "$0")/../../.." && pwd)"
compose=(docker compose -f "$root/compose.rust-dev.yml")
container="$("${compose[@]}" ps -q rails)"

docker cp "$root/rust/scripts/parity/seed.rb" "$container:/tmp/parity_seed.rb" >/dev/null
docker cp "$root/spec/fixtures/sample-document.pdf" "$container:/tmp/sample-document.pdf" >/dev/null
docker cp "$root/spec/fixtures/sample-image.png" "$container:/tmp/sample-image.png" >/dev/null
"${compose[@]}" exec -T -w /app rails bin/rails runner /tmp/parity_seed.rb /tmp/sample-document.pdf /tmp/sample-image.png \
  | grep '^PARITY_'
