#!/usr/bin/env python3
"""Compares API responses of the Rails reference app and the Rust server.

    rust/scripts/parity/compare.py TOKEN GET /api/templates GET /api/submissions/1 ...

Each request runs against both servers (RAILS_URL, default http://127.0.0.1:3000, and
RUST_URL, default http://127.0.0.1:8080). Signed file tokens in URLs are normalized,
since they embed an expiry time. Status, key order and values must match.
"""

import json
import os
import re
import sys
import urllib.error
import urllib.request

RAILS = os.environ.get("RAILS_URL", "http://127.0.0.1:3000")
RUST = os.environ.get("RUST_URL", "http://127.0.0.1:8080")
SIGNED = re.compile(r"/file/[A-Za-z0-9_=-]+--[0-9a-f]+/")


def fetch(base, method, path, token, body):
    data = body.encode() if body else None
    request = urllib.request.Request(base + path, data=data, method=method)
    request.add_header("X-Auth-Token", token)
    if data:
        request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request) as response:
            return response.status, response.read().decode()
    except urllib.error.HTTPError as err:
        return err.code, err.read().decode()


def normalize(text):
    text = SIGNED.sub("/file/SIGNED/", text)
    try:
        return json.dumps(json.loads(text), indent=1, ensure_ascii=False)
    except ValueError:
        return text


def diff(a, b):
    a_lines, b_lines = a.splitlines(), b.splitlines()
    for i, (x, y) in enumerate(zip(a_lines, b_lines)):
        if x != y:
            start = max(0, i - 3)
            return "\n".join(
                [f"  line {i + 1}:"]
                + [f"    rails: {line}" for line in a_lines[start : i + 2]]
                + [f"    rust:  {line}" for line in b_lines[start : i + 2]]
            )
    return f"  length differs: rails {len(a_lines)} lines, rust {len(b_lines)} lines"


def main():
    token, args = sys.argv[1], sys.argv[2:]
    failures = 0
    while args:
        method, path = args[0], args[1]
        args = args[2:]
        body = None
        if args and args[0].startswith("{"):
            body, args = args[0], args[1:]
        rails = fetch(RAILS, method, path, token, body)
        rust = fetch(RUST, method, path, token, body)
        a, b = normalize(rails[1]), normalize(rust[1])
        if rails[0] == rust[0] and a == b:
            print(f"ok   {method} {path} [{rails[0]}]")
        else:
            failures += 1
            print(f"FAIL {method} {path} [rails {rails[0]}, rust {rust[0]}]")
            print(diff(a, b))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
