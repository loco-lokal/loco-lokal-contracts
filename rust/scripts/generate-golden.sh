#!/usr/bin/env bash
# Regenerates golden fixtures from the Rails reference container (compose.rust-dev.yml).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RUN=(docker compose -f "$ROOT/compose.rust-dev.yml" exec -T -w /app rails bin/rails runner -)

# The container's entrypoint may print to stdout before the JSON starts.
"${RUN[@]}" < "$ROOT/rust/scripts/golden/rails_compat.rb" 2>/dev/null | sed -n '/^{/,$p' \
  > "$ROOT/rust/crates/rails-compat/tests/fixtures/golden.json"
echo "Wrote rust/crates/rails-compat/tests/fixtures/golden.json"
