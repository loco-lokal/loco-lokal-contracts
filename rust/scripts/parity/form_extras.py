#!/usr/bin/env python3
"""Compares the signing form's smaller endpoints of the Rails reference app and the Rust server.

    rust/scripts/parity/form_extras.py TOKEN TEMPLATE_ID

Run the Rust server with job workers, as for signing.py. For each backend this script:

1. invites a party (`POST /s/:slug/invite`) from a clone of the template whose second
   party is invited by the first, with the request errors (no `submission`, no email,
   completed);
2. reads the documents' text runs (`GET /s/:slug/metadata`); the cached
   `document_metadata` row is deleted before each backend creates it;
3. uploads a signature with `remember_signature=true` (the `signature_uuids` cookie),
   opens another submission of the same email (the remembered signature is prefilled),
   attaches it (`POST /s/:slug/prefill_attachments`) twice and with bad tokens, and opens
   a form with `signed_signature_uuids`.

Responses and pages are compared after normalizing tokens, slugs and uuids, and so are
the rows each backend stored.
"""

import json
import os
import re
import subprocess
import sys
import time
import urllib.parse

from compare import RAILS, RUST, diff
from signing import ROOT, Browser, check, json_text, page, signature_png
from writes import call

FAILURES = 0
DB = os.environ.get("DATABASE_URL", "postgres://docuseal:docuseal_dev@127.0.0.1:5435/docuseal")
BASES = {"rails": RAILS, "rust": RUST}


def sql(query):
    return subprocess.run(["psql", DB, "-tAc", query], capture_output=True, text=True, check=True).stdout.strip()


def rails_runner(code):
    result = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", code],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    return result.stdout.strip().splitlines()[-1]


def stored(submission_id):
    """A submission's submitters, events and attachments, without ids, slugs and times."""
    query = f"""
      SELECT json_build_object(
        'submitters_order', s.submitters_order,
        'submitters', (SELECT json_agg(json_build_object(
            'uuid', r.uuid, 'email', r.email, 'template_id', r.template_id = s.template_id,
            'values', r.values, 'completed', r.completed_at IS NOT NULL, 'sent', r.sent_at IS NOT NULL,
            'attachments', (SELECT coalesce(json_agg(json_build_array(a.name, b.content_type, b.filename)
                                                     ORDER BY a.id), '[]')
                            FROM active_storage_attachments a JOIN active_storage_blobs b ON b.id = a.blob_id
                            WHERE a.record_type = 'Submitter' AND a.record_id = r.id),
            'events', (SELECT coalesce(json_agg(json_build_array(e.event_type, e.data) ORDER BY e.id), '[]')
                       FROM submission_events e WHERE e.submitter_id = r.id))
          ORDER BY r.id) FROM submitters r WHERE r.submission_id = s.id))
      FROM submissions s WHERE s.id = {int(submission_id)}
    """
    rows = json.loads(sql(query))
    for submitter in rows["submitters"]:
        for event in submitter["events"]:
            data = event[1] = json.loads(event[1])
            for key in ("ip", "sid", "uid"):
                if key in data:
                    data[key] = "<present>"
    text = json.dumps(rows, indent=1, sort_keys=True)
    return re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<uuid>", text)


def compare_stored(label, ids):
    global FAILURES
    a, b = stored(ids["rails"]), stored(ids["rust"])
    if a == b:
        print(f"ok   stored rows ({label})")
    else:
        FAILURES += 1
        print(f"FAIL stored rows ({label})")
        print(diff(a, b))


def norm(text):
    text = re.sub(r"(&quot;token&quot;:&quot;)[^&]+", r"\1TOKEN", text)
    text = re.sub(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z", "TIME", text)
    return page(text) if text.lstrip().startswith("<") else json_text(text)


def compare(label, results):
    global FAILURES
    before = __import__("signing").FAILURES
    check(label, results["rails"], results["rust"], norm)
    signing = __import__("signing")
    FAILURES += signing.FAILURES - before
    signing.FAILURES = before


def cookie(browser):
    """The `signature_uuids` Set-Cookie attributes of the last response."""
    for value in browser.headers.get_all("Set-Cookie") or []:
        if value.startswith("signature_uuids="):
            parts = [p.strip().lower() for p in value.split(";")[1:]]
            return sorted(re.sub(r"expires=.*", "expires=<time>", p) for p in parts)
    return None


def create(token, template_id, submitters):
    status, created = call(RAILS, token, "POST", "/api/submissions",
                           {"template_id": template_id, "send_email": False, "submitters": submitters})
    assert status == 200, created
    return created


def clone(token, template_id, patch):
    status, created = call(RAILS, token, "POST", f"/api/templates/{template_id}/clone", {"name": "Invite Form"})
    assert status == 200, created
    subs = created["submitters"]
    patch(subs)
    status, body = call(RAILS, token, "PUT", f"/api/templates/{created['id']}", {"submitters": subs})
    assert status == 200, body
    return created["id"]


def form(browser, *pairs):
    return [("_method", "put"), ("authenticity_token", browser.csrf), *pairs]


def main():
    global FAILURES
    token, template_id = sys.argv[1], int(sys.argv[2])
    _, template = call(RAILS, token, "GET", f"/api/templates/{template_id}")
    fields = {f["name"]: f["uuid"] for f in template["fields"]}

    def invited(subs):
        subs[1]["invite_by_uuid"] = subs[0]["uuid"]

    templates = {b: clone(token, template_id, invited) for b in BASES}
    clones = {b: call(RAILS, token, "GET", f"/api/templates/{t}")[1] for b, t in templates.items()}
    clone_fields = {b: {f["name"]: f["uuid"] for f in c["fields"]} for b, c in clones.items()}
    invited_uuid = {b: c["submitters"][1]["uuid"] for b, c in clones.items()}
    cleanup = []

    # 1. Inviting the second party.
    results, ids, browsers = {}, {}, {}
    for b, base in BASES.items():
        created = create(token, templates[b], [{"role": "First Party", "email": "ana@example.test"}])
        submitter = created[0]
        ids[b] = submitter["submission_id"]
        cleanup.append(ids[b])
        browser = browsers[b] = Browser(base)
        s = submitter["slug"]
        steps = results[b] = {}
        steps["show"] = browser.request("GET", f"/s/{s}")
        steps["upload remembered"] = browser.request(
            "POST", f"/s/{s}/upload", [("type", "signature"), ("remember_signature", "true")],
            {"file": ("signature.png", "image/png", signature_png())},
        )
        steps["remember cookie"] = (200, json.dumps(cookie(browser)), None)
        signature_uuid = json.loads(steps["upload remembered"][1])["uuid"]
        steps["save draft"] = browser.request(
            "POST", f"/s/{s}",
            form(browser, (f"values[{clone_fields[b]['Full Name']}]", "Ana Example"),
                 (f"values[{clone_fields[b]['Signature']}]", signature_uuid)),
            files={},
        )
        steps["complete without invite"] = browser.request(
            "POST", f"/s/{s}", form(browser, ("completed", "true")), files={})
        steps["invite without submission"] = browser.request(
            "POST", f"/s/{s}/invite", [("authenticity_token", browser.csrf)], files={})
        steps["invite"] = browser.request(
            "POST", f"/s/{s}/invite",
            [("authenticity_token", browser.csrf), ("submission[submitters][][uuid]", invited_uuid[b]),
             ("submission[submitters][][email]", "Bob@Example.TEST")],
            files={},
        )
        steps["invite again"] = browser.request(
            "POST", f"/s/{s}/invite",
            [("authenticity_token", browser.csrf), ("submission[submitters][][uuid]", invited_uuid[b]),
             ("submission[submitters][][email]", "bob@example.test")],
            files={},
        )
        steps["unknown"] = browser.request("POST", "/s/NoSuchSlug1234/invite",
                                           [("authenticity_token", browser.csrf)], files={})
    for label in results["rails"]:
        compare(label, {b: results[b][label] for b in BASES})
    time.sleep(4)  # the completion jobs
    compare_stored("invite", ids)

    # An invite without an email: nothing is created and the form stays open.
    results, ids = {}, {}
    for b, base in BASES.items():
        created = create(token, templates[b], [{"role": "First Party", "email": "cy@example.test"}])
        ids[b] = created[0]["submission_id"]
        cleanup.append(ids[b])
        browser = Browser(base)
        s = created[0]["slug"]
        browser.request("GET", f"/s/{s}")
        results[b] = browser.request(
            "POST", f"/s/{s}/invite",
            [("authenticity_token", browser.csrf), ("submission[submitters][][uuid]", invited_uuid[b]),
             ("submission[submitters][][email]", " ")],
            files={},
        )
    compare("invite without email", results)
    compare_stored("invite without email", ids)

    # 2. Text runs.
    checksum = rails_runner(
        f"puts Template.find({template_id}).documents.first.blob.checksum"
    )
    results, stored_runs, ids = {}, {}, {}
    for b, base in BASES.items():
        created = create(token, template_id, [{"role": "First Party", "email": "dee@example.test"},
                                              {"role": "Second Party", "email": "eve@example.test"}])
        ids[b] = created[0]["submission_id"]
        cleanup.append(ids[b])
        sql(f"DELETE FROM document_metadata WHERE blob_checksum = '{checksum}'")
        browser = Browser(base)
        first, second = created[0]["slug"], created[1]["slug"]
        results.setdefault("metadata (created)", {})[b] = browser.request("GET", f"/s/{first}/metadata")
        stored_runs[b] = sql(f"SELECT text_runs FROM document_metadata WHERE blob_checksum = '{checksum}'")
        results.setdefault("metadata (cached)", {})[b] = browser.request("GET", f"/s/{first}/metadata")
        results.setdefault("metadata second party", {})[b] = browser.request("GET", f"/s/{second}/metadata")
        results.setdefault("metadata unknown", {})[b] = browser.request("GET", "/s/NoSuchSlug1234/metadata")
        browser.request("GET", f"/s/{second}")
        browser.request("POST", f"/s/{second}/decline", [("authenticity_token", browser.csrf)])
        results.setdefault("metadata declined", {})[b] = browser.request("GET", f"/s/{second}/metadata")
    for label, pair in results.items():
        compare(label, pair)
    if stored_runs["rails"] == stored_runs["rust"] and stored_runs["rails"]:
        print(f"ok   stored text runs [{len(stored_runs['rails'])} bytes]")
    else:
        FAILURES += 1
        print("FAIL stored text runs")
        print(diff(stored_runs["rails"], stored_runs["rust"]))

    # 3. The remembered signature and prefill attachments.
    results, ids = {}, {}
    for b, base in BASES.items():
        browser = browsers[b]
        created = create(token, template_id, [{"role": "First Party", "email": "ana@example.test"},
                                              {"role": "Second Party", "email": "bob@example.test"}])
        other = create(token, template_id, [{"role": "First Party", "email": "fay@example.test"},
                                            {"role": "Second Party", "email": "gus@example.test"}])
        ids[b] = created[0]["submission_id"]
        cleanup += [ids[b], other[0]["submission_id"]]
        s = created[0]["slug"]
        steps = results[b] = {}
        steps["show remembered"] = browser.request("GET", f"/s/{s}")
        match = re.search(r'data-prefill-signature="\{&quot;token&quot;:&quot;([^&]+)', steps["show remembered"][1])
        prefill_token = match.group(1) if match else ""
        steps["prefill token found"] = (200, json.dumps(bool(match)), None)
        steps["prefill"] = browser.request("POST", f"/s/{s}/prefill_attachments",
                                           [("prefill_token", prefill_token)], files={})
        steps["prefill again"] = browser.request("POST", f"/s/{s}/prefill_attachments",
                                                 [("prefill_token", prefill_token)], files={})
        steps["prefill other submitter"] = browser.request(
            "POST", f"/s/{other[0]['slug']}/prefill_attachments", [("prefill_token", prefill_token)], files={})
        steps["prefill forged token"] = browser.request("POST", f"/s/{s}/prefill_attachments",
                                                        [("prefill_token", "forged")], files={})
        steps["show other email"] = browser.request("GET", f"/s/{other[0]['slug']}")
        signature = sql(f"""SELECT a.uuid FROM active_storage_attachments a JOIN submitters r ON r.id = a.record_id
                            WHERE a.record_type = 'Submitter' AND r.email = 'ana@example.test'
                            AND r.submission_id = {int(cleanup[0 if b == 'rails' else 1])} ORDER BY a.id LIMIT 1""")
        signed = rails_runner(f"puts Submitters::FindRememberedSignature.sign_signature_uuid('{signature}')")
        fresh = Browser(base)
        query = urllib.parse.urlencode({"signed_signature_uuids[ana@example.test]": signed})
        steps["show signed uuids"] = fresh.request("GET", f"/s/{s}?{query}")
        query = urllib.parse.urlencode({"signed_signature_uuids[ana@example.test]": "forged"})
        steps["show forged signed uuids"] = fresh.request("GET", f"/s/{s}?{query}")
        steps["show without cookie"] = fresh.request("GET", f"/s/{s}")
    for label in results["rails"]:
        compare(label, {b: results[b][label] for b in BASES})
    compare_stored("prefill attachments", ids)

    # 4. Starting a template as the signed-in user (`PUT /start_form_self/:id`).
    rails_runner(
        "u = User.find_by(email: 'parity-self@example.test') || User.new(email: 'parity-self@example.test'); "
        "u.update!(account: User.order(:id).first.account, first_name: 'Pat', last_name: 'Self', role: 'admin', "
        "password: 'parity-self-password', archived_at: nil); puts u.id"
    )
    import start_form

    self_templates = {b: clone(token, template_id, lambda subs: None) for b in BASES}
    archived = {b: clone(token, template_id, lambda subs: None) for b in BASES}
    for tid in archived.values():
        call(RAILS, token, "DELETE", f"/api/templates/{tid}")
    https = {"X-Forwarded-Proto": "https"}
    results = {}
    for b, base in BASES.items():
        user = Browser(base)
        user.request("GET", "/sign_in", headers=https)
        signed_in = user.request("POST", "/sign_in", [("authenticity_token", user.csrf),
                                                      ("user[email]", "parity-self@example.test"),
                                                      ("user[password]", "parity-self-password")], headers=https)
        assert signed_in[0] in (302, 303), (b, signed_in[:1])
        user.request("GET", "/", headers=https)
        steps = results[b] = {}
        put = lambda tid, browser=user: browser.request(
            "POST", f"/start_form_self/{tid}", [("_method", "put"), ("authenticity_token", browser.csrf)],
            headers=https)
        steps["start self"] = put(self_templates[b])
        steps["start self again"] = put(self_templates[b])
        same = steps["start self"][2] == steps["start self again"][2]
        steps["start self reopens"] = (200, json.dumps(same), None)
        # A draft saves the fields snapshot; after the template's fields change, a new
        # submission starts.
        slug = (steps["start self"][2] or "").rsplit("/", 1)[-1]
        user.request("GET", f"/s/{slug}", headers=https)
        user.request("POST", f"/s/{slug}", [("_method", "put"), ("authenticity_token", user.csrf)],
                     files={}, headers=https)
        _, tpl = call(RAILS, token, "GET", f"/api/templates/{self_templates[b]}")
        tpl["fields"][0]["name"] = "Renamed"
        call(RAILS, token, "PUT", f"/api/templates/{self_templates[b]}", {"fields": tpl["fields"]})
        user.request("GET", "/", headers=https)
        steps["start self after fields changed"] = put(self_templates[b])
        changed = steps["start self after fields changed"][2] != steps["start self"][2]
        steps["start self new submission"] = (200, json.dumps(changed), None)
        steps["start self unknown"] = put(999999999)
        status, body, location = put(archived[b])
        steps["start self archived"] = (status, body, (location or "").replace(f"/templates/{archived[b]}", "/templates/ID"))
        anonymous = Browser(base)
        anonymous.request("GET", "/sign_in", headers=https)
        steps["start self signed out"] = put(self_templates[b], anonymous)
    for label in results["rails"]:
        compare(label, {b: results[b][label] for b in BASES})
    a, b = (start_form.stored(self_templates[x]) for x in BASES)
    if a == b:
        print("ok   stored rows (start self)")
    else:
        FAILURES += 1
        print("FAIL stored rows (start self)")
        print(diff(a, b))
    templates.update({f"self-{b}": t for b, t in self_templates.items()})
    templates.update({f"archived-{b}": t for b, t in archived.items()})

    if not os.environ.get("KEEP"):
        for submission_id in cleanup:
            call(RAILS, token, "DELETE", f"/api/submissions/{submission_id}")
        for tid in templates.values():
            call(RAILS, token, "DELETE", f"/api/templates/{tid}?permanently=true")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
