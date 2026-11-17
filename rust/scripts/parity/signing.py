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

A second, single-party submission is then completed so that the submission completes and
the result documents are signed.

Responses are compared after normalizing tokens, slugs, uuids and times, and so are
the rows the two backends stored (submitter state, events, completed submitters, result
documents). The result PDFs cannot match byte for byte (fonts are subset differently and
the signatures differ), so each page is rendered for both backends and the images are
compared (`pdftoppm` from poppler), and every signature must verify (`openssl cms`).
"""

import base64
import http.cookiejar
import tempfile
import io
import json
import os
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

    class Policy(http.cookiejar.DefaultCookiePolicy):
        """Sends `secure` cookies over http too, as a browser would over https."""

        def return_ok_secure(self, cookie, request):
            return True

    def __init__(self, base):
        self.base = base
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar(self.Policy())), self.NoRedirect
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
           result_documents: e.documents.map {{ |d| [d.filename.to_s, d.content_type, d.metadata.keys.sort, d.metadata['original_uuid']] }},
           completed_documents: CompletedDocument.where(submitter_id: e.id).count }}
      end
      audit = s.audit_trail.attached? && [s.audit_trail.filename.to_s, s.audit_trail.content_type, s.audit_trail.metadata]
      puts({{ completed: s.completed_at.present?, audit_trail: audit, submitters: out }}.to_json)
    """
    result = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", script],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    return json.dumps(json.loads(result.stdout.strip().splitlines()[-1]), indent=1)


def result_pdfs(submission_id):
    """Each submitter's result PDFs, read through Rails."""
    script = f"""
      s = Submission.find({submission_id})
      puts(s.submitters.order(:id).map {{ |e| e.documents.map {{ |d| Base64.strict_encode64(d.download) }} }}.to_json)
    """
    result = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", script],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    return [[base64.b64decode(d) for d in docs] for docs in json.loads(result.stdout.strip().splitlines()[-1])]


def audit_pdf(submission_id):
    """The submission's audit trail PDF, read through Rails (None without one)."""
    script = f"""
      s = Submission.find({submission_id})
      puts('AUDIT:' + (s.audit_trail.attached? ? Base64.strict_encode64(s.audit_trail.download) : ''))
    """
    result = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", script],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    data = next(line[6:] for line in result.stdout.splitlines() if line.startswith("AUDIT:"))
    return base64.b64decode(data) if data else None


def render(pdf):
    from PIL import Image

    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "doc.pdf"
        path.write_bytes(pdf)
        subprocess.run(["pdftoppm", "-r", "60", "-png", str(path), str(Path(tmp) / "page")], check=True)
        return [Image.open(p).convert("L").copy() for p in sorted(Path(tmp).glob("page*.png"))]


def der_element(data):
    """The first DER element of `data`, without the zero padding after it (a signature's
    own bytes may end in zeros, so the padding cannot just be stripped)."""
    length, header = data[1], 2
    if length & 0x80:
        count = length & 0x7F
        length, header = int.from_bytes(data[2:2 + count], "big"), 2 + count
    return data[:header + length]


def signatures(pdf):
    """The number of signatures, how many verify, and whether the one covering the whole file
    verifies. pdfsig's NSS cannot read the DocuSeal certificate chain and reports even Rails'
    signatures as invalid, so each signature is checked with `openssl cms -verify` over its
    byte ranges (chain trust not checked). A merged image PDF also carries the signatures of
    the PDFs it was merged from, which do not cover it."""
    found = valid = 0
    whole = False
    for match in re.finditer(rb"/ByteRange\s*\[\s*(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s*\]", pdf):
        a, b, c, d = map(int, match.groups())
        if b == 1000000000000:  # an unfilled placeholder
            continue
        found += 1
        contents = pdf[a + b:c].strip().strip(b"<>")
        if c + d > len(pdf) or not re.fullmatch(rb"[0-9a-fA-F]*", contents):
            continue  # byte ranges of another file
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "data").write_bytes(pdf[a:a + b] + pdf[c:c + d])
            Path(tmp, "sig").write_bytes(der_element(bytes.fromhex(contents.decode())))
            result = subprocess.run(
                ["openssl", "cms", "-verify", "-binary", "-noverify", "-inform", "DER",
                 "-in", str(Path(tmp, "sig")), "-content", str(Path(tmp, "data")), "-out", os.devnull],
                capture_output=True,
            )
        valid += result.returncode == 0
        whole = whole or (result.returncode == 0 and c + d == len(pdf))
    return found, valid, whole


def compare_pdfs(rails_id, rust_id, signed=False):
    """Renders both backends' result PDFs and compares their words (per-run tokens such as
    times may differ), metadata and signatures. With `signed`, each document of the last
    party (whose results are signed) must carry a signature."""
    global FAILURES
    pairs = list(zip(result_pdfs(rails_id), result_pdfs(rust_id)))
    for index, (a_docs, b_docs) in enumerate(pairs):
        for doc, (a, b) in enumerate(zip(a_docs, b_docs)):
            label = f"result pdf {'signed ' if signed else ''}{index}.{doc}"
            pages_a, pages_b = render(a), render(b)
            if len(pages_a) != len(pages_b):
                FAILURES += 1
                print(f"FAIL {label}: {len(pages_a)} pages vs {len(pages_b)}")
                continue
            worst = 0.0
            for x, y in zip(pages_a, pages_b):
                if x.size != y.size:
                    worst = 1.0
                    break
                from PIL import ImageChops

                diff = ImageChops.difference(x, y)
                changed = sum(1 for v in diff.getdata() if v > 64)
                worst = max(worst, changed / (x.size[0] * x.size[1]))
            sig_a, sig_b = signatures(a), signatures(b)
            same_info = pdf_info(a) == pdf_info(b)
            text_a, text_b = ([w[0] for w in words(pdf)] for pdf in (a, b))
            same_text = len(text_a) == len(text_b) and all(
                x == y or (VARIABLE.search(x) and VARIABLE.search(y)) for x, y in zip(text_a, text_b)
            )
            ok = (worst < 0.002 and sig_a == sig_b and (sig_b[0] == 0 or sig_b[2]) and same_info and same_text
                  and (sig_b[0] > 0 or not signed or index < len(pairs) - 1))
            if not ok:
                FAILURES += 1
            print(f"{'ok  ' if ok else 'FAIL'} {label} [pixels differing {worst:.4%}, signatures rails {sig_a} rust {sig_b}]"
                  + ("" if same_info else f"\n     metadata {pdf_info(a)} vs {pdf_info(b)}")
                  + ("" if same_text else f"\n     text {text_a} vs {text_b}"))


def words(pdf):
    """Each word with its box (points from the top left) and page index, from
    `pdftotext -bbox`."""
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "doc.pdf"
        path.write_bytes(pdf)
        out = subprocess.run(["pdftotext", "-bbox", str(path), "-"], capture_output=True, check=True).stdout
    pattern = r'<word xMin="([\d.]+)" yMin="([\d.]+)" xMax="([\d.]+)" yMax="([\d.]+)">([^<]*)</word>'
    return [(m[4], tuple(float(v) for v in m[:4]), page)
            for page, text in enumerate(out.decode().split("<page ")[1:])
            for m in re.findall(pattern, text)]


# Text that differs between two runs: ids, checksums, addresses and times.
VARIABLE = re.compile(r"\d|^[A-Za-z0-9_=-]{20,}$")


def pdf_text(token):
    """A literal or hexadecimal PDF string token as text (UTF-16BE with a byte order mark,
    else Latin-1), so that both serializations of the same string compare equal."""
    if token.startswith(b"<"):
        data = bytes.fromhex(re.sub(rb"\s", b"", token[1:-1]).decode())
    else:
        escapes = {b"n": b"\n", b"r": b"\r", b"t": b"\t", b"b": b"\b", b"f": b"\f"}
        data = re.sub(rb"\\([0-7]{1,3}|.)",
                      lambda m: bytes([int(m[1], 8) & 255]) if m[1][:1].isdigit() else escapes.get(m[1], m[1]),
                      token[1:-1], flags=re.S)
    if data.startswith(b"\xfe\xff"):
        return data[2:].decode("utf-16-be", "replace")
    return data.decode("latin1")


def pdf_info(pdf):
    """The info dictionary's and the XMP packet's entries, with dates and packet ids masked
    (how many distinct times an incremental file carries depends on timing)."""
    keys = rb"Creator|Producer|ModDate|CreationDate|Title|Author|Subject|Keywords|M"
    pattern = rb"/(" + keys + rb")\s*(\((?:\\.|[^\\)])*\)|<[0-9A-Fa-f\s]*>)"
    entries = [(k, pdf_text(v)) for k, v in re.findall(pattern, pdf, re.S)]
    packets = re.findall(rb"<x:xmpmeta.*?</x:xmpmeta>", pdf, re.S)
    masked = b"\n".join(re.sub(rb"\d{4}-\d\d-\d\dT[\d:]+[+-]\d\d:\d\d", b"<time>", x) for x in packets)
    intents = re.findall(rb"/S\s*/GTS_PDFA1|/OutputConditionIdentifier\s*\([^)]*\)|/RegistryName\s*\([^)]*\)", pdf)
    entries = sorted({(k.decode(), re.sub(r"\d{14}", "<date>", v)) for k, v in entries})
    return entries, masked.decode(), sorted(set(intents))


def compare_audit(rails_id, rust_id, label):
    """The audit trails of two submissions: word positions (per-run tokens such as ids may
    differ in text, not in position), the page renders, the metadata and the signature."""
    global FAILURES
    a, b = audit_pdf(rails_id), audit_pdf(rust_id)
    if a is None and b is None:
        print(f"ok   {label} [none]")
        return
    if a is None or b is None:
        FAILURES += 1
        print(f"FAIL {label}: audit trail rails {a is not None} rust {b is not None}")
        return
    problems = []
    wa, wb = words(a), words(b)
    if len(wa) != len(wb):
        problems.append(f"{len(wa)} words vs {len(wb)}")
    for (ta, ba, _), (tb, bb, _) in zip(wa, wb):
        same = ta == tb or (VARIABLE.search(ta) and VARIABLE.search(tb))
        box = (0, 1, 2, 3) if ta == tb else (0, 1, 3)
        if not same or any(abs(ba[i] - bb[i]) > 0.01 for i in box):
            problems.append(f"{ta!r} {ba} vs {tb!r} {bb}")
            break
    pages_a, pages_b = render(a), render(b)
    worst = 0.0 if len(pages_a) == len(pages_b) else 1.0
    from PIL import ImageChops, ImageDraw

    # The words that may differ (times, ids) were compared above; blank them on both renders
    # (60 dpi) so that a different minute on every event line does not count as pixels.
    for (ta, ba, page), (tb, bb, _) in zip(wa, wb):
        if ta != tb and page < min(len(pages_a), len(pages_b)):
            box = [min(ba[0], bb[0]) * 60 / 72 - 1, min(ba[1], bb[1]) * 60 / 72 - 1,
                   max(ba[2], bb[2]) * 60 / 72 + 1, max(ba[3], bb[3]) * 60 / 72 + 1]
            for image in (pages_a[page], pages_b[page]):
                ImageDraw.Draw(image).rectangle(box, fill=255)

    for x, y in zip(pages_a, pages_b):
        changed = sum(1 for v in ImageChops.difference(x, y).getdata() if v > 64) if x.size == y.size else 1e9
        worst = max(worst, changed / (x.size[0] * x.size[1]))
    if worst > 0.002:
        problems.append(f"pixels differing {worst:.4%}")
    if pdf_info(a) != pdf_info(b):
        problems.append(f"metadata {pdf_info(a)} vs {pdf_info(b)}")
    sig_a, sig_b = signatures(a), signatures(b)
    if sig_a != sig_b or sig_b[0] != sig_b[1] or (sig_b[0] and not sig_b[2]):
        problems.append(f"signatures rails {sig_a} rust {sig_b}")
    if problems:
        FAILURES += 1
    print(f"{'FAIL' if problems else 'ok  '} {label} [{len(pages_b)} pages, pixels differing {worst:.4%}, "
          f"signatures {sig_b}]" + "".join(f"\n     {p}" for p in problems))


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


# Right-to-left party names and values (Hebrew, Arabic, mixed with Latin and digits),
# which Rails draws in visual order (`TextUtils.maybe_rtl_reverse`).
RTL = {"first": "דוד כהן", "second": "سارة أحمد", "full_name": "דוד כהן (מנהל) 2026",
       "approver": "محمد أحمد 123 - Approved"}


def run_signed(base, token, template_id, fields, rtl=False):
    """A submission both parties complete, so its result documents and audit trail are
    signed. Repeated form views make the event log long enough to continue on a second
    page of the audit trail. With `rtl`, the names and text values are right-to-left."""
    status, created = call(
        RAILS, token, "POST", "/api/submissions",
        {"template_id": template_id, "send_email": False,
         "submitters": [{"role": "First Party", "email": "ana@example.test",
                         "name": RTL["first"] if rtl else "Ana Example"},
                        {"role": "Second Party", "email": "bob@example.test", "phone": "+4915112345678",
                         **({"name": RTL["second"]} if rtl else {})}]},
    )
    assert status == 200, created
    values = {
        "First Party": [(fields["Full Name"], RTL["full_name"] if rtl else "Ana Example"),
                        (fields["Start Date"], "2026-03-14"), (fields["Plan"], "Pro")],
        "Second Party": [(fields["Approver"], RTL["approver"] if rtl else "Bob Approver")],
    }
    signature = {"First Party": "Signature", "Second Party": "Countersignature"}
    for submitter in created:
        signer = Browser(base)
        s = submitter["slug"]
        page = signer.request("GET", f"/s/{s}")[1]
        view_token = re.search(r'data-view-token="([^"]+)"', page).group(1)
        for _ in range(20 if submitter["role"] == "First Party" else 1):
            signer.request("POST", f"/s/{s}/view", [("v", view_token)])
        upload = signer.request(
            "POST", f"/s/{s}/upload", [("type", "signature")],
            {"file": ("signature.png", "image/png", signature_png())},
        )
        status = signer.request(
            "POST", f"/s/{s}",
            [("_method", "put"), ("authenticity_token", signer.csrf),
             *((f"values[{uuid}]", value) for uuid, value in values[submitter["role"]]),
             (f"values[{fields[signature[submitter['role']]]}]", json.loads(upload[1])["uuid"]),
             ("completed", "true"), ("timezone", "Europe/Berlin")],
            files={},
        )
        assert status[0] == 200, f"{base}: completing {submitter['role']} returned {status[0]}"
    return created[0]["submission_id"]


def run_images(base, token, template_id):
    """A single-party submission of the image template: each image becomes a result PDF, and
    Rails also merges them into one PDF (all signed, as the submission completes)."""
    _, template = call(RAILS, token, "GET", f"/api/templates/{template_id}")
    fields = {f["name"]: f["uuid"] for f in template["fields"]}
    status, created = call(
        RAILS, token, "POST", "/api/submissions",
        {"template_id": template_id, "send_email": False,
         "submitters": [{"role": "Signer", "email": "ana@example.test", "name": "Ana Example"}]},
    )
    assert status == 200, created
    signer = Browser(base)
    s = created[0]["slug"]
    signer.request("GET", f"/s/{s}")
    upload = signer.request(
        "POST", f"/s/{s}/upload", [("type", "signature")],
        {"file": ("signature.png", "image/png", signature_png())},
    )
    status = signer.request(
        "POST", f"/s/{s}",
        [("_method", "put"), ("authenticity_token", signer.csrf),
         (f"values[{fields['Full Name']}]", "Ana Example"),
         (f"values[{fields['Signature']}]", json.loads(upload[1])["uuid"]),
         ("completed", "true"), ("timezone", "Europe/Berlin")],
        files={},
    )
    assert status[0] == 200, f"{base}: completing the image submission returned {status[0]}"
    return created[0]["submission_id"]


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
    signed_ids = (run_signed(RAILS, token, template_id, fields),
                  run_signed(RUST, token, template_id, fields))
    rtl_ids = (run_signed(RAILS, token, template_id, fields, rtl=True),
               run_signed(RUST, token, template_id, fields, rtl=True))
    _, found = call(RAILS, token, "GET", "/api/templates?external_id=parity-images")
    images_template_id = found["data"][0]["id"]
    image_ids = (run_images(RAILS, token, images_template_id), run_images(RUST, token, images_template_id))
    for label in rails:
        is_page = rails[label][1].lstrip().startswith("<")
        check(label, rails[label], rust[label], page if is_page else json_text)

    time.sleep(5)  # the completion jobs
    a, b = stored(rails_id), stored(rust_id)
    global FAILURES
    if a == b:
        print("ok   stored rows")
    else:
        FAILURES += 1
        print("FAIL stored rows")
        print(diff(a, b))
    compare_pdfs(rails_id, rust_id)
    a, b = (stored(i) for i in signed_ids)
    if a == b:
        print("ok   stored rows (signed)")
    else:
        FAILURES += 1
        print("FAIL stored rows (signed)")
        print(diff(a, b))
    compare_pdfs(*signed_ids, signed=True)
    a, b = (stored(i) for i in image_ids)
    if a == b:
        print("ok   stored rows (images)")
    else:
        FAILURES += 1
        print("FAIL stored rows (images)")
        print(diff(a, b))
    compare_pdfs(*image_ids, signed=True)
    compare_audit(rails_id, rust_id, "audit trail")
    compare_audit(*signed_ids, "audit trail (signed)")
    compare_audit(*image_ids, "audit trail (images)")
    a, b = (stored(i) for i in rtl_ids)
    if a == b:
        print("ok   stored rows (right-to-left)")
    else:
        FAILURES += 1
        print("FAIL stored rows (right-to-left)")
        print(diff(a, b))
    compare_pdfs(*rtl_ids, signed=True)
    compare_audit(*rtl_ids, "audit trail (right-to-left)")
    if os.environ.get("KEEP"):
        print("kept", rails_id, rust_id, *signed_ids, *image_ids, *rtl_ids)
        sys.exit(1 if FAILURES else 0)
    for submission_id in (rails_id, rust_id, *signed_ids, *image_ids, *rtl_ids):
        call(RAILS, token, "DELETE", f"/api/submissions/{submission_id}")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
