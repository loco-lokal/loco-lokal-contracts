#!/usr/bin/env python3
"""Compares the signing form (`/s/:slug`) of the Rails reference app and the Rust server.

    rust/scripts/parity/signing.py TOKEN TEMPLATE_ID

Run the Rust server with job workers (JOB_CONCURRENCY=2 tmp/run-rust.sh) so the
completion job runs. For each backend this script creates a two-party submission
through the Rails API and then, as the signers would:

1. opens the form, records the view, reads values and uploads a signature;
2. saves a draft, fails to complete with a required field missing, and completes;
3. opens the completed page; and
4. declines as the second party and opens the declined page.

Responses are compared after normalizing tokens, slugs, uuids and times, and so are
the rows the two backends stored (submitter state, events, completed submitters).
Result documents are reported as "todo" until the Rust completion job generates them.
"""

import http.cookiejar
import io
import json
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid as uuidlib
from pathlib import Path

from compare import RAILS, RUST, diff
from writes import call, scrub

ROOT = Path(__file__).resolve().parents[3]
FAILURES = 0


def signature_png():
    from PIL import Image, ImageDraw

    image = Image.new("RGBA", (400, 120), (255, 255, 255, 0))
    draw = ImageDraw.Draw(image)
    draw.line([(20, 90), (120, 30), (200, 95), (300, 25), (380, 80)], fill=(0, 0, 0, 255), width=6)
    out = io.BytesIO()
    image.save(out, "PNG")
    return out.getvalue()


class Browser:
    """A signer's browser: cookies, the page's CSRF token, and no redirect following."""

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):
            return None

    def __init__(self, base):
        self.base = base
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()), self.NoRedirect
        )
        self.csrf = None

    def request(self, method, path, fields=None, files=None, headers=None):
        data, content_type = None, None
        if files is not None:
            boundary = uuidlib.uuid4().hex
            parts = []
            for name, value in fields or []:
                parts.append(
                    f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
                )
            for name, (filename, mime, blob) in files.items():
                parts.append(
                    f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
                    f"Content-Type: {mime}\r\n\r\n".encode()
                    + blob
                    + b"\r\n"
                )
            parts.append(f"--{boundary}--\r\n".encode())
            data, content_type = b"".join(parts), f"multipart/form-data; boundary={boundary}"
        elif fields is not None:
            data = urllib.parse.urlencode(fields).encode()
            content_type = "application/x-www-form-urlencoded"
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if content_type:
            request.add_header("Content-Type", content_type)
        request.add_header("User-Agent", "parity-signing")
        for key, value in (headers or {}).items():
            request.add_header(key, value)
        try:
            with self.opener.open(request) as response:
                status, body, location = response.status, response.read().decode(), response.headers.get("Location")
        except urllib.error.HTTPError as err:
            status, body, location = err.code, err.read().decode(), err.headers.get("Location")
        match = re.search(r'name="csrf-token" content="([^"]+)"', body)
        if match:
            self.csrf = match.group(1)
        return status, body, location


def page(text):
    """An HTML page with per-request values normalized away."""
    text = re.sub(r'(csrf-token" content="|authenticity_token" value=")[^"]+', r"\1TOKEN", text)
    text = re.sub(r'data-view-token="[^"]+', 'data-view-token="VT', text)
    text = re.sub(r'nonce="[^"]*"', 'nonce="N"', text)
    text = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "UUID", text)
    text = re.sub(r"https?://(localhost|127\.0\.0\.1):\d+", "ORIGIN", text)
    text = re.sub(r"/disk/[^/\"&]+/", "/disk/T/", text)
    text = re.sub(r"/(s|d)/[A-Za-z0-9]{14}\b", r"/\1/SLUG", text)
    text = re.sub(r'value="[A-Za-z0-9]{14}"', 'value="SLUG"', text)
    text = re.sub(r"(&quot;slug&quot;:&quot;)[A-Za-z0-9]+", r"\1SLUG", text)
    text = re.sub(r'<script src="/packs[^\n]*|<link rel="stylesheet" href="/packs[^\n]*', "PACK", text)
    lines = (line.strip() for line in text.split("\n"))
    return "\n".join(line for line in lines if line)


def check(label, rails, rust, normalize=lambda x: x):
    global FAILURES
    a, b = normalize(rails[1]), normalize(rust[1])
    same_location = (rails[2] is None) == (rust[2] is None) and (
        rails[2] is None or page(rails[2]) == page(rust[2])
    )
    if rails[0] == rust[0] and a == b and same_location:
        print(f"ok   {label} [{rails[0]}]")
    else:
        FAILURES += 1
        print(f"FAIL {label} [rails {rails[0]} {rails[2]}, rust {rust[0]} {rust[2]}]")
        if a != b:
            print(diff(a, b))


def json_text(text):
    try:
        text = json.dumps(scrub(json.loads(text)), indent=1, ensure_ascii=False)
    except ValueError:
        pass
    text = re.sub(r"https?://(localhost|127\.0\.0\.1):\d+", "ORIGIN", text)
    return re.sub(r"/disk/[^/\"&]+/", "/disk/T/", text)


def stored(submission_id):
    """The rows a backend stored for a submission, read through Rails."""
    script = f"""
      s = Submission.find({submission_id})
      out = s.submitters.order(:id).map do |e|
        {{ values: e.values.transform_values {{ |v| v.is_a?(String) && v.match?(/\\A\\h{{8}}-/) ? '<uuid>' : v }},
           opened: e.opened_at.present?, completed: e.completed_at.present?, declined: e.declined_at.present?,
           ip: e.ip.present?, ua: e.ua, timezone: e.timezone,
           attachments: e.attachments.map {{ |a| [a.name, a.blob.content_type, a.blob.metadata.except('sha256')] }},
           events: e.submission_events.order(:id).map {{ |ev| [ev.event_type, ev.data.keys.sort] }},
           completed_submitters: CompletedSubmitter.where(submitter_id: e.id).count,
           result_documents: [e.documents.count, CompletedDocument.where(submitter_id: e.id).count] }}
      end
      puts({{ completed: s.completed_at.present?, submitters: out }}.to_json)
    """
    result = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", script],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    return json.dumps(json.loads(result.stdout.strip().splitlines()[-1]), indent=1)


def run(base, token, template_id, fields):
    status, created = call(
        RAILS, token, "POST", "/api/submissions",
        {"template_id": template_id, "send_email": False,
         "submitters": [{"role": "First Party", "email": "ana@example.test", "name": "Ana Example"},
                        {"role": "Second Party", "email": "bob@example.test"}]},
    )
    assert status == 200, created
    first, second = created[0], created[1]
    steps = {}
    signer = Browser(base)
    s = first["slug"]
    steps["show"] = signer.request("GET", f"/s/{s}")
    view_token = re.search(r'data-view-token="([^"]+)"', steps["show"][1]).group(1)
    csrf = {"authenticity_token": signer.csrf}
    steps["view"] = signer.request("POST", f"/s/{s}/view", [("v", view_token)])
    steps["view forged"] = signer.request("POST", f"/s/{s}/view", [("v", "forged")])
    steps["values"] = signer.request("GET", f"/s/{s}/values?field_uuid={fields['Full Name']}")
    steps["upload blank"] = signer.request(
        "POST", f"/s/{s}/upload", [("type", "signature")],
        {"file": ("signature.png", "image/png", blank_png())},
    )
    steps["upload"] = signer.request(
        "POST", f"/s/{s}/upload", [("type", "signature")],
        {"file": ("signature.png", "image/png", signature_png())},
    )
    signature_uuid = json.loads(steps["upload"][1])["uuid"]
    form = lambda *pairs: [("_method", "put"), ("authenticity_token", signer.csrf), *pairs]
    steps["save draft"] = signer.request(
        "POST", f"/s/{s}", form((f"values[{fields['Full Name']}]", "Ana Example")), files={}
    )
    steps["complete missing"] = signer.request(
        "POST", f"/s/{s}", form((f"values[{fields['Full Name']}]", "Ana Example"), ("completed", "true")), files={}
    )
    steps["complete"] = signer.request(
        "POST", f"/s/{s}",
        form((f"values[{fields['Full Name']}]", "Ana Example"), (f"values[{fields['Signature']}]", signature_uuid),
             ("completed", "true"), ("timezone", "Europe/Berlin")),
        files={},
    )
    steps["complete again"] = signer.request(
        "POST", f"/s/{s}", form((f"values[{fields['Full Name']}]", "Ana"), ("completed", "true")), files={}
    )
    steps["show completed"] = signer.request("GET", f"/s/{s}")
    steps["completed page"] = signer.request("GET", f"/s/{s}/completed")

    decliner = Browser(base)
    s2 = second["slug"]
    steps["second show"] = decliner.request("GET", f"/s/{s2}")
    steps["decline"] = decliner.request(
        "POST", f"/s/{s2}/decline", [("authenticity_token", decliner.csrf), ("reason", "Not today")]
    )
    steps["declined page"] = decliner.request("GET", f"/s/{s2}")
    steps["missing"] = decliner.request("GET", "/s/NoSuchSlug1234")
    return first["submission_id"], steps


def blank_png():
    from PIL import Image

    out = io.BytesIO()
    Image.new("RGBA", (400, 120), (255, 255, 255, 0)).save(out, "PNG")
    return out.getvalue()


def main():
    token, template_id = sys.argv[1], int(sys.argv[2])
    _, template = call(RAILS, token, "GET", f"/api/templates/{template_id}")
    fields = {f["name"]: f["uuid"] for f in template["fields"]}
    rails_id, rails = run(RAILS, token, template_id, fields)
    rust_id, rust = run(RUST, token, template_id, fields)
    for label in rails:
        is_page = rails[label][1].lstrip().startswith("<")
        check(label, rails[label], rust[label], page if is_page else json_text)

    time.sleep(5)  # the completion jobs
    a, b = json.loads(stored(rails_id)), json.loads(stored(rust_id))
    # Result PDFs (and their completed_documents rows) come with Phase 4b.
    gaps = [(x.pop("result_documents"), y.pop("result_documents")) for x, y in zip(a["submitters"], b["submitters"])]
    a, b = json.dumps(a, indent=1), json.dumps(b, indent=1)
    global FAILURES
    if a == b:
        print("ok   stored rows")
    else:
        FAILURES += 1
        print("FAIL stored rows")
        print(diff(a, b))
    if any(x != y for x, y in gaps):
        print(f"todo result documents [rails, rust] per submitter: {gaps} (Phase 4b)")
    for submission_id in (rails_id, rust_id):
        call(RAILS, token, "DELETE", f"/api/submissions/{submission_id}")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
