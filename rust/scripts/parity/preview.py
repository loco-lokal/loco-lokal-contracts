#!/usr/bin/env python3
"""Compares the submission preview of the Rails reference app and the Rust server.

    rust/scripts/parity/preview.py TOKEN TEMPLATE_ID

The preview pages only read, so both backends serve the same submissions, created through
the Rails API and signed on the Rails signing form (with drawn signatures):

- `/e/:slug` signed out (404 before completion), with a valid, a foreign and a bogus
  `sig`, after the 40 minute window (the redirect to `/e/:slug/completed`, still open with
  `sig`), and for a party that requires two-factor verification;
- `/e/:slug` signed in as a user of the account (seed.rb creates it): a submission in
  progress (edit, send email, sign in person, the event log), a declined party, an
  expiring and an archived submission, a single open party (the share link), a party
  of the user's own (resubmit) and a masked field;
- `/e/:slug/completed`, with `?status=error`, `email` and `lang`;
- `/e/:slug/download`: the result document links (the files must be served by both
  servers with the same bytes), its 404s, and combined documents, which the Rust server
  answers with 422 ("not supported yet");
- `/p/:slug` (drawing a signature on a phone) with and without a matching signature
  field, and its redirects.

Pages are compared after normalizing tokens, nonces and signed URLs (`signing.page`).
Completing the parties sends the usual completion emails to Mailpit.
"""

import base64
import hashlib
import hmac
import json
import os
import subprocess
import sys
import time

from compare import RAILS, RUST, SIGNED
from downloads import files
from signing import Browser, check, page, signature_png
from writes import call

FAILURES = 0
DB = os.environ.get("DATABASE_URL", "postgres://docuseal:docuseal_dev@127.0.0.1:5435/docuseal")
SECRET_KEY_BASE = os.environ.get(
    "SECRET_KEY_BASE",
    "rust-dev-secret-key-base-not-for-production-use-0123456789abcdef0123456789abcdef",
)
VIEWER = ("parity-viewer@example.com", "parity-viewer-password")


def sql(statement):
    return subprocess.run(["psql", DB, "-qtAc", statement], capture_output=True, text=True,
                          check=True).stdout.strip()


def signed_id(record_id, purpose):
    """`record.signed_id(purpose:)`, as Rails generates it from SECRET_KEY_BASE."""
    key = hashlib.pbkdf2_hmac("sha256", SECRET_KEY_BASE.encode(), b"active_record/signed_id", 1000, 64)
    data = json.dumps({"_rails": {"data": record_id, "pur": purpose}}, separators=(",", ":"))
    message = base64.urlsafe_b64encode(data.encode()).rstrip(b"=")
    return (message + b"--" + hmac.new(key, message, hashlib.sha256).hexdigest().encode()).decode()


def sig(submitter_id):
    return signed_id(submitter_id, "submitter/download_completed")


def norm(text):
    return SIGNED.sub("/file/SIGNED/", page(text))


def create(token, template_id, submitters, **extra):
    status, created = call(RAILS, token, "POST", "/api/submissions", {
        "template_id": template_id, "send_email": False, "order": "random", "submitters": submitters, **extra})
    assert status == 200, (status, created)
    status, submission = call(RAILS, token, "GET", f"/api/submissions/{created[0]['submission_id']}")
    assert status == 200, (status, submission)
    return created, submission


def sign(submitter, values, signature_field):
    """Completes a party on the Rails signing form with a drawn signature."""
    browser = Browser(RAILS)
    slug = submitter["slug"]
    browser.request("GET", f"/s/{slug}")
    upload = browser.request("POST", f"/s/{slug}/upload", [("type", "signature")],
                             {"file": ("signature.png", "image/png", signature_png())})
    response = browser.request("POST", f"/s/{slug}", [
        ("_method", "put"), ("authenticity_token", browser.csrf),
        *[(f"values[{uuid}]", value) for uuid, value in values],
        (f"values[{signature_field}]", json.loads(upload[1])["uuid"]), ("completed", "true")], files={})
    assert response[0] == 200, response


def decline(submitter, reason):
    browser = Browser(RAILS)
    slug = submitter["slug"]
    browser.request("GET", f"/s/{slug}")
    response = browser.request("POST", f"/s/{slug}/decline", [
        ("authenticity_token", browser.csrf), ("decline_reason", reason)])
    assert response[0] in (200, 302), response


def wait_for_audit_trail(submission_id):
    deadline = time.time() + 60
    while time.time() < deadline:
        if sql("SELECT count(*) FROM active_storage_attachments WHERE record_type = 'Submission' "
               f"AND name = 'audit_trail' AND record_id = {int(submission_id)}") != "0":
            return
        time.sleep(1)
    raise RuntimeError(f"no audit trail for submission {submission_id}")


def sign_in(browser):
    browser.request("GET", "/sign_in")
    status, body, location = browser.request("POST", "/sign_in", [
        ("authenticity_token", browser.csrf), ("user[email]", VIEWER[0]), ("user[password]", VIEWER[1])])
    assert status in (302, 303), (status, location, body[:300])


def main():
    global FAILURES
    token, template_id = sys.argv[1], int(sys.argv[2])
    status, template = call(RAILS, token, "GET", f"/api/templates/{template_id}")
    assert status == 200, template
    fields = {f["name"]: f["uuid"] for f in template["fields"]}

    signed_out = {"rails": Browser(RAILS), "rust": Browser(RUST)}
    signed_in = {"rails": Browser(RAILS), "rust": Browser(RUST)}
    for browser in signed_in.values():
        sign_in(browser)

    def both(label, path, browsers=signed_out, normalize=norm, headers=None):
        rails = browsers["rails"].request("GET", path, headers=headers)
        rust = browsers["rust"].request("GET", path, headers=headers)
        check(label, rails, rust, normalize)
        return rails, rust

    def download(label, path, browsers=signed_out):
        rails, rust = both(label, path, browsers, normalize=lambda t: SIGNED.sub("/file/SIGNED/", t))
        global FAILURES
        a, r = files(rails[1]), files(rust[1])
        if a != r:
            FAILURES += 1
            print(f"FAIL {label} files: rails {a}, rust {r}")

    def unsupported(label, path, browsers=signed_out, headers=None):
        """A branch the Rust server does not port: Rails answers, Rust says so with 422."""
        global FAILURES
        rails = browsers["rails"].request("GET", path, headers=headers)
        rust = browsers["rust"].request("GET", path, headers=headers)
        if rust[0] == 422 and "not supported yet" in rust[1]:
            print(f"ok   {label} [rails {rails[0]}, rust 422 not supported yet]")
        else:
            FAILURES += 1
            print(f"FAIL {label} [rails {rails[0]}, rust {rust[0]}]: {rust[1][:200]}")

    first = {"role": "First Party", "email": "preview-first@example.com", "name": "Ann First"}
    second = {"role": "Second Party", "email": "preview-second@example.com", "phone": "+15551234567"}
    created_ids = []

    # A two-party submission: the first party signed, the second one pending.
    created, submission = create(token, template_id, [first, second])
    created_ids.append(submission["id"])
    slug = submission["slug"]
    sign(created[0], [(fields["Full Name"], "Ann First"), (fields["Start Date"], "2026-03-04"),
                      (fields["Plan"], "Pro")], fields["Signature"])
    time.sleep(3)
    both("show in progress, signed out", f"/e/{slug}")
    both("show in progress with sig", f"/e/{slug}?sig={sig(created[0]['id'])}")
    both("show with a bogus sig", f"/e/{slug}?sig=nope")
    both("show unknown", "/e/nope")
    both("completed page in progress", f"/e/{slug}/completed")
    download("download in progress", f"/e/{slug}/download")
    both("download combined in progress", f"/e/{slug}/download?combined=true")
    both("show in progress, signed in", f"/e/{slug}", signed_in)
    both("show in progress, signed in (turbo frame)", f"/e/{slug}", signed_in,
         headers={"X-Turbo-Request-Id": "1"})

    countersignature = fields["Countersignature"]
    both("draw signature", f"/p/{created[1]['slug']}?f={countersignature[:8]}")
    both("draw signature in German", f"/p/{created[1]['slug']}?f={countersignature[:8]}&lang=de")
    both("draw signature without a field", f"/p/{created[1]['slug']}", normalize=lambda t: "")
    both("draw signature unknown field", f"/p/{created[1]['slug']}?f=zzzz", normalize=lambda t: "")
    both("draw signature completed", f"/p/{created[0]['slug']}?f={countersignature[:8]}")
    both("draw signature unknown", "/p/nope")

    # The second party signs: the submission completes.
    sign(created[1], [(fields["Approver"], "Bob Stone")], countersignature)
    wait_for_audit_trail(submission["id"])
    both("show completed", f"/e/{slug}")
    both("show completed with sig", f"/e/{slug}?sig={sig(created[0]['id'])}")
    both("show completed in German", f"/e/{slug}?lang=de")
    both("show completed, signed in", f"/e/{slug}", signed_in)
    both("completed page", f"/e/{slug}/completed")
    both("completed page with error", f"/e/{slug}/completed?status=error&email=someone%40example.com")
    both("completed page in German", f"/e/{slug}/completed?lang=de")
    both("completed page, signed in", f"/e/{slug}/completed", signed_in)
    download("download", f"/e/{slug}/download")
    unsupported("download combined", f"/e/{slug}/download?combined=true")
    both("draw signature after completion", f"/p/{created[1]['slug']}?f={countersignature[:8]}")

    # A signed link of another submission is ignored.
    other, other_submission = create(token, template_id, [{**first, "email": "preview-other@example.com"}])
    created_ids.append(other_submission["id"])
    both("show with another submission's sig", f"/e/{other_submission['slug']}?sig={sig(created[0]['id'])}")

    # After the 40 minute window the preview needs a signed link.
    sql(f"UPDATE submitters SET completed_at = completed_at - interval '2 hours' "
        f"WHERE submission_id = {int(submission['id'])}")
    both("show after the window", f"/e/{slug}")
    both("show after the window with sig", f"/e/{slug}?sig={sig(created[1]['id'])}")
    both("show after the window, signed in", f"/e/{slug}", signed_in)
    both("download after the window", f"/e/{slug}/download")
    download("download after the window, signed in", f"/e/{slug}/download", signed_in)

    # A party that requires two-factor verification.
    twofa, twofa_submission = create(token, template_id, [first, second])
    created_ids.append(twofa_submission["id"])
    call(RAILS, token, "PUT", f"/api/submitters/{twofa[1]['id']}", {"completed": True})
    sql(f"UPDATE submitters SET preferences = preferences::jsonb || '{{\"require_email_2fa\": true}}' "
        f"WHERE id = {int(twofa[0]['id'])}")
    time.sleep(3)
    both("show with two-factor party", f"/e/{twofa_submission['slug']}")
    both("show with two-factor party and sig", f"/e/{twofa_submission['slug']}?sig={sig(twofa[1]['id'])}")
    both("download with two-factor party", f"/e/{twofa_submission['slug']}/download")

    # Signed in: a declined party, expiry, archiving, a single open party, the user's own.
    declined, declined_submission = create(token, template_id, [first, second])
    created_ids.append(declined_submission["id"])
    decline(declined[1], "Not my contract.\n\nPlease <check> the name.")
    both("show with a declined party, signed in", f"/e/{declined_submission['slug']}", signed_in)

    expiring, expiring_submission = create(token, template_id, [first, second], expire_at="2099-01-02 10:00:00 UTC")
    created_ids.append(expiring_submission["id"])
    both("show expiring, signed in", f"/e/{expiring_submission['slug']}", signed_in)
    call(RAILS, token, "DELETE", f"/api/submissions/{expiring_submission['id']}")
    both("show archived, signed in", f"/e/{expiring_submission['slug']}", signed_in)

    single, single_submission = create(token, template_id, [{**first, "email": "preview-single@example.com"}])
    created_ids.append(single_submission["id"])
    both("show single open party, signed in", f"/e/{single_submission['slug']}", signed_in)

    own, own_submission = create(token, template_id, [{**first, "email": VIEWER[0]}, second])
    created_ids.append(own_submission["id"])
    call(RAILS, token, "PUT", f"/api/submitters/{own[0]['id']}", {"completed": True})
    time.sleep(3)
    both("show own completed party, signed in", f"/e/{own_submission['slug']}", signed_in)
    sql(f"UPDATE submissions SET archived_at = now() WHERE id = {int(own_submission['id'])}")
    both("show archived with a completed party, signed in", f"/e/{own_submission['slug']}", signed_in)

    # A masked field (`preferences.mask`): masked for the signed link, revealed on hover
    # for users of the account.
    masked, masked_submission = create(token, template_id, [first])
    created_ids.append(masked_submission["id"])
    sql(f"""UPDATE submissions SET template_fields = (
              SELECT jsonb_agg(CASE WHEN f->>'name' = 'Full Name'
                                    THEN f || '{{"preferences": {{"mask": 2}}}}' ELSE f END)::text
              FROM jsonb_array_elements((SELECT fields::jsonb FROM templates WHERE id = {template_id})) f)
            WHERE id = {int(masked_submission['id'])}""")
    sign(masked[0], [(fields["Full Name"], "Ann First")], fields["Signature"])
    time.sleep(3)
    both("show masked value with sig", f"/e/{masked_submission['slug']}?sig={sig(masked[0]['id'])}")
    both("show masked value, signed in", f"/e/{masked_submission['slug']}", signed_in)

    unsupported("show in the mobile app", f"/e/{single_submission['slug']}", signed_in,
                headers={"User-Agent": "Hotwire Native iOS"})

    if not os.environ.get("KEEP"):
        for submission_id in created_ids:
            call(RAILS, token, "DELETE", f"/api/submissions/{submission_id}?permanently=true")
    import signing
    sys.exit(1 if FAILURES or signing.FAILURES else 0)


if __name__ == "__main__":
    main()
