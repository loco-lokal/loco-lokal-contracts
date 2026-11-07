#!/usr/bin/env python3
"""Compares the webhook requests of Rails and the Rust server.

    rust/scripts/parity/webhooks.py TOKEN TEMPLATE_ID

Starts a capture server on HOOK_HOST:HOOK_PORT (default: this machine's en0 address,
port 4599, reachable from the Rails container and the Rust server) and creates a
webhook URL for all events through the Rails container, so its columns are encrypted
by Rails. Each action then runs on one backend at a time; the captured requests are
compared (body with ids, slugs, uuids and times normalized; content type, user agent
and the custom secret header), every signature is verified with the URL's HMAC secret,
and the `webhook_events`/`webhook_attempts` rows are compared. Both backends must run
their job workers. The webhook URL and the created records are deleted at the end.
"""

import hashlib
import hmac
import json
import os
import re
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

from compare import RAILS, RUST
from create import DATABASE_URL, rows
from writes import call, check
from signing import Browser, signature_png

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../.."))
HOST = os.environ.get("HOOK_HOST") or subprocess.run(
    ["ipconfig", "getifaddr", "en0"], capture_output=True, text=True).stdout.strip()
PORT = int(os.environ.get("HOOK_PORT", "4599"))
CAPTURED = []
LOCK = threading.Lock()


class Capture(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0))).decode()
        with LOCK:
            CAPTURED.append({"path": self.path, "headers": dict(self.headers), "body": body})
        status = 500 if self.path.endswith("/fail") else 200
        self.send_response(status)
        self.end_headers()
        self.wfile.write(b"nope" * 40 if status == 500 else b"ok")

    def log_message(self, *args):
        pass


def rails_runner(code):
    out = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", code],
        cwd=ROOT, check=True, capture_output=True, text=True)
    return out.stdout.strip().splitlines()[-1]


def psql(sql):
    subprocess.run(["psql", DATABASE_URL, "-Atc", sql], check=True, capture_output=True)


def wait_for(count, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with LOCK:
            if len(CAPTURED) >= count:
                break
        time.sleep(0.3)
    time.sleep(1.5)  # anything unexpected
    with LOCK:
        result = list(CAPTURED)
        CAPTURED.clear()
    return result


def mask_ip(value):
    """Rails sees requests from the Docker bridge, the Rust server from localhost."""
    if isinstance(value, dict):
        return {k: ("<ip>" if k == "ip" and v else mask_ip(v)) for k, v in value.items()}
    if isinstance(value, list):
        return [mask_ip(v) for v in value]
    return value


def summarize(requests, hmac_secret):
    result = []
    for r in requests:
        headers = {k.lower(): v for k, v in r["headers"].items()}
        ts, _, digest = headers.get("x-docuseal-signature", "").partition(".")
        expected = hmac.new(hmac_secret.encode(), f"{ts}.{r['body']}".encode(), hashlib.sha256).hexdigest()
        result.append({
            "path": r["path"],
            "content_type": headers.get("content-type"),
            "user_agent": headers.get("user-agent"),
            "custom": headers.get("x-custom"),
            "signature_valid": hmac.compare_digest(expected, digest),
            "body": mask_ip(json.loads(r["body"])),
        })
    return sorted(result, key=lambda r: (r["body"]["event_type"], json.dumps(r["body"], sort_keys=True)))


def webhook_rows(url_id):
    events = rows("webhook_events", "webhook_url_id", [url_id])
    attempts = rows("webhook_attempts", "webhook_event_id", [e["id"] for e in events])
    return {
        "events": sorted((e["event_type"], e["record_type"], e["status"]) for e in events),
        "attempts": sorted((a["attempt"], a["response_status_code"], a["response_body"]) for a in attempts),
    }


def compare(label, url_id, secret, actions, expected):
    """Runs `actions(base)` on each backend; returns what each created."""
    results, created = {}, {}
    for name, base in (("rails", RAILS), ("rust", RUST)):
        psql(f"DELETE FROM webhook_attempts WHERE webhook_event_id IN "
             f"(SELECT id FROM webhook_events WHERE webhook_url_id = {url_id})")
        psql(f"DELETE FROM webhook_events WHERE webhook_url_id = {url_id}")
        created[name] = actions(base)
        results[name] = (summarize(wait_for(expected), secret), webhook_rows(url_id))
    check(label, (0, results["rails"][0]), (0, results["rust"][0]))
    check("  webhook rows", (0, results["rails"][1]), (0, results["rust"][1]))
    return created


def main():
    token, t = sys.argv[1], int(sys.argv[2])
    server = HTTPServer(("0.0.0.0", PORT), Capture)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    url_id, secret = rails_runner(
        "w = WebhookUrl.create!(account: Account.order(:id).first, "
        f"url: 'http://{HOST}:{PORT}/hook', events: WebhookUrl::EVENTS, "
        "secret: { 'X-Custom' => 'abc' }); puts [w.id, w.hmac_secret].join(' ')"
    ).split(" ")
    url_id = int(url_id)
    cleanup = {"rails": [], "rust": []}
    try:
        def create(base):
            status, body = call(base, token, "POST", "/api/submissions", {
                "template_id": t, "send_email": False,
                "submitters": [{"role": "First Party", "email": "hook@example.com", "name": "Hook"}]})
            assert status == 200, body
            return body[0]
        made = compare("submission.created", url_id, secret, create, 1)
        for name in made:
            cleanup[name].append(made[name]["submission_id"])

        compare("form.viewed", url_id, secret, lambda base: call(
            base, token, "POST", "/api/submitter_form_views",
            {"submitter_slug": made["rails" if base == RAILS else "rust"]["slug"]}), 1)

        # The signing form (`/s/:slug`): view, complete (single party), decline.
        def signer(base):
            name = "rails" if base == RAILS else "rust"
            browser = Browser(base)
            slug = made[name]["slug"]
            body = browser.request("GET", f"/s/{slug}")[1]
            return browser, slug, re.search(r'data-view-token="([^"]+)"', body).group(1)
        signers = {}

        def view(base):
            signers[base] = signer(base)
            browser, slug, token_ = signers[base]
            return browser.request("POST", f"/s/{slug}/view", [("v", token_)])
        compare("form.viewed (signing form)", url_id, secret, view, 1)

        _, template = call(RAILS, token, "GET", f"/api/templates/{t}")
        uuids = {f["name"]: f["uuid"] for f in template["fields"]}

        def complete(base):
            browser, slug, _ = signers[base]
            upload = browser.request("POST", f"/s/{slug}/upload", [("type", "signature")],
                                     {"file": ("signature.png", "image/png", signature_png())})
            fields = [("_method", "put"), ("authenticity_token", browser.csrf),
                      (f"values[{uuids['Full Name']}]", "Hook"),
                      (f"values[{uuids['Signature']}]", json.loads(upload[1])["uuid"]), ("completed", "true")]
            return browser.request("POST", f"/s/{slug}", fields, files={})
        # `documents` in form.completed and submission.completed are result PDFs (Phase 4b);
        # `audit_log_url` stays null until the audit trail (Phase 4c).
        compare("form.started, form.completed, submission.completed", url_id, secret, complete, 3)

        declining = compare("submission.created (to decline)", url_id, secret, create, 1)
        for name in declining:
            cleanup[name].append(declining[name]["submission_id"])

        def decline(base):
            browser = Browser(base)
            slug = declining["rails" if base == RAILS else "rust"]["slug"]
            browser.request("GET", f"/s/{slug}")
            return browser.request("POST", f"/s/{slug}/decline",
                                   [("authenticity_token", browser.csrf), ("reason", "No")])
        compare("form.declined", url_id, secret, decline, 1)

        compare("submission.archived", url_id, secret, lambda base: call(
            base, token, "DELETE",
            f"/api/submissions/{made['rails' if base == RAILS else 'rust']['submission_id']}"), 1)

        def clone(base):
            status, body = call(base, token, "POST", f"/api/templates/{t}/clone", {"name": "Hook clone"})
            assert status == 200, body
            return body
        clones = compare("template.created (clone)", url_id, secret, clone, 1)

        compare("template.updated", url_id, secret, lambda base: call(
            base, token, "PUT", f"/api/templates/{clones['rails' if base == RAILS else 'rust']['id']}",
            {"name": "Hook clone renamed"}), 1)

        compare("template.archived", url_id, secret, lambda base: call(
            base, token, "DELETE", f"/api/templates/{clones['rails' if base == RAILS else 'rust']['id']}"), 1)
        for name in clones:
            call(RAILS, token, "DELETE", f"/api/templates/{clones[name]['id']}?permanently=true")

        # A failing endpoint: the first attempt and its error row (retries come minutes later).
        psql("UPDATE webhook_urls SET events = '[\"submission.created\"]' WHERE id = " + str(url_id))
        rails_runner(f"WebhookUrl.find({url_id}).update!(url: 'http://{HOST}:{PORT}/fail')")
        made = compare("failing endpoint", url_id, secret, create, 1)
        for name in made:
            cleanup[name].append(made[name]["submission_id"])
    finally:
        psql(f"DELETE FROM webhook_attempts WHERE webhook_event_id IN "
             f"(SELECT id FROM webhook_events WHERE webhook_url_id = {url_id})")
        psql(f"DELETE FROM webhook_events WHERE webhook_url_id = {url_id}")
        psql(f"DELETE FROM webhook_urls WHERE id = {url_id}")
        for name, ids in cleanup.items():
            for sid in ids:
                call(RAILS, token, "DELETE", f"/api/submissions/{sid}?permanently=true")
        server.shutdown()

    from writes import FAILURES
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
