#!/usr/bin/env bash
# Writes rust/data/bidi_classes.txt: the bidi class of each code point according to
# twitter_cldr, and the code points TextUtils::RTL_REGEXP matches, taken from the Rails
# reference container (compose.rust-dev.yml), and the Rails output of
# TextUtils.maybe_rtl_reverse for sample texts that the Rust port is tested against.
set -euo pipefail

cd "$(dirname "$0")/../.."

docker compose -f compose.rust-dev.yml exec -T -w /app rails bin/rails runner - \
  < rust/scripts/golden/bidi_classes.rb | sed -n '/^# Generated/,$p' > rust/data/bidi_classes.txt

docker compose -f compose.rust-dev.yml exec -T -w /app rails bin/rails runner - \
  < rust/scripts/golden/rtl_text.rb | sed -n '/^\[/,$p' > rust/crates/domain/tests/fixtures/rtl_golden.json

echo "Wrote rust/data/bidi_classes.txt and rust/crates/domain/tests/fixtures/rtl_golden.json"
