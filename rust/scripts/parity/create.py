#!/usr/bin/env python3
"""Compares submission creation and submitter updates of Rails and the Rust server.

    rust/scripts/parity/create.py TOKEN TEMPLATE_ID

Every request runs once per backend; each creates its own records. The responses and
the stored submission and submitter rows (read with psql from DATABASE_URL) are
compared with ids, slugs, uuids and timestamps normalized. Created records are
deleted at the end.
"""

import json
import os
import subprocess
import sys

from compare import RAILS, RUST
from writes import call, check, scrub

DATABASE_URL = os.environ.get(
    "DATABASE_URL", "postgres://docuseal:docuseal_dev@127.0.0.1:5435/docuseal"
)
CREATED = {"rails": set(), "rust": set()}


def rows(table, column, ids):
    if not ids:
        return []
    sql = (
        f"SELECT coalesce(json_agg(t ORDER BY t.id), '[]') FROM {table} t "
        f"WHERE {column} IN ({','.join(str(i) for i in ids)})"
    )
    out = subprocess.run(["psql", DATABASE_URL, "-Atc", sql], check=True, capture_output=True, text=True)
    result = []
    for row in json.loads(out.stdout):
        for key in ("values", "preferences", "metadata", "template_fields", "template_schema",
                    "template_submitters", "variables", "variables_schema"):
            if isinstance(row.get(key), str):
                row[key] = json.loads(row[key])
        result.append(row)
    return result


def submission_ids(body):
    if isinstance(body, list):
        return sorted({s["submission_id"] for s in body if isinstance(s, dict) and "submission_id" in s})
    if isinstance(body, dict) and "submitters" in body:
        return submission_ids(body["submitters"])
    return []


def case(token, label, path, body):
    rails = call(RAILS, token, "POST", path, body)
    rust = call(RUST, token, "POST", path, body)
    check(label, rails, rust)
    ids = {"rails": submission_ids(rails[1]), "rust": submission_ids(rust[1])}
    for b in ids:
        CREATED[b].update(ids[b])
    if ids["rails"] or ids["rust"]:
        for table, column in (("submissions", "id"), ("submitters", "submission_id")):
            check(f"  {table} rows", (0, rows(table, column, ids["rails"])), (0, rows(table, column, ids["rust"])))
    return ids


def main():
    token, template_id = sys.argv[1], int(sys.argv[2])
    t = template_id
    base = {"template_id": t, "send_email": False}

    case(token, "submitters by role with values", "/api/submissions", {
        **base, "submitters": [
            {"role": "First Party", "email": "Ana@Example.com", "name": "Ana",
             "values": {"Full Name": "Ana A", "start_date": "05.01.2024", "Plan": "Pro"},
             "metadata": {"crm": 7}, "external_id": "ext-1"},
            {"role": "Second Party", "email": "bob@example.com"}]})
    case(token, "submitters by index, phone", "/api/submissions", {
        **base, "order": "random", "submitters": [
            {"name": "Only Name", "phone": "+491701234"}, {"email": "x@gmial.com"}]})
    case(token, "message, bcc, reply_to", "/api/submissions", {
        **base, "message": {"subject": "Sign <now>", "body": "Hello {{submitter.name}}"},
        "bcc_completed": "audit@example.com", "reply_to": "boss@example.com",
        "completed_redirect_url": "https://example.com/done", "expire_at": "2031-05-01 12:00:00",
        "submitters": [{"role": "First Party", "email": "m@example.com", "go_to_last": True}]})
    case(token, "fields param", "/api/submissions", {
        **base, "submitters": [{"role": "First Party", "email": "f@example.com",
                                "readonly_fields": ["plan"],
                                "fields": [{"name": "Full Name", "default_value": "Pre", "readonly": True,
                                            "title": "Your name", "validation_pattern": "^P",
                                            "invalid_message": "Must start with P"},
                                           {"name": "Start Date", "value": 1704067200}]}]})
    case(token, "merged roles", "/api/submissions", {
        **base, "submitters": [{"roles": ["First Party", "Second Party"], "email": "both@example.com"}]})
    case(token, "cc recipients", "/api/submissions", {
        **base, "cc_recipients": ["cc@example.com", {"name": "CC Two", "email": "cc2@example.com"}],
        "submitters": [{"role": "First Party", "email": "c@example.com"}]})
    case(token, "submission array", "/api/submissions", {
        **base, "submission": [{"submitters": [{"role": "First Party", "email": "s1@example.com"}]},
                               {"name": "Second", "submitters": [{"role": "Second Party", "email": "s2@example.com"}]}]})
    case(token, "init", "/api/submissions/init", {
        **base, "submitters": [{"role": "First Party", "email": "init@example.com"}]})
    case(token, "emails", "/api/submissions/emails", {**base, "emails": "e1@example.com, e2@example.com"})
    case(token, "template path", f"/api/templates/{t}/submissions", {
        "send_email": False, "submitters": [{"email": "path@example.com"}]})
    case(token, "include fields", "/api/submissions", {
        **base, "include": "fields", "submitters": [{"role": "First Party", "email": "inc@example.com",
                                                    "values": {"Full Name": "Inc"}}]})

    sending = {"template_id": t}
    case(token, "send preserved", "/api/submissions", {
        **sending, "submitters": [{"role": "First Party", "email": "p1@example.com"},
                                  {"role": "Second Party", "email": "p2@example.com"}]})
    case(token, "send random", "/api/submissions", {
        **sending, "order": "random", "submitters": [{"role": "First Party", "email": "r1@example.com"},
                                                     {"role": "Second Party", "email": "r2@example.com"}]})
    case(token, "send per-recipient order", "/api/submissions", {
        **sending, "submitters": [{"role": "First Party", "email": "o1@example.com", "order": 1},
                                  {"role": "Second Party", "email": "o2@example.com", "order": 0}]})
    case(token, "send one opted out", "/api/submissions", {
        **sending, "submitters": [{"role": "First Party", "email": "n1@example.com", "send_email": False},
                                  {"role": "Second Party", "email": "n2@example.com"}]})
    case(token, "invite_by", "/api/submissions", {
        **base, "submitters": [{"role": "First Party", "email": "i1@example.com"},
                               {"role": "Second Party", "invite_by": "First Party"}]})
    case(token, "emails sent", "/api/submissions/emails", {**sending, "emails": "es@example.com"})

    # Errors.
    case(token, "no submitters", "/api/submissions", {**base})
    case(token, "invalid email", "/api/submissions", {**base, "submitters": [{"email": "nope"}]})
    case(token, "unknown field", "/api/submissions", {
        **base, "submitters": [{"role": "First Party", "email": "u@example.com",
                                "fields": [{"name": "Missing", "default_value": "x"}]}]})
    case(token, "unknown role", "/api/submissions", {
        **base, "submitters": [{"role": "Nobody", "email": "u@example.com", "values": {"Full Name": "x"}}]})
    case(token, "too many parties", "/api/submissions", {
        **base, "submitters": [{"email": "1@example.com"}, {"email": "2@example.com"},
                               {"email": "3@example.com", "uuid": "x"}]})
    case(token, "missing template", "/api/submissions", {"template_id": 999999, "submitters": [{"email": "a@b.com"}]})
    case(token, "no template id", "/api/submissions", {"submitters": [{"email": "a@b.com"}]})
    case(token, "duplicate roles", "/api/submissions", {
        **base, "submitters": [{"role": "First Party", "email": "a@b.com"}, {"role": "First Party", "email": "c@d.com"}]})

    # Submitter updates on a fresh submission per backend.
    body = {**base, "submitters": [{"role": "First Party", "email": "upd@example.com"},
                                   {"role": "Second Party", "email": "upd2@example.com"}]}
    ids = {}
    for b, url in (("rails", RAILS), ("rust", RUST)):
        status, created = call(url, token, "POST", "/api/submissions", body)
        assert status == 200, created
        ids[b] = created[0]["id"]
        CREATED[b].add(created[0]["submission_id"])
    for label, update in (
        ("PUT submitter values", {"name": "Updated", "email": "New@Example.com", "send_email": False,
                                  "values": {"Full Name": "Changed", "Plan": "Basic"},
                                  "metadata": {"a": 1}, "external_id": "e-2"}),
        ("PUT submitter fields", {"fields": [{"name": "Start Date", "default_value": "2024-02-03",
                                             "readonly": True}], "phone": "+1 555 0100"}),
        ("PUT submitter unknown field", {"values": {"Nope": "x"}}),
        ("PUT submitter message", {"message": {"subject": "Again", "body": "Body"}}),
    ):
        rails = call(RAILS, token, "PUT", f"/api/submitters/{ids['rails']}", update)
        rust = call(RUST, token, "PUT", f"/api/submitters/{ids['rust']}", update)
        check(label, rails, rust)
        check("  submitter row", (0, rows("submitters", "id", [ids["rails"]])),
              (0, rows("submitters", "id", [ids["rust"]])))
        sub = {b: rows("submitters", "id", [ids[b]])[0]["submission_id"] for b in ids}
        check("  submission row", (0, rows("submissions", "id", [sub["rails"]])),
              (0, rows("submissions", "id", [sub["rust"]])))

    for b, url in (("rails", RAILS), ("rust", RUST)):
        for sid in CREATED[b]:
            call(url, token, "DELETE", f"/api/submissions/{sid}?permanently=true")

    from writes import FAILURES
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
