#!/usr/bin/env bash
# Writes rust/data/locales/rails.yml: translations that Devise and rails-i18n add to the
# Rails app, taken from the Rails reference container (compose.rust-dev.yml).
set -euo pipefail

cd "$(dirname "$0")/../.."

docker compose -f compose.rust-dev.yml exec -T -w /app rails bin/rails runner - \
  < rust/scripts/golden/rails_locales.rb > rust/data/locales/rails.yml

echo "Wrote rust/data/locales/rails.yml"
