#!/usr/bin/env python3
"""Compares email two-factor verification of the Rails reference app and the Rust server.

    rust/scripts/parity/email_2fa.py TOKEN TEMPLATE_ID

Both servers need their job workers and the same SMTP settings (see emails.py); the codes
are read from Mailpit (MAILPIT_URL). For each backend, as a recipient would:

1. the signing form with `require_email_2fa` (`/s/:slug`, `/submit_form_email_2fa`):
   the page, sending a code, sending again within 15 seconds, a wrong code, the right
   code, and the form behind it;
2. a share link with `shared_link_2fa` (`/d/:slug`, `/start_form_email_2fa_send`):
   starting (the verification page and email), resending, a third send (too many
   attempts), `?email_verification`, a wrong code, the right code;
3. reopening a pending party of an API submission from the share link without the start
   form cookie, verified with a code;
4. resubmitting (`/resubmit_form`) a completed share link party with `shared_link_2fa`.

Pages and redirects are compared after normalizing tokens and slugs, emails with the code
normalized, and so are the stored rows: `otp_challenges` (attempts, verified),
`submission_events`, `email_events` and the share link submissions.

The send limits (`RateLimit`, two per 45 seconds per IP) are kept per process, so the
script waits between the share link cases.
"""

import json
import os
import re
import subprocess
import sys
import time
import urllib.parse

from compare import RAILS, RUST, diff
from emails import mailpit, messages
from signing import Browser, check, page
from writes import call

FAILURES = 0
DB = os.environ.get("DATABASE_URL", "postgres://docuseal:docuseal_dev@127.0.0.1:5435/docuseal")
BASES = {"rails": RAILS, "rust": RUST}
HTTPS = {"X-Forwarded-Proto": "https"}
CODE = re.compile(r"<b>(\d{6})</b>")
WAIT = int(os.environ.get("RATE_WAIT", "46"))
SLUGS = {}


def psql(sql):
    return subprocess.run(["psql", DB, "-tAc", sql], capture_output=True, text=True, check=True).stdout.strip()


def norm(text):
    text = page(text or "")
    for value, name in SLUGS.items():
        text = text.replace(value, f"<{name}>")
    text = re.sub(r"\b\d{6}\b", "CODE", text)
    return text


def compare(label, rails, rust):
    global FAILURES
    before = __import__("signing").FAILURES
    check(label, rails, rust, norm)
    if __import__("signing").FAILURES != before:
        FAILURES += 1


def compare_data(label, rails, rust):
    global FAILURES
    a = norm(json.dumps(rails, indent=1, sort_keys=True))
    b = norm(json.dumps(rust, indent=1, sort_keys=True))
    if a == b:
        print(f"ok   {label}")
    else:
        FAILURES += 1
        print(f"FAIL {label}")
        print(diff(a, b))


def mails(expected):
    """The messages in Mailpit, with their codes; clears the inbox."""
    found = messages(expected)
    mailpit("DELETE", "/api/v1/messages")
    codes = [m.group(1) for m in (CODE.search(x["html"]) for x in found) if m]
    for message in found:
        message["html"] = CODE.sub("<b>CODE</b>", message["html"])
        message["text"] = re.sub(r"\b\d{6}\b", "CODE", message["text"])
    return found, codes


def stored(submitter_ids, template_id, otp_values):
    keys = ",".join(
        "encode(sha256(convert_to('form_email_2fa:' || %s, 'UTF8')), 'hex')" % psql_literal(v)
        for v in otp_values
    ) or "''"
    ids = ",".join(str(int(i)) for i in submitter_ids) or "0"
    return {
        "otp_challenges": json.loads(psql(f"""
          SELECT coalesce(json_agg(json_build_object('version', version, 'attempts', attempts,
                   'verified', verified_at IS NOT NULL, 'ttl_min', round(extract(epoch FROM expire_at - created_at) / 60))
                   ORDER BY key, version), '[]')
          FROM otp_challenges WHERE key IN ({keys})""")),
        "submission_events": json.loads(psql(f"""
          SELECT coalesce(json_agg(json_build_object('type', event_type,
                   'data_keys', (SELECT json_agg(k ORDER BY k) FROM json_object_keys(data::json) k),
                   'email', data::json->>'email') ORDER BY id), '[]')
          FROM submission_events WHERE submitter_id IN ({ids})""")),
        "email_events": json.loads(psql(f"""
          SELECT coalesce(json_agg(json_build_object('tag', tag, 'type', emailable_type,
                   'event', event_type, 'email', email, 'data', data::json) ORDER BY id), '[]')
          FROM email_events WHERE tag = 'otp_verification_email'
            AND ((emailable_type = 'Submitter' AND emailable_id IN ({ids}))
              OR (emailable_type = 'Template' AND emailable_id = {int(template_id or 0)}))""")),
    }


def psql_literal(value):
    return "'" + value.replace("'", "''") + "'"


def clone(token, template_id, name, shared, preferences):
    status, created = call(RAILS, token, "POST", f"/api/templates/{template_id}/clone", {"name": name})
    assert status == 200, (status, created)
    subs = created["submitters"]
    subs[1]["email"] = "defined-party@example.com"
    status, body = call(RAILS, token, "PUT", f"/api/templates/{created['id']}",
                        {"shared_link": shared, "submitters": subs})
    assert status == 200, (status, body)
    if preferences:
        psql(f"UPDATE templates SET preferences = {psql_literal(json.dumps(preferences))} WHERE id = {created['id']}")
    return created["id"], created["slug"]


def location_path(location):
    return urllib.parse.urlsplit(location).path if location else location


def slug_of(location):
    return location_path(location).rsplit("/", 1)[-1]


def submitter_id(slug):
    return int(psql(f"SELECT id FROM submitters WHERE slug = {psql_literal(slug)}"))


def run(browser, method, path, fields=None):
    if method != "GET" and fields is not None:
        fields = [("authenticity_token", browser.csrf)] + fields
    return browser.request(method, path, fields, headers=HTTPS)


def signing_form(token, template_id):
    """`require_email_2fa` on the signing form."""
    results = {}
    for b, base in BASES.items():
        status, created = call(RAILS, token, "POST", "/api/submissions", {
            "template_id": template_id, "send_email": False,
            "submitters": [{"role": "First Party", "email": "two.factor@example.com"},
                           {"role": "Second Party", "email": "second@example.com"}],
        })
        assert status == 200, created
        first = created[0]
        SLUGS[first["slug"]] = "signer"
        psql(f"""UPDATE submitters SET preferences = (preferences::jsonb || '{{"require_email_2fa": true}}')::text
                 WHERE id = {int(first['id'])}""")
        browser = Browser(base)
        slug = first["slug"]
        out = {"submission_id": first["submission_id"], "id": first["id"], "slug": slug}
        out["page"] = run(browser, "GET", f"/s/{slug}")
        out["page_t"] = run(browser, "GET", f"/s/{slug}?t=abc")
        mailpit("DELETE", "/api/v1/messages")
        out["send"] = run(browser, "POST", "/submit_form_email_2fa",
                          [("_method", "put"), ("submitter_slug", slug)])
        out["mails"], codes = mails(1)
        out["sent_page"] = run(browser, "GET", f"/s/{slug}?status=sent")
        out["send_again"] = run(browser, "POST", "/submit_form_email_2fa",
                                [("_method", "put"), ("submitter_slug", slug), ("resend", "true")])
        out["again_page"] = run(browser, "GET", f"/s/{slug}?status=error")
        out["wrong"] = run(browser, "POST", "/submit_form_email_2fa",
                           [("submitter_slug", slug), ("one_time_code", "12-34-56" if codes[:1] != ["123456"] else "654321")])
        out["wrong_page"] = run(browser, "GET", f"/s/{slug}?status=error")
        out["right"] = run(browser, "POST", "/submit_form_email_2fa",
                           [("submitter_slug", slug), ("one_time_code", " ".join(codes[:1]))])
        out["form"] = run(browser, "GET", f"/s/{slug}")
        out["unknown"] = run(browser, "POST", "/submit_form_email_2fa", [("submitter_slug", "nope")])
        out["stored"] = stored([first["id"]], None, [f"two.factor@example.com:{slug}"])
        results[b] = out

    for key in ("page", "page_t", "send", "sent_page", "send_again", "again_page", "wrong", "wrong_page",
                "right", "unknown"):
        compare(f"signing form: {key}", results["rails"][key], results["rust"][key])
    # The form itself is compared by signing.py.
    compare("signing form: form after the code (status)", results["rails"]["form"][:1] + ("", None),
            results["rust"]["form"][:1] + ("", None))
    compare_data("signing form: emails", results["rails"]["mails"], results["rust"]["mails"])
    compare_data("signing form: stored rows", results["rails"]["stored"], results["rust"]["stored"])
    for b in BASES:
        call(RAILS, token, "DELETE", f"/api/submissions/{results[b]['submission_id']}?permanently=true")


def share_link(token, template_id, templates):
    """`shared_link_2fa`: start, resend, too many sends, wrong and right codes."""
    results = {}
    for b, base in BASES.items():
        tid, slug = templates[b]["2fa"]
        browser = Browser(base)
        out = {}
        out["show"] = run(browser, "GET", f"/d/{slug}")
        mailpit("DELETE", "/api/v1/messages")
        start = [("_method", "put"), ("submitter[email]", "Link.Visitor@Example.com")]
        out["start"] = run(browser, "POST", f"/d/{slug}", start)
        out["start_mails"], codes = mails(1)
        resend = [("resend", "true"), ("slug", slug), ("submitter[email]", "Link.Visitor@Example.com"),
                  ("submitter[name]", ""), ("submitter[phone]", "")]
        out["resend"] = run(browser, "POST", "/start_form_email_2fa_send", resend)
        target = urllib.parse.urlsplit(out["resend"][2] or f"/d/{slug}")
        out["resend_page"] = run(browser, "GET", f"{target.path}?{target.query}")
        out["resend_mails"], resent = mails(1)
        out["too_many"] = run(browser, "POST", "/start_form_email_2fa_send", resend)
        out["too_many_mails"], _ = mails(0)
        verification = f"/d/{slug}?email=link.visitor%40example.com&email_verification=true"
        out["too_many_page"] = run(browser, "GET", verification)
        out["no_email"] = run(browser, "POST", "/start_form_email_2fa_send",
                              [("slug", slug), ("submitter[name]", "x")])
        out["no_email_page"] = run(browser, "GET", f"/d/{slug}")
        out["wrong"] = run(browser, "POST", f"/d/{slug}", start + [("one_time_code", "000-000")])
        out["wrong_page"] = run(browser, "GET", verification)
        out["right"] = run(browser, "POST", f"/d/{slug}", start + [("one_time_code", (resent or codes)[0])])
        signer = slug_of(out["right"][2])
        SLUGS[signer] = "link-signer"
        out["form"] = run(browser, "GET", f"/s/{signer}")
        other = Browser(base)
        out["other_browser_form"] = run(other, "GET", f"/s/{signer}")
        out["stored"] = stored([submitter_id(signer)], tid, [f"link.visitor@example.com:{slug}"])
        out["browser"] = browser
        out["signer"] = signer
        results[b] = out

    for key in ("show", "start", "resend", "resend_page", "too_many", "too_many_page", "no_email", "no_email_page",
                "wrong", "wrong_page", "right", "other_browser_form"):
        compare(f"share link: {key}", results["rails"][key], results["rust"][key])
    compare("share link: form after the code (status)", results["rails"]["form"][:1] + ("", None),
            results["rust"]["form"][:1] + ("", None))
    for key in ("start_mails", "resend_mails", "too_many_mails", "stored"):
        compare_data(f"share link: {key}", results["rails"][key], results["rust"][key])
    return results


def reopen(token, templates):
    """A pending party of an API submission, started from the share link in a new browser."""
    results = {}
    for b, base in BASES.items():
        tid, slug = templates[b]["plain"]
        status, created = call(RAILS, token, "POST", "/api/submissions", {
            "template_id": tid, "send_email": False,
            "submitters": [{"role": "First Party", "email": "api.party@example.com"}],
        })
        assert status == 200, created
        party = next(s for s in created if s["email"] == "api.party@example.com")
        SLUGS[party["slug"]] = "api-party"
        browser = Browser(base)
        run(browser, "GET", f"/d/{slug}")
        out = {}
        mailpit("DELETE", "/api/v1/messages")
        start = [("_method", "put"), ("submitter[email]", "api.party@example.com")]
        out["start"] = run(browser, "POST", f"/d/{slug}", start)
        out["mails"], codes = mails(1)
        out["right"] = run(browser, "POST", f"/d/{slug}", start + [("one_time_code", codes[0])])
        out["stored"] = stored([party["id"]], tid, [f"api.party@example.com:{slug}"])
        out["count"] = psql(f"SELECT count(*) FROM submissions WHERE template_id = {int(tid)}")
        results[b] = out
    for key in ("start", "right"):
        compare(f"reopen: {key}", results["rails"][key], results["rust"][key])
    for key in ("mails", "stored", "count"):
        compare_data(f"reopen: {key}", results["rails"][key], results["rust"][key])


def resubmit(token, templates, link):
    """Resubmitting a completed `shared_link_2fa` party."""
    results = {}
    for b in BASES:
        tid, slug = templates[b]["2fa"]
        browser, signer = link[b]["browser"], link[b]["signer"]
        status, body = call(RAILS, token, "PUT", f"/api/submitters/{submitter_id(signer)}", {"completed": True})
        assert status == 200, (status, body)
        out = {}
        mailpit("DELETE", "/api/v1/messages")
        out["resubmit"] = run(browser, "POST", "/resubmit_form", [("_method", "put"), ("resubmit", signer)])
        out["mails"], codes = mails(1)
        out["wrong"] = run(browser, "POST", "/resubmit_form",
                           [("_method", "put"), ("resubmit", signer), ("one_time_code", "1")])
        out["right"] = run(browser, "POST", "/resubmit_form",
                           [("_method", "put"), ("resubmit", signer), ("one_time_code", codes[0])])
        SLUGS[slug_of(out["right"][2])] = "resubmitted"
        out["submissions"] = json.loads(psql(f"""
          SELECT json_agg(json_build_object('source', s.source,
                   'submitters', (SELECT json_agg(json_build_object('email', r.email, 'completed', r.completed_at IS NOT NULL,
                                    'preferences', r.preferences) ORDER BY r.id) FROM submitters r WHERE r.submission_id = s.id))
                   ORDER BY s.id)
          FROM submissions s WHERE s.template_id = {int(tid)}"""))
        out["stored"] = stored([submitter_id(slug_of(out["right"][2]))], tid, [f"link.visitor@example.com:{slug}"])
        results[b] = out
    for key in ("resubmit", "wrong", "right"):
        compare(f"resubmit: {key}", results["rails"][key], results["rust"][key])
    for key in ("mails", "submissions", "stored"):
        compare_data(f"resubmit: {key}", results["rails"][key], results["rust"][key])


def main():
    token, template_id = sys.argv[1], int(sys.argv[2])
    templates = {}
    for b in BASES:
        templates[b] = {
            "2fa": clone(token, template_id, "Share Link 2FA", True, {"shared_link_2fa": True}),
            "plain": clone(token, template_id, "Share Link Reopen", True, None),
        }
        for name, (_, slug) in templates[b].items():
            SLUGS[slug] = name
    try:
        signing_form(token, template_id)
        link = share_link(token, template_id, templates)
        print(f"waiting {WAIT}s for the send limits")
        time.sleep(WAIT)
        reopen(token, templates)
        print(f"waiting {WAIT}s for the send limits")
        time.sleep(WAIT)
        resubmit(token, templates, link)
    finally:
        if not os.environ.get("KEEP"):
            for b in BASES:
                for tid, _ in templates[b].values():
                    call(RAILS, token, "DELETE", f"/api/templates/{tid}?permanently=true")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
