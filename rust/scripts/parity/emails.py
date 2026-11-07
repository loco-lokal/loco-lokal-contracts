#!/usr/bin/env python3
"""Compares the invitation emails of Rails and the Rust server.

    rust/scripts/parity/emails.py TOKEN TEMPLATE_ID

Each case creates a submission with send_email on one backend, waits for the emails in
Mailpit (MAILPIT_URL, default http://127.0.0.1:8025), and then does the same on the
other backend. Both must run their job workers (Sidekiq in the Rails container,
JOB_CONCURRENCY > 0 for Rust). Messages, `email_events` and `send_email` submission
events are compared with slugs, tracking params and message ids normalized.
Mailpit's inbox is cleared before each case; created submissions are deleted.
"""

import json
import os
import re
import sys
import time
import urllib.request

from compare import RAILS, RUST
from create import rows
from writes import call, check

MAILPIT = os.environ.get("MAILPIT_URL", "http://127.0.0.1:8025")
LINK = re.compile(r"/(s|e)/[A-Za-z0-9]+(\?t=[A-Za-z0-9_-]+)?")
UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
IDS = re.compile(r"/submissions/\d+")


def mailpit(method, path):
    request = urllib.request.Request(MAILPIT + path, method=method)
    with urllib.request.urlopen(request) as response:
        text = response.read().decode()
        return json.loads(text) if text.strip().startswith(("{", "[")) else text


def normalize(text, collapse=True):
    text = LINK.sub(lambda m: f"/{m[1]}/SLUG" + ("?t=T" if m[2] else ""), text or "")
    text = IDS.sub("/submissions/ID", UUID.sub("UUID", text))
    return re.sub(r"\s+", " ", text).strip() if collapse else text


def messages(expected):
    deadline = time.time() + 20
    while time.time() < deadline:
        listing = mailpit("GET", "/api/v1/messages")
        if listing["total"] >= expected:
            break
        time.sleep(0.5)
    time.sleep(1)  # anything unexpected
    result = []
    for summary in mailpit("GET", "/api/v1/messages")["messages"]:
        message = mailpit("GET", f"/api/v1/message/{summary['ID']}")
        headers = mailpit("GET", f"/api/v1/message/{summary['ID']}/headers")
        result.append({
            "subject": message["Subject"],
            "from": message["From"],
            "to": message["To"],
            "cc": message["Cc"],
            "bcc": message["Bcc"],
            "reply_to": message["ReplyTo"],
            "html": normalize(message["HTML"]),
            "text": normalize(message["Text"], collapse=False),
            "has_message_uuid": "X-Message-Uuid" in headers,
        })
    return sorted(result, key=lambda m: json.dumps(m["to"]))


def events(submission_ids):
    submitter_ids = [r["id"] for r in rows("submitters", "submission_id", submission_ids)]
    email_events = [
        {k: v for k, v in e.items() if k in ("tag", "emailable_type", "event_type", "email", "data")}
        for e in rows("email_events", "emailable_id", submitter_ids)
        if e["emailable_type"] == "Submitter"
    ]
    email_events.sort(key=lambda e: e["email"])  # the jobs run concurrently
    send_events = [
        e["event_type"] for e in rows("submission_events", "submission_id", submission_ids)
        if e["event_type"] == "send_email"
    ]
    sent = [r["sent_at"] is not None for r in rows("submitters", "submission_id", submission_ids)]
    return {"email_events": email_events, "send_email_events": send_events, "sent": sent}


def run(base, token, body, expected):
    mailpit("DELETE", "/api/v1/messages")
    status, created = call(base, token, "POST", "/api/submissions", body)
    assert status == 200, created
    ids = sorted({s["submission_id"] for s in created})
    result = messages(expected)
    found = events(ids)
    for sid in ids:
        call(base, token, "DELETE", f"/api/submissions/{sid}?permanently=true")
    return result, found


def case(token, label, body, expected=1):
    rails = run(RAILS, token, body, expected)
    rust = run(RUST, token, body, expected)
    check(label, (0, rails[0]), (0, rust[0]))
    check("  events", (0, rails[1]), (0, rust[1]))


def main():
    token, t = sys.argv[1], int(sys.argv[2])
    first = {"role": "First Party", "email": "ana@example.com", "name": "Ana Lopez"}
    second = {"role": "Second Party", "email": "bob@example.com"}

    case(token, "default invitation", {"template_id": t, "submitters": [first, second]})
    case(token, "random order sends to all", {
        "template_id": t, "order": "random", "submitters": [first, second]}, expected=2)
    case(token, "custom message with variables", {
        "template_id": t, "name": "Q3 {contract}",
        "message": {
            "subject": "Sign {{template.name}} for {{account.name}}",
            "body": "Hello {{submitter.first_name}},\n\nplease sign **{{submission.name}}** "
                    "from {{sender.name}} <{{sender.email}}>. Parties: {{submitters}}.\n"
                    "Name: {{submitter.Full Name}}; second: {{submitters[2].email}}",
        },
        "submitters": [{**first, "values": {"Full Name": "Ana L"}}, second]})
    case(token, "long markdown body", {
        "template_id": t,
        "message": {
            "subject": "Wrapped",
            "body": "# Heading\n\n" + "A fairly long sentence that keeps going on and on. " * 4
                    + "\n\n- one\n- two\n\nMail [us](mailto:help@example.com) or see "
                    "[https://example.com](https://example.com) and [docs](https://example.com/docs).",
        },
        "submitters": [first, second]})
    case(token, "message with link variable", {
        "template_id": t,
        "message": {"subject": "Link", "body": "Open {{submitter.link}} now"},
        "submitters": [first, second]})
    case(token, "reply_to and noreply", {
        "template_id": t, "order": "random",
        "submitters": [{**first, "reply_to": "boss@example.com"},
                       {**second, "reply_to": "no-reply@example.com"}]}, expected=2)
    case(token, "plain custom body", {
        "template_id": t, "message": {"subject": "S", "body": "Plain body"},
        "submitters": [first, second]})

    from writes import FAILURES
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
