#!/usr/bin/env python3
"""Compares the result download endpoints of the Rails reference app and the Rust server.

    rust/scripts/parity/downloads.py TOKEN TEMPLATE_ID

For each backend a two-party submission is created and its first party completed
through the Rails API, so both backends serve documents Rails generated. Then:

- `/s/:slug/download` (partial download) before and after the first party completed,
  and its redirect once that party has completed;
- `/s/:slug/documents` and `/submitters/:slug/download`, before and after completion;
- `/submitters/:id/download` without a session (the sign-in redirect).

The returned links are compared with their signatures masked. Each backend's links must
serve a PDF from both servers, with the same bytes (the submissions differ, so the
files of the two backends are not compared with each other).
"""

import json
import sys
import time
import urllib.error
import urllib.request

from compare import RAILS, RUST
from signing import Browser, check, json_text
from writes import call

FAILURES = 0


def fetch_bytes(base, path):
    try:
        with urllib.request.urlopen(base + path) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()


def files(body):
    """For each linked file: whether both servers serve the same PDF."""
    try:
        paths = json.loads(body)
    except ValueError:
        return None
    if not isinstance(paths, list):
        return None
    out = []
    for path in paths:
        (s1, d1), (s2, d2) = fetch_bytes(RAILS, path), fetch_bytes(RUST, path)
        out.append(s1 == s2 == 200 and d1 == d2 and d1.startswith(b"%PDF"))
    return out


def setup(token, template_id):
    status, created = call(RAILS, token, "POST", "/api/submissions", {
        "template_id": template_id, "send_email": False, "order": "random",
        "submitters": [{"role": "First Party", "email": "dl-first@example.com"},
                       {"role": "Second Party", "email": "dl-second@example.com"}],
    })
    assert status == 200, (status, created)
    return created


def complete(token, submitter_id):
    status, body = call(RAILS, token, "PUT", f"/api/submitters/{submitter_id}", {"completed": True})
    assert status == 200, (status, body)


def main():
    global FAILURES
    token, template_id = sys.argv[1], int(sys.argv[2])
    subs = {"rails": setup(token, template_id), "rust": setup(token, template_id)}
    bases = {"rails": RAILS, "rust": RUST}

    def both(label, path_for, compare_files=True):
        global FAILURES
        results = {}
        for b in ("rails", "rust"):
            results[b] = Browser(bases[b]).request("GET", path_for(subs[b]))
        before = FAILURES
        check(label, results["rails"], results["rust"], json_text)
        if compare_files:
            a = files(results["rails"][1])
            r = files(results["rust"][1])
            if a != r:
                FAILURES += 1
                print(f"FAIL {label} files: rails {a}, rust {r}")
        return FAILURES == before

    first = lambda s: s[0]["slug"]
    second = lambda s: s[1]["slug"]

    both("partial download (none completed)", lambda s: f"/s/{first(s)}/download")
    both("documents (not completed)", lambda s: f"/s/{first(s)}/documents")
    both("submitters download by slug (not completed)", lambda s: f"/submitters/{first(s)}/download")
    both("submitters download by id (signed out)", lambda s: f"/submitters/{s[0]['id']}/download", False)
    both("unknown slug", lambda s: "/s/nope/download", False)

    for b in ("rails", "rust"):
        complete(token, subs[b][0]["id"])
    time.sleep(8)  # Rails' completion job generates the result documents

    both("partial download (first completed)", lambda s: f"/s/{second(s)}/download")
    both("partial download redirect", lambda s: f"/s/{first(s)}/download", False)
    both("documents", lambda s: f"/s/{first(s)}/documents")
    both("submitters download by slug", lambda s: f"/submitters/{first(s)}/download")
    both("documents (second, not completed)", lambda s: f"/s/{second(s)}/documents")
    both("documents combined", lambda s: f"/s/{first(s)}/documents?combined=true", False)

    for b in ("rails", "rust"):
        call(RAILS, token, "DELETE", f"/api/submissions/{subs[b][0]['submission_id']}")
    import signing
    sys.exit(1 if FAILURES or signing.FAILURES else 0)


if __name__ == "__main__":
    main()
