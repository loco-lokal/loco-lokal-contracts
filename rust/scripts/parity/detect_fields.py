#!/usr/bin/env python3
"""Compares field detection (`POST /templates/:id/detect_fields`) of the Rails reference
app and the Rust server.

    rust/scripts/parity/detect_fields.py

`detect_fields_seed.rb` (run in the `rails` service) creates a user with a known password
in the first user's account and a template with form-like PDFs and images (see the seed).
Signed in as that user on each backend, the script posts the requests the template
builder sends (JSON bodies with `attachment_uuid` and `page`), and some it doesn't, and
compares the event streams: status, content type, and every event with the field
coordinates as written (Rails' float digits), only the random field uuids normalized.
That covers the whole template (every page of every document), single pages, image
documents (one with an integer shrink before the model), errors in the middle of the
stream, and the authorization answers.

The unported branch (an image document whose preview is a grayscale PNG) is checked on
the Rust server only: it must answer 422.
"""

import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

from compare import RAILS, RUST, diff
from previews import Session, rails_runner, seed as _seed  # noqa: F401

FAILURES = 0
UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


def report(ok, label, detail=None):
    global FAILURES
    if ok:
        print(f"ok   {label}")
    else:
        FAILURES += 1
        print(f"FAIL {label}")
        if detail:
            print(detail)


def seed():
    from pathlib import Path
    import subprocess

    root = Path(__file__).resolve().parents[3]
    script = Path(__file__).with_name("detect_fields_seed.rb")
    subprocess.run(["docker", "compose", "-f", "compose.rust-dev.yml", "cp", str(script),
                    "rails:/tmp/detect_fields_seed.rb"], cwd=root, check=True)
    return json.loads(rails_runner("load '/tmp/detect_fields_seed.rb'"))


def post(session, path, body=None, form=None, query=""):
    if form is not None:
        data = urllib.parse.urlencode(form).encode()
        content_type = "application/x-www-form-urlencoded"
    else:
        data = json.dumps(body if body is not None else {}).encode()
        content_type = "application/json"
    request = urllib.request.Request(session.base + path + query, data=data, method="POST")
    request.add_header("Content-Type", content_type)
    request.add_header("User-Agent", "parity-detect-fields")
    if session.csrf:
        request.add_header("X-CSRF-Token", session.csrf)
    started = time.time()
    try:
        with session.opener.open(request) as response:
            result = response.status, response.headers, response.read().decode()
    except urllib.error.HTTPError as err:
        result = err.code, err.headers, err.read().decode()
    return result + (time.time() - started,)


def events(body):
    """The `data:` events of a stream, with the random field uuids normalized."""
    out = []
    for chunk in body.split("\n\n"):
        if not chunk:
            continue
        if not chunk.startswith("data: "):
            out.append({"raw": chunk})
            continue
        out.append(UUID.sub(lambda m: m.group(0) if m.group(0) in KNOWN else "UUID", chunk[6:]))
    return out


KNOWN = set()


def location(headers):
    value = headers.get("Location")
    return value and re.sub(r"https?://[^/]+", "ORIGIN", value)


def compare(label, sessions, path, **kwargs):
    results = {name: post(s, path, **kwargs) for name, s in sessions.items()}
    (s1, h1, b1, t1), (s2, h2, b2, t2) = results["rails"], results["rust"]
    head1 = (s1, h1.get("Content-Type"), h1.get("Cache-Control"), location(h1))
    head2 = (s2, h2.get("Content-Type"), h2.get("Cache-Control"), location(h2))
    if "event-stream" in (h1.get("Content-Type") or ""):
        a, b = events(b1), events(b2)
    else:
        a, b = b1, b2
    ok = head1 == head2 and a == b
    detail = None
    if not ok:
        detail = f"  rails {head1}\n  rust  {head2}"
        if a != b:
            detail += "\n" + diff(json.dumps(a, indent=1), json.dumps(b, indent=1))
    pages = sum(1 for e in a if isinstance(e, str) and '"fields"' in e)
    fields = sum(e.count('"areas"') for e in a if isinstance(e, str))
    report(ok, f"{label} [{s1}; {pages} pages, {fields} fields; {t1:.1f}s / {t2:.1f}s]", detail)
    return results


def main():
    data = seed()
    docs = data["documents"]
    KNOWN.update(docs.values())
    template = data["template_id"]
    path = f"/templates/{template}/detect_fields"
    sessions = {"rails": Session(RAILS, data["email"], data["password"]),
                "rust": Session(RUST, data["email"], data["password"])}
    anonymous = {"rails": Session(RAILS), "rust": Session(RUST)}
    for s in anonymous.values():
        s.request("GET", "/sign_in")

    form, letter = docs["form"], docs["letter"]
    image, tall = docs["form-image"], docs["tall-image"]

    # What the builder sends: the whole template, then pages and documents one by one.
    compare("all documents (schema)", sessions, path, body={})
    compare("all documents (builder list)", sessions, path,
            body={"attachment_uuid": [form, letter, image, tall], "fields": []})
    for page in range(6):
        compare(f"form page {page}", sessions, path, body={"attachment_uuid": form, "page": page})
    compare("letter", sessions, path, body={"attachment_uuid": letter})
    compare("form image", sessions, path, body={"attachment_uuid": image, "page": 0})
    compare("tall image", sessions, path, body={"attachment_uuid": tall})

    # Parameters as Rails reads them.
    compare("page as a string in the query", sessions, path, query=f"?attachment_uuid={form}&page=3")
    compare("form-encoded body", sessions, path, form={"attachment_uuid[]": letter, "page": "0"})
    compare("float page", sessions, path, body={"attachment_uuid": form, "page": 1.9})
    compare("blank page", sessions, path, body={"attachment_uuid": letter, "page": ""})
    compare("image document, page 1", sessions, path, body={"attachment_uuid": image, "page": 1})
    compare("unknown attachment", sessions, path, body={"attachment_uuid": "nope"})
    compare("empty attachment list", sessions, path, body={"attachment_uuid": [], "page": 0})

    # Errors end the stream where they happen.
    compare("page out of range", sessions, path, body={"attachment_uuid": form, "page": 99})
    compare("negative page", sessions, path, body={"attachment_uuid": letter, "page": -1})
    compare("error after a page", sessions, path, body={"attachment_uuid": [letter, form], "page": 5})
    compare("page: true", sessions, path, body={"page": True})

    # Authorization.
    compare("missing template", sessions, "/templates/999999999/detect_fields", body={})
    other = data.get("other_template_id")
    if other:
        compare("other account's template", sessions, f"/templates/{other}/detect_fields", body={})
    compare("signed out", anonymous, path, body={})

    unsupported = data.get("unsupported")
    if unsupported:
        status, headers, body, _ = post(sessions["rust"], f"/templates/{unsupported['template_id']}/detect_fields",
                                        body={})
        expected = "Detecting fields in image documents with grayscale or 16-bit previews is not supported yet"
        report(status == 422 and json.loads(body or "{}").get("error") == expected,
               f"grayscale preview: Rust answers 422 [{status}]", body[:200])

    print(f"\n{FAILURES} failure(s)")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
