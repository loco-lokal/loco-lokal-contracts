#!/usr/bin/env bash
# Regenerates golden fixtures from the Rails reference container (compose.rust-dev.yml).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RUN=(docker compose -f "$ROOT/compose.rust-dev.yml" exec -T -w /app rails bin/rails runner -)

# The container's entrypoint may print to stdout before the JSON starts.
"${RUN[@]}" < "$ROOT/rust/scripts/golden/rails_compat.rb" 2>/dev/null | sed -n '/^{/,$p' \
  > "$ROOT/rust/crates/rails-compat/tests/fixtures/golden.json"
echo "Wrote rust/crates/rails-compat/tests/fixtures/golden.json"

# The form fields and links of the upload fixtures, as Rails extracts them.
CONTAINER="$(docker compose -f "$ROOT/compose.rust-dev.yml" ps -q rails)"
docker cp "$ROOT/rust/scripts/parity/uploads_seed.rb" "$CONTAINER:/tmp/uploads_seed.rb" >/dev/null
docker cp "$ROOT/rust/scripts/golden/acro_fields.rb" "$CONTAINER:/tmp/acro_fields.rb" >/dev/null
docker compose -f "$ROOT/compose.rust-dev.yml" exec -T -w /app rails \
    bin/rails runner /tmp/acro_fields.rb /tmp/uploads_seed.rb 2>/dev/null | sed -n '/^{/,$p' \
  | python3 -c '
import base64, json, sys
from pathlib import Path
out = Path(sys.argv[1])
golden = json.load(sys.stdin)
for name, entry in golden.items():
    (out / name).write_bytes(base64.b64decode(entry.pop("pdf")))
(out / "upload-golden.json").write_text(json.dumps(golden, indent=1, ensure_ascii=False) + "\n")
' "$ROOT/rust/crates/pdf/tests/fixtures"
echo "Wrote rust/crates/pdf/tests/fixtures/upload-*.pdf and upload-golden.json"
