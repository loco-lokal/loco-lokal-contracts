#!/usr/bin/env python3
"""Compares the write endpoints of the Rails reference app and the Rust server.

    rust/scripts/parity/writes.py TOKEN TEMPLATE_ID

Each backend works on its own copy: a clone of the template, and submissions created
through the Rails API. Ids, slugs, uuids and timestamps are normalized away; status,
key order and the remaining values must match. The copies are deleted at the end.
"""

import json
import re
import sys

from compare import RAILS, RUST, SIGNED, diff, fetch

UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
TIME = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?(Z|[+-]\d\d:\d\d)?")
SLUG = re.compile(r"/(s|d|e)/[A-Za-z0-9]+")
VOLATILE = {"id", "slug", "submission_id", "template_id", "folder_id", "attachment_uuid", "created_by_user_id"}


def scrub(value, key=None):
    if isinstance(value, dict):
        return {k: scrub(v, k) for k, v in value.items()}
    if isinstance(value, list):
        return [scrub(v) for v in value]
    if key in VOLATILE and value is not None:
        return "<volatile>"
    if isinstance(value, str):
        value = SIGNED.sub("/file/SIGNED/", value)
        value = UUID.sub("<uuid>", value)
        value = TIME.sub("<time>", value)
        return SLUG.sub(r"/\1/<slug>", value)
    return value


def call(base, token, method, path, body=None):
    status, text = fetch(base, method, path, token, json.dumps(body) if body else None)
    try:
        return status, json.loads(text)
    except ValueError:
        return status, text


FAILURES = 0


def check(label, rails, rust):
    global FAILURES
    a = json.dumps(scrub(rails[1]), indent=1, ensure_ascii=False)
    b = json.dumps(scrub(rust[1]), indent=1, ensure_ascii=False)
    if rails[0] == rust[0] and a == b:
        print(f"ok   {label} [{rails[0]}]")
    else:
        FAILURES += 1
        print(f"FAIL {label} [rails {rails[0]}, rust {rust[0]}]")
        print(diff(a, b))


def both(token, label, method, path_for, body=None):
    """Runs the request on each backend with its own record id."""
    rails = call(RAILS, token, method, path_for("rails"), body)
    rust = call(RUST, token, method, path_for("rust"), body)
    check(label, rails, rust)
    return rails, rust


def main():
    token, template_id = sys.argv[1], sys.argv[2]

    # Templates: clone, update, archive, permanent delete.
    clone = {"name": "Parity Clone", "external_id": None}
    rails, rust = both(token, "POST clone", "POST", lambda _: f"/api/templates/{template_id}/clone", clone)
    ids = {"rails": rails[1]["id"], "rust": rust[1]["id"]}
    path = lambda b: f"/api/templates/{ids[b]}"
    both(token, "GET clone", "GET", path)
    both(token, "PUT template", "PUT", path,
         {"name": "Parity Renamed", "folder_name": "Parity / Writes",
          "roles": ["Signer", "Witness"], "shared_link": True})
    both(token, "GET updated", "GET", path)
    both(token, "PUT bad folder", "PUT", path, {"folder_name": ""})
    both(token, "DELETE archive", "DELETE", path)
    both(token, "GET archived", "GET", path)
    both(token, "PUT unarchive", "PUT", path, {"archived": False})
    both(token, "DELETE permanently", "DELETE", lambda b: path(b) + "?permanently=true")
    both(token, "GET deleted", "GET", path)

    # Submissions: each backend gets one created through Rails.
    new = {"template_id": int(template_id), "send_email": False,
           "submitters": [{"role": "First Party", "email": "writes@example.com"}]}
    subs = {}
    for b in ("rails", "rust"):
        status, created = call(RAILS, token, "POST", "/api/submissions", new)
        assert status == 200, (status, created)
        subs[b] = created[0]["submission_id"]
    spath = lambda b: f"/api/submissions/{subs[b]}"
    both(token, "PUT submission", "PUT", spath,
         {"name": "Parity Writes", "expire_at": "2030-01-02 10:00:00"})
    both(token, "PUT bad expire_at", "PUT", spath, {"expire_at": "nope"})
    both(token, "DELETE submission archive", "DELETE", spath)
    both(token, "GET archived submission", "GET", spath)
    both(token, "DELETE submission permanently", "DELETE", lambda b: spath(b) + "?permanently=true")
    both(token, "GET deleted submission", "GET", spath)

    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
