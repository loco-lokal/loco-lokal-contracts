#!/usr/bin/env bash
# Builds the Shakapacker JS/CSS bundles into public/packs without Ruby, so the Rust server
# can serve them. Pass `production` for minified, fingerprinted assets.
set -euo pipefail

cd "$(dirname "$0")/../.."

env_name="${1:-development}"
[ -d node_modules ] || yarn install --frozen-lockfile

RAILS_ENV="$env_name" NODE_ENV="$env_name" \
  ./node_modules/.bin/webpack --config config/webpack/webpack.config.js

echo "Built public/packs/manifest.json ($env_name)"
