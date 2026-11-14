#!/usr/bin/env python3
"""Compares the template share link (`/d/:slug`) of the Rails reference app and the Rust server.

    rust/scripts/parity/start_form.py TOKEN TEMPLATE_ID

Each backend gets its own clones of the template (the share link finds earlier
submitters of the same template): a shared one with a single open party (the second party
has a fixed email), a shared one with two open parties, and a private one. Then, as a
visitor would:

1. opens the pages (shared, private, unknown);
2. starts with an email, starts again with the start form cookie (the same submitter)
   and from another browser (a new one);
3. starts the two-party template (the multiple signers error);
4. completes the first submitter through the Rails API, starts again with its email (the
   completed page) and resubmits from there and from `/resubmit_form`.

Responses are compared after normalizing tokens and slugs, and so are the stored
submissions of each clone.
"""

import json
import os
import re
import subprocess
import sys
import urllib.parse

from compare import RAILS, RUST, diff
from signing import Browser, check, page
from writes import call

FAILURES = 0
DB = os.environ.get("DATABASE_URL", "postgres://docuseal:docuseal_dev@127.0.0.1:5435/docuseal")
BASES = {"rails": RAILS, "rust": RUST}


def clone(token, template_id, name, submitters_patch, shared):
    status, created = call(RAILS, token, "POST", f"/api/templates/{template_id}/clone", {"name": name})
    assert status == 200, (status, created)
    subs = created["submitters"]
    submitters_patch(subs)
    status, body = call(RAILS, token, "PUT", f"/api/templates/{created['id']}",
                        {"shared_link": shared, "submitters": subs})
    assert status == 200, (status, body)
    return created["id"], created["slug"]


def stored(template_id):
    """The submissions a backend stored for a template, without ids, slugs and times."""
    sql = f"""
      SELECT coalesce(json_agg(row_to_json(t) ORDER BY t.id), '[]') FROM (
        SELECT s.id, s.source, s.submitters_order, s.template_submitters, s.template_fields,
               s.template_schema, s.preferences, s.name, s.expire_at, s.created_by_user_id,
               (SELECT json_agg(json_build_object(
                  'uuid', r.uuid, 'email', r.email, 'name', r.name, 'phone', r.phone,
                  'values', r.values, 'preferences', r.preferences, 'metadata', r.metadata,
                  'ip', r.ip IS NOT NULL, 'ua', r.ua, 'sent', r.sent_at IS NOT NULL,
                  'completed', r.completed_at IS NOT NULL, 'same_times', r.created_at = r.updated_at)
                  ORDER BY r.id) FROM submitters r WHERE r.submission_id = s.id) AS submitters
        FROM submissions s WHERE s.template_id = {int(template_id)}) t
    """
    out = subprocess.run(["psql", DB, "-tAc", sql], capture_output=True, text=True, check=True).stdout
    rows = json.loads(out)
    for row in rows:
        row.pop("id")
    text = json.dumps(rows, indent=1, sort_keys=True)
    return re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<uuid>", text)


def norm(text):
    text = page(text)
    for b in ("rails", "rust"):
        for name, (tid, slug) in TEMPLATES[b].items():
            text = text.replace(f"/templates/{tid}/", f"/templates/{name}/")
            text = text.replace(slug, f"<{name}>")
    return text


TEMPLATES = {"rails": {}, "rust": {}}
HTTPS = {"X-Forwarded-Proto": "https"}


def location_path(location):
    if not location:
        return location
    return urllib.parse.urlsplit(location).path


def main():
    global FAILURES
    token, template_id = sys.argv[1], int(sys.argv[2])

    def single(subs):
        subs[1]["email"] = "defined-party@example.com"

    for b in ("rails", "rust"):
        TEMPLATES[b]["single"] = clone(token, template_id, "Share Link", single, True)
        TEMPLATES[b]["multi"] = clone(token, template_id, "Share Link Multi", lambda s: None, True)
        TEMPLATES[b]["private"] = clone(token, template_id, "Private Link", lambda s: None, False)
        TEMPLATES[b]["fields"] = clone(token, template_id, "Share Link Fields", single, True)
        call(RAILS, token, "PUT", f"/api/templates/{TEMPLATES[b]['fields'][0]}",
             {"preferences": {"link_form_fields": ["name", "email", "phone"]}})

    browsers = {b: Browser(BASES[b]) for b in BASES}

    def both(label, method, path_for, fields_for=None, browser_for=None):
        results = {}
        for b in BASES:
            browser = browser_for(b) if browser_for else browsers[b]
            if method != "GET" and browser.csrf is None:
                browser.request("GET", f"/d/{TEMPLATES[b]['single'][1]}", headers=HTTPS)
            fields = fields_for(b, browser) if fields_for else None
            # As behind a TLS proxy: Rails writes its secure cookies only for https.
            results[b] = browser.request(method, path_for(b), fields, headers=HTTPS)
        before = FAILURES
        check(label, results["rails"], results["rust"], norm)
        globals()["FAILURES"] = FAILURES + (FAILURES != before)
        return results

    slug = lambda b, name: TEMPLATES[b][name][1]
    start = lambda email: (lambda b, br: [("_method", "put"), ("authenticity_token", br.csrf),
                                          ("submitter[email]", email)])

    both("show shared", "GET", lambda b: f"/d/{slug(b, 'single')}")
    both("show shared with email param", "GET", lambda b: f"/d/{slug(b, 'single')}?email=a%40example.com")
    both("show private (signed out)", "GET", lambda b: f"/d/{slug(b, 'private')}")
    both("show unknown", "GET", lambda b: "/d/nope")
    both("completed without params", "GET", lambda b: f"/d/{slug(b, 'single')}/completed")

    first = both("start", "POST", lambda b: f"/d/{slug(b, 'single')}", start("Visitor@Example.com"))
    again = both("start again (cookie)", "POST", lambda b: f"/d/{slug(b, 'single')}", start("visitor@example.com"))
    for b in BASES:
        if location_path(first[b][2]) != location_path(again[b][2]):
            FAILURES += 1
            print(f"FAIL {b}: the start form cookie did not reopen the submitter")
    other = {b: Browser(BASES[b]) for b in BASES}
    third = both("start from another browser", "POST", lambda b: f"/d/{slug(b, 'single')}",
                 start("visitor@example.com"), lambda b: other[b])
    for b in BASES:
        if location_path(first[b][2]) == location_path(third[b][2]):
            FAILURES += 1
            print(f"FAIL {b}: another browser reopened the submitter")
    both("start two open parties", "POST", lambda b: f"/d/{slug(b, 'multi')}", start("visitor@example.com"))
    # Without the requested details (`field_error_proc`).
    both("start without email", "POST", lambda b: f"/d/{slug(b, 'single')}", start(""))
    both("start with a blank email", "POST", lambda b: f"/d/{slug(b, 'single')}", start("  "))
    both("start two open parties without email", "POST", lambda b: f"/d/{slug(b, 'multi')}", start(""))
    both("start without email (German)", "POST", lambda b: f"/d/{slug(b, 'single')}?lang=de", start(""))
    both("start all fields blank", "POST", lambda b: f"/d/{slug(b, 'fields')}",
         lambda b, br: [("_method", "put"), ("authenticity_token", br.csrf), ("submitter[email]", ""),
                        ("submitter[name]", " "), ("submitter[phone]", "")])
    both("start without phone", "POST", lambda b: f"/d/{slug(b, 'fields')}",
         lambda b, br: [("_method", "put"), ("authenticity_token", br.csrf), ("submitter[email]", "a@example.com"),
                        ("submitter[name]", "A & B"), ("submitter[phone]", "")])
    both("start private", "POST", lambda b: f"/d/{slug(b, 'private')}", start("visitor@example.com"))

    # Complete the first submitter, then start again with its email.
    for b in BASES:
        s = location_path(first[b][2]).rsplit("/", 1)[-1]
        out = subprocess.run(["psql", DB, "-tAc", f"SELECT id FROM submitters WHERE slug = '{s}'"],
                             capture_output=True, text=True, check=True).stdout.strip()
        status, body = call(RAILS, token, "PUT", f"/api/submitters/{out}", {"completed": True})
        assert status == 200, (status, body)
    done = both("start completed", "POST", lambda b: f"/d/{slug(b, 'single')}", start("visitor@example.com"))
    both("completed page", "GET",
         lambda b: f"/d/{slug(b, 'single')}/completed?email=visitor%40example.com")
    both("resubmit from completed page", "POST", lambda b: f"/d/{slug(b, 'single')}",
         lambda b, br: [("_method", "put"), ("authenticity_token", br.csrf),
                        ("submitter[email]", "visitor@example.com"), ("resubmit", "true")])
    first_slug = {b: location_path(first[b][2]).rsplit("/", 1)[-1] for b in BASES}
    both("signing completed page (resubmit button)", "GET", lambda b: f"/s/{first_slug[b]}/completed")
    both("resubmit form", "POST", lambda b: "/resubmit_form",
         lambda b, br: [("_method", "put"), ("authenticity_token", br.csrf), ("resubmit", first_slug[b])])
    both("resubmit form unknown", "POST", lambda b: "/resubmit_form",
         lambda b, br: [("_method", "put"), ("authenticity_token", br.csrf), ("resubmit", "nope")])
    del done

    # Resubmitting a party whose `default_values` include an uploaded signature: the copy
    # keeps the attachment uuid the values refer to.
    from signing import signature_png
    import base64
    resubmitted = {}
    for b in BASES:
        tid = TEMPLATES[b]["single"][0]
        _, tpl = call(RAILS, token, "GET", f"/api/templates/{tid}")
        names = {f["name"]: f["uuid"] for f in tpl["fields"]}
        status, created = call(RAILS, token, "POST", f"/api/templates/{tid}/submissions", {
            "send_email": False,
            "submitters": [{"role": "First Party", "email": "default-values@example.com",
                            "values": {"Full Name": "Dee Fault",
                                       "Signature": "data:image/png;base64," + base64.b64encode(signature_png()).decode()}},
                           {"role": "Second Party", "email": "defined-party@example.com"}]})
        assert status == 200, created
        first = next(x for x in created if x["role"] == "First Party")
        subprocess.run(["psql", DB, "-tAc", f"UPDATE submissions SET source = 'invite' WHERE id = {int(first['submission_id'])}"],
                       check=True, capture_output=True)
        status, body = call(RAILS, token, "PUT", f"/api/submitters/{first['id']}", {"completed": True})
        assert status == 200, (status, body)
        resubmitted[b] = first["slug"]
        del names
    both("default values completed page", "GET", lambda b: f"/s/{resubmitted[b]}/completed")
    both("resubmit default values", "POST", lambda b: "/resubmit_form",
         lambda b, br: [("_method", "put"), ("authenticity_token", br.csrf), ("resubmit", resubmitted[b])])
    for b in BASES:
        out = subprocess.run(["psql", DB, "-tAc", f"""
            SELECT json_agg(json_build_array(a.uuid = ANY(SELECT a2.uuid FROM active_storage_attachments a2
                                                           JOIN submitters r2 ON r2.id = a2.record_id
                                                           WHERE r2.slug = '{resubmitted[b]}'),
                                             a.blob_id, a.name) ORDER BY a.id)
            FROM active_storage_attachments a JOIN submitters r ON r.id = a.record_id AND a.record_type = 'Submitter'
            WHERE r.email = 'default-values@example.com' AND r.template_id = {int(TEMPLATES[b]['single'][0])}
              AND r.completed_at IS NULL"""], capture_output=True, text=True, check=True).stdout.strip()
        rows = json.loads(out or "null") or []
        ok = len(rows) == 1 and rows[0][0] is True
        print(f"{'ok  ' if ok else 'FAIL'} resubmit copies the attachment ({b}) {rows}")
        FAILURES += not ok

    for name in ("single", "multi", "private", "fields"):
        a, r = stored(TEMPLATES["rails"][name][0]), stored(TEMPLATES["rust"][name][0])
        if a == r:
            print(f"ok   stored submissions ({name})")
        else:
            FAILURES += 1
            print(f"FAIL stored submissions ({name})")
            print(diff(a, r))

    if not os.environ.get("KEEP"):
        for b in BASES:
            for tid, _ in TEMPLATES[b].values():
                call(RAILS, token, "DELETE", f"/api/templates/{tid}?permanently=true")
    import signing
    sys.exit(1 if FAILURES or signing.FAILURES else 0)


if __name__ == "__main__":
    main()
