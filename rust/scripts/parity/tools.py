#!/usr/bin/env python3
"""Compares `POST /api/tools/merge` and `POST /api/tools/verify` of Rails and the Rust server.

    rust/scripts/parity/tools.py TOKEN TEMPLATE_ID

Merging: the errors, and two files merged; the merged PDFs differ byte for byte (the
backends use different PDFium builds), so their page counts and rendered pages are
compared. Verifying: an unsigned document, result documents and audit trails that each
backend signed (submissions completed through the Rails API, so their checksums are
known), a tampered copy, a copy with unsigned data appended, and a malformed file.
"""

import base64
import json
import os
import subprocess
import sys
import time
from pathlib import Path

from compare import RAILS, RUST
from signing import render
from writes import call, check

ROOT = Path(__file__).resolve().parents[3]
DB = os.environ.get("DATABASE_URL", "postgres://docuseal:docuseal_dev@127.0.0.1:5435/docuseal")
FAILURES = 0


def both(token, label, path, body, normalize=lambda x: x):
    rails = call(RAILS, token, "POST", path, body)
    rust = call(RUST, token, "POST", path, body)
    check(label, (rails[0], normalize(rails[1])), (rust[0], normalize(rust[1])))
    if path.endswith("/verify") and rails != rust:
        # `check` masks times; the signing times must match exactly too.
        global FAILURES
        FAILURES += 1
        print(f"FAIL {label}: raw responses differ\n  rails {rails[1]}\n  rust  {rust[1]}")
    return rails, rust


def compare_pdfs(label, a, b):
    global FAILURES
    from PIL import ImageChops

    pages_a, pages_b = render(a), render(b)
    worst = 0.0 if len(pages_a) == len(pages_b) else 1.0
    for x, y in zip(pages_a, pages_b):
        if x.size != y.size:
            worst = 1.0
            continue
        changed = sum(1 for v in ImageChops.difference(x, y).get_flattened_data() if v > 64)
        worst = max(worst, changed / (x.size[0] * x.size[1]))
    ok = worst < 0.002
    FAILURES += not ok
    print(f"{'ok  ' if ok else 'FAIL'} {label} [{len(pages_a)} vs {len(pages_b)} pages, pixels differing {worst:.4%}]")


def rust_only(token, label, path, body, expected):
    global FAILURES
    status, result = call(RUST, token, "POST", path, body)
    ok = status == 422 and expected in str(result)
    FAILURES += not ok
    print(f"{'ok  ' if ok else 'FAIL'} {label} (Rust only) [{status}] {result}")


def signed_documents(token, template_id, base):
    """The result documents and audit trail of a submission both parties completed."""
    status, created = call(base, token, "POST", "/api/submissions", {
        "template_id": template_id, "send_email": False,
        "submitters": [{"role": "First Party", "email": "tool1@example.com", "completed": True,
                        "values": {"Full Name": "Tool One"}},
                       {"role": "Second Party", "email": "tool2@example.com", "completed": True,
                        "values": {"Approver": "Tool Two"}}]})
    assert status == 200, created
    submission_id = created[0]["submission_id"]
    for _ in range(30):
        time.sleep(2)
        status, submission = call(RAILS, token, "GET", f"/api/submissions/{submission_id}")
        if submission.get("audit_log_url") and all(s["documents"] for s in submission["submitters"]):
            break
    urls = [d["url"] for s in submission["submitters"] for d in s["documents"]] + [submission["audit_log_url"]]
    files = []
    for url in urls:
        out = subprocess.run(["curl", "-sfL", url], capture_output=True, check=True).stdout
        files.append(out)
    return submission_id, files


def main():
    token, template_id = sys.argv[1], int(sys.argv[2])
    pdf = (ROOT / "spec/fixtures/sample-document.pdf").read_bytes()
    b64 = base64.b64encode(pdf).decode()

    both(token, "merge without files", "/api/tools/merge", {})
    both(token, "merge one file", "/api/tools/merge", {"files": [b64]})
    rails, rust = both(token, "merge two files", "/api/tools/merge", {"files": [b64, b64]},
                       lambda body: {k: ("<pdf>" if k == "data" else v) for k, v in body.items()}
                       if isinstance(body, dict) else body)
    if rails[0] == rust[0] == 200:
        lines = lambda data: all(len(l) <= 60 for l in data.split("\n")) and data.endswith("\n")
        global FAILURES
        ok = lines(rails[1]["data"]) and lines(rust[1]["data"])
        FAILURES += not ok
        print(f"{'ok  ' if ok else 'FAIL'} merge data is Base64.encode64 lines")
        compare_pdfs("merged pages", base64.b64decode(rails[1]["data"]), base64.b64decode(rust[1]["data"]))

    both(token, "verify unsigned", "/api/tools/verify", {"file": b64})
    both(token, "verify malformed", "/api/tools/verify", {"file": base64.b64encode(b"not a pdf").decode()})

    submissions = []
    for name, base in (("Rails", RAILS), ("Rust", RUST)):
        submission_id, files = signed_documents(token, template_id, base)
        submissions.append(submission_id)
        for index, data in enumerate(files):
            label = f"verify {name}-signed {'audit trail' if index == len(files) - 1 else f'document {index}'}"
            both(token, label, "/api/tools/verify", {"file": base64.b64encode(data).decode()})
        data = files[-2]
        tampered = bytearray(data)
        position = data.find(b"stream") + 20
        tampered[position] = (tampered[position] + 1) % 256
        both(token, f"verify {name}-signed tampered", "/api/tools/verify",
             {"file": base64.b64encode(bytes(tampered)).decode()})
        both(token, f"verify {name}-signed with appended data", "/api/tools/verify",
             {"file": base64.b64encode(data + b"\nunsigned").decode()})
        rust_only(token, f"verify {name}-signed with an appended revision", "/api/tools/verify",
                  {"file": base64.b64encode(data + b"\n1 0 obj\n<<>>\nendobj\n%%EOF\n").decode()},
                  "not supported yet")

    for submission_id in submissions:
        call(RAILS, token, "DELETE", f"/api/submissions/{submission_id}")
    import writes
    sys.exit(1 if FAILURES or writes.FAILURES else 0)


if __name__ == "__main__":
    main()
