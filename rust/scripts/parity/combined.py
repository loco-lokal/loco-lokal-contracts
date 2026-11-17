#!/usr/bin/env python3
"""Compares the combined result documents (`combine_pdf_result_key`) of Rails and the Rust
server.

    rust/scripts/parity/combined.py

The check seeds its own account ("Parity Combined", `seed.sh combined`) with the setting
enabled, so the other checks keep seeing uncombined results. Both servers need their job
workers and the Mailpit SMTP settings (see emails.py).

For each backend a two-party submission with emails (`order: random`, so that both
parties are invited when it is created) is completed on that backend's
signing form, so its completion job generates the result documents, the combined
document and the audit trail, and sends the completed emails. Then:

1. the stored rows: the combined document's attachment (filename, content type,
   metadata), its lock events and the audit trail;
2. the combined PDFs: rendered pages, words and their positions, metadata, signatures;
3. the emails (the combined document replaces the documents and the audit trail);
4. `/s/:slug/documents` (last and first party, with and without `combined=true`) and
   `/submitters/:slug/download`: the links, and the files both servers serve for them;
5. `GET /api/submissions/:id`.

The combined documents are then deleted, and generated again on each backend through
`GET /api/submissions/:id?include=combined_document_url`, then through
`/s/:slug/documents?combined=true` of the first party; rows and PDFs are compared again.
"""

import base64
import json
import os
import subprocess
import sys
import time
import urllib.parse
import urllib.request

from compare import RAILS, RUST
from downloads import files
from emails import MAILPIT, normalize
from signing import ROOT, Browser, compare_documents, json_text, run_signed
from writes import call, check

import signing
import writes

# The same parties on both backends, which run one after the other; their emails are
# collected and deleted in between.
PARTIES = ("combined-ana@example.test", "combined-bob@example.test")
FAILURES = 0


def rails_runner(script, *args):
    result = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", script, *args],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    return result.stdout


def seed():
    out = subprocess.run([str(ROOT / "rust/scripts/parity/seed.sh"), "combined"],
                         capture_output=True, text=True, check=True).stdout
    values = dict(line.split("=", 1) for line in out.split())
    return values["PARITY_TOKEN"], int(values["PARITY_TEMPLATE_ID"])


def stored(submission_id):
    """The combined document's rows, read through Rails."""
    script = f"""
      s = Submission.find({submission_id})
      c = s.combined_document
      keys = s.submitters.order(:id).map {{ |e| "combined_document:#{{e.id}}" }}
      puts('ROWS:' + {{
        combined: c.attached? && [c.filename.to_s, c.content_type, c.metadata],
        audit_trail: s.audit_trail.attached? && s.audit_trail.filename.to_s,
        attachments: ActiveStorage::Attachment.where(record: s).order(:id).pluck(:name),
        lock_events: LockEvent.where(key: keys).order(:id).map {{ |e| [keys.index(e.key), e.event_name] }}
      }}.to_json)
    """
    out = rails_runner(script)
    return json.loads(next(line[5:] for line in out.splitlines() if line.startswith("ROWS:")))


def combined_pdf(submission_id):
    script = f"""
      s = Submission.find({submission_id})
      puts('PDF:' + (s.combined_document.attached? ? Base64.strict_encode64(s.combined_document.download) : ''))
    """
    data = next(line[4:] for line in rails_runner(script).splitlines() if line.startswith("PDF:"))
    return base64.b64decode(data) if data else None


def drop_combined(submission_ids):
    """Deletes the combined documents and their lock events, as if never generated."""
    ids = ", ".join(str(i) for i in submission_ids)
    rails_runner(f"""
      Submission.where(id: [{ids}]).each do |s|
        s.combined_document.purge
        LockEvent.where(key: s.submitters.map {{ |e| "combined_document:#{{e.id}}" }}).delete_all
      end
    """)


def wait_for_rows(submission_ids):
    """Waits for the combined documents (and Rails' analyze job) of both submissions."""
    deadline = time.time() + 90
    while True:
        rows = [stored(i) for i in submission_ids]
        if all(r["combined"] and r["combined"][2].get("analyzed") for r in rows) or time.time() > deadline:
            return rows
        time.sleep(2)


def compare_rows(label, submission_ids):
    global FAILURES
    a, b = wait_for_rows(submission_ids)
    if a == b:
        print(f"ok   {label} {json.dumps(a)}")
    else:
        FAILURES += 1
        print(f"FAIL {label}\n     rails {json.dumps(a)}\n     rust  {json.dumps(b)}")


def mailpit(method, path):
    request = urllib.request.Request(MAILPIT + path, method=method)
    with urllib.request.urlopen(request) as response:
        text = response.read().decode()
        return json.loads(text) if text.strip().startswith(("{", "[")) else text


def search(addresses):
    """Message summaries to any of `addresses` or to the account's user."""
    wanted = {*addresses, "parity-combined@example.com"}
    listing = mailpit("GET", "/api/v1/messages?limit=1000")["messages"]
    return [m for m in listing if wanted & {t["Address"] for t in m["To"]}]


def emails(submission_id):
    """The completion emails to the parties and to the sender about the submission (the
    invitations come from the Rails API for both backends)."""
    out = []
    for summary in search(PARTIES):
        message = mailpit("GET", f"/api/v1/message/{summary['ID']}")
        to = [t["Address"] for t in message["To"]]
        html = message["HTML"] or ""
        if not any(a in to for a in PARTIES) and f"/submissions/{submission_id}" not in html:
            continue
        if message["Subject"] == "You are invited to sign a document":
            continue
        out.append({
            "subject": message["Subject"],
            "to": to,
            "html": normalize(html),
            "attachments": [(a["FileName"], a["ContentType"]) for a in message["Attachments"]],
            "id": summary["ID"],
        })
    return sorted(out, key=lambda m: (json.dumps(m["to"]), m["subject"]))


def wait_for_invitations(created):
    """Waits until Sidekiq sent both invitations, so their `send_email` events come before
    the signers' events on both backends."""
    deadline = time.time() + 30
    while time.time() < deadline:
        invited = [m for m in all_messages() if m["Subject"] == "You are invited to sign a document"]
        if len(invited) >= len(created):
            return
        time.sleep(0.5)


def all_messages():
    return [m for m in search(PARTIES) if m["To"][0]["Address"] in PARTIES]


def delete_messages(messages):
    if not messages:
        return
    body = json.dumps({"IDs": [m.get("id") or m["ID"] for m in messages]}).encode()
    request = urllib.request.Request(MAILPIT + "/api/v1/messages", data=body, method="DELETE")
    request.add_header("Content-Type", "application/json")
    urllib.request.urlopen(request).read()


def compare_emails(found):
    global FAILURES
    a = [{k: v for k, v in m.items() if k != "id"} for m in found["rails"]]
    b = [{k: v for k, v in m.items() if k != "id"} for m in found["rust"]]
    if a == b:
        print(f"ok   emails [{len(a)} messages, attachments {[m['attachments'] for m in a]}]")
    else:
        FAILURES += 1
        print(f"FAIL emails\n     rails {json.dumps(a)[:3000]}\n     rust  {json.dumps(b)[:3000]}")


def compare_links(label, subs, path_for):
    """A download endpoint on each backend: the response, and the files behind the links."""
    global FAILURES
    results = {b: Browser(base).request("GET", path_for(subs[b])) for b, base in (("rails", RAILS), ("rust", RUST))}
    signing.check(label, results["rails"], results["rust"], json_text)
    a, r = files(results["rails"][1]), files(results["rust"][1])
    if a != r:
        FAILURES += 1
        print(f"FAIL {label} files: rails {a}, rust {r}")


def main():
    global FAILURES
    token, template_id = seed()
    _, template = call(RAILS, token, "GET", f"/api/templates/{template_id}")
    fields = {f["name"]: f["uuid"] for f in template["fields"]}

    delete_messages(all_messages())
    ids, subs, found = {}, {}, {}
    for backend, base in (("rails", RAILS), ("rust", RUST)):
        ids[backend] = run_signed(base, token, template_id, fields, send_email=True, emails=PARTIES,
                                  # Rails invites both parties on creation: no next-party
                                  # invitation racing the second party's completion.
                                  order="random", on_created=wait_for_invitations)
        # Through the same backend: Rails' `show` would generate missing documents.
        _, submission = call(base, token, "GET", f"/api/submissions/{ids[backend]}")
        subs[backend] = submission["submitters"]
        wait_for_rows([ids[backend]])
        time.sleep(5)  # the email jobs
        found[backend] = emails(ids[backend])
        delete_messages(all_messages())
    pair = (ids["rails"], ids["rust"])

    compare_rows("stored rows", pair)
    compare_documents(combined_pdf(ids["rails"]), combined_pdf(ids["rust"]), "combined document")
    compare_emails(found)

    by_role = lambda s, role: next(e for e in s if e["role"] == role)
    last = lambda s: by_role(s, "Second Party")["slug"]
    first = lambda s: by_role(s, "First Party")["slug"]
    compare_links("documents (last party)", subs, lambda s: f"/s/{last(s)}/documents")
    compare_links("documents combined (last party)", subs, lambda s: f"/s/{last(s)}/documents?combined=true")
    compare_links("documents combined (first party)", subs, lambda s: f"/s/{first(s)}/documents?combined=true")
    compare_links("submitters download by slug", subs, lambda s: f"/submitters/{last(s)}/download")

    shown = {b: call(base, token, "GET", f"/api/submissions/{ids[b]}") for b, base in (("rails", RAILS), ("rust", RUST))}
    check("GET /api/submissions/:id", shown["rails"], shown["rust"])

    drop_combined(pair)
    shown = {b: call(base, token, "GET", f"/api/submissions/{ids[b]}?include=combined_document_url")
             for b, base in (("rails", RAILS), ("rust", RUST))}
    check("GET /api/submissions/:id?include=combined_document_url (generates)", shown["rails"], shown["rust"])
    compare_rows("stored rows (generated by the API)", pair)
    compare_documents(combined_pdf(ids["rails"]), combined_pdf(ids["rust"]), "combined document (API)")

    drop_combined(pair)
    compare_links("documents combined (first party, generates)", subs, lambda s: f"/s/{first(s)}/documents?combined=true")
    compare_rows("stored rows (generated by the download)", pair)
    compare_documents(combined_pdf(ids["rails"]), combined_pdf(ids["rust"]), "combined document (download)")

    if os.environ.get("KEEP"):
        print("kept", *pair)
    else:
        for submission_id in pair:
            call(RAILS, token, "DELETE", f"/api/submissions/{submission_id}")
    sys.exit(1 if FAILURES or signing.FAILURES or writes.FAILURES else 0)


if __name__ == "__main__":
    main()
