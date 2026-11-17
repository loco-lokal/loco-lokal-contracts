#!/usr/bin/env python3
"""Compares document previews and page endpoints of the Rails reference app and the Rust
server.

    rust/scripts/parity/previews.py

`previews_seed.rb` (run in the `rails` service) creates a user with a known password in
the first user's account, a template with an 18-page PDF (a filled form field, text in a
form XObject, an image, a rotated page) and a second document, and a template of another
account. Rails stores `preview_images` for the first 16 pages only. Signed in as that user
on each backend, the script then compares:

1. `/templates/:id/preview` (the read-only builder): the page, with the `data-template`
   JSON (`Templates.serialize_for_builder`), for the template, an archived copy, another
   account's template, a missing one, and signed out;
2. `/templates/:id/documents_page_objects`: the JSON bodies byte for byte (Rails' float
   format included) for plain, rotated and form pages, and the errors;
3. `/preview/:signed_key/:page`: redirects to stored previews, signed uuids, bad keys,
   pages out of range, and the lazy JPEG preview of pages 16 and 17. Each backend
   renders the page once; the stored rows (blob metadata, checksum, size) and the JPEG
   bytes must be the same, then both redirect to the stored preview.

The preview rows created here and the archived copy are deleted at the end.
"""

import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

from compare import RAILS, RUST, diff
from signing import Browser, page

ROOT = Path(__file__).resolve().parents[3]
DATABASE_URL = os.environ.get(
    "DATABASE_URL", "postgres://docuseal:docuseal_dev@127.0.0.1:5435/docuseal"
)
FAILURES = 0
SIGNED = re.compile(r"eyJfcmFpbHMi[A-Za-z0-9=_-]*--[0-9a-f]+")


def report(ok, label, detail=None):
    global FAILURES
    if ok:
        print(f"ok   {label}")
    else:
        FAILURES += 1
        print(f"FAIL {label}")
        if detail:
            print(detail)


def same(label, a, b):
    a = a if isinstance(a, str) else json.dumps(a, indent=1, ensure_ascii=False)
    b = b if isinstance(b, str) else json.dumps(b, indent=1, ensure_ascii=False)
    report(a == b, label, None if a == b else diff(a, b))


def rails_runner(code):
    out = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", code],
        cwd=ROOT, check=True, capture_output=True, text=True)
    lines = out.stdout.strip().splitlines()
    return lines[-1] if lines else ""


def seed():
    script = Path(__file__).with_name("previews_seed.rb")
    subprocess.run(["docker", "compose", "-f", "compose.rust-dev.yml", "cp", str(script),
                    "rails:/tmp/previews_seed.rb"], cwd=ROOT, check=True)
    return json.loads(rails_runner("load '/tmp/previews_seed.rb'"))


def psql_json(sql):
    out = subprocess.run(["psql", DATABASE_URL, "-Atc", sql], check=True, capture_output=True, text=True)
    return json.loads(out.stdout or "null")


class Session(Browser):
    def __init__(self, base, email=None, password=None):
        super().__init__(base)
        if email:
            self.request("GET", "/sign_in")
            status, _, _ = self.request("POST", "/sign_in", [
                ("authenticity_token", self.csrf), ("user[email]", email), ("user[password]", password)])
            if status not in (302, 303):
                sys.exit(f"{base}: signing in failed ({status})")
            self.request("GET", "/")

    def get(self, path, headers=None):
        request = urllib.request.Request(self.base + path, method="GET")
        request.add_header("User-Agent", "parity-previews")
        for key, value in (headers or {}).items():
            request.add_header(key, value)
        try:
            with self.opener.open(request) as response:
                return response.status, response.read(), response.headers
        except urllib.error.HTTPError as err:
            return err.code, err.read(), err.headers


def normalize(text):
    return SIGNED.sub("SIGNED", page(text))


def location(headers):
    value = headers.get("Location")
    return value and re.sub(r"/disk/[^/]+/", "/disk/T/", re.sub(r"https?://[^/]+", "ORIGIN", value))


def compare_get(label, sessions, path, body=normalize, headers=None, content_type=False):
    results = {name: s.get(path, headers) for name, s in sessions.items()}
    (s1, b1, h1), (s2, b2, h2) = results["rails"], results["rust"]
    a = body(b1.decode(errors="replace")) if body else ""
    b = body(b2.decode(errors="replace")) if body else ""
    ok = s1 == s2 and location(h1) == location(h2) and a == b
    if content_type:
        ok = ok and h1.get("Content-Type") == h2.get("Content-Type")
    detail = None
    if not ok:
        detail = (f"  rails {s1} {location(h1)} {h1.get('Content-Type')}\n"
                  f"  rust  {s2} {location(h2)} {h2.get('Content-Type')}")
        if a != b:
            detail += "\n" + diff(a, b)
    report(ok, f"{label} [{s1}]", detail)
    return results


def template_data(html):
    match = re.search(r'data-template="([^"]*)"', html)
    if not match:
        return None
    import html as htmllib
    data = json.loads(htmllib.unescape(match.group(1)))
    for document in data.get("documents", []):
        document["signed_key"] = "SIGNED" if document.get("signed_key") else None
        for image in document.get("preview_images", []):
            image["url"] = re.sub(r"/disk/[^/]+/", "/disk/T/", re.sub(r"https?://[^/]+", "ORIGIN", image["url"]))
    return data


def builder_pages(sessions, template_id, label):
    results = compare_get(f"preview page{label}", sessions, f"/templates/{template_id}/preview")
    a, b = (template_data(results[n][1].decode()) for n in ("rails", "rust"))
    same(f"preview page{label} data-template", a, b)
    return a


def preview_rows(attachment_id, filename):
    return psql_json(f"""
      SELECT coalesce(json_agg(json_build_object(
        'id', a.id, 'name', a.name, 'record_type', a.record_type, 'record_id', a.record_id,
        'filename', b.filename, 'content_type', b.content_type, 'metadata', b.metadata,
        'service_name', b.service_name, 'byte_size', b.byte_size, 'checksum', b.checksum,
        'has_uuid', b.uuid IS NOT NULL) ORDER BY a.id), '[]')
      FROM active_storage_attachments a JOIN active_storage_blobs b ON b.id = a.blob_id
      WHERE a.record_type = 'ActiveStorage::Attachment' AND a.record_id = {attachment_id}
        AND a.name = 'preview_images' AND b.filename = '{filename}'""")


def purge(ids):
    if ids:
        rails_runner(f"ActiveStorage::Attachment.where(id: {list(ids)}).each(&:purge)")


def fetch(url):
    with urllib.request.urlopen(url) as response:
        return response.read()


def lazy_preview(sessions, key, attachment_id, index):
    """Each backend renders the page once; rows and bytes must match."""
    filename = f"{index}.jpg"
    purge([r["id"] for r in preview_rows(attachment_id, filename)])
    stored = {}
    for name, base in (("rails", RAILS), ("rust", RUST)):
        status, _, headers = sessions[name].get(f"/preview/{key}/{filename}")
        rows = preview_rows(attachment_id, filename)
        target = headers.get("Location")
        data = fetch(target) if status == 302 and target else b""
        stored[name] = (status, rows, data, target)
        if name == "rails":
            purge([r["id"] for r in rows])

    (s1, rows1, data1, loc1), (s2, rows2, data2, loc2) = stored["rails"], stored["rust"]
    strip = lambda rows: [{k: v for k, v in r.items() if k != "id"} for r in rows]
    report(s1 == s2 == 302 and location({"Location": loc1}) == location({"Location": loc2}),
           f"lazy preview {index}: redirect [{s1}, {s2}]")
    same(f"lazy preview {index}: stored rows", strip(rows1), strip(rows2))
    report(data1 == data2 and data1.startswith(b"\xff\xd8"),
           f"lazy preview {index}: same JPEG bytes ({len(data1)} / {len(data2)} bytes)")

    # Both now redirect to the preview the Rust server stored.
    results = compare_get(f"lazy preview {index}: stored", sessions, f"/preview/{key}/{filename}", body=None)
    report(len(preview_rows(attachment_id, filename)) == 1, f"lazy preview {index}: stored once")
    return [r["id"] for r in rows2], results


def main():
    data = seed()
    template_id, other_id = data["template_id"], data["other_template_id"]
    sessions = {"rails": Session(RAILS, data["email"], data["password"]),
                "rust": Session(RUST, data["email"], data["password"])}
    anonymous = {"rails": Session(RAILS), "rust": Session(RUST)}

    documents = psql_json(f"""
      SELECT json_agg(json_build_object('id', id, 'uuid', uuid) ORDER BY id)
      FROM active_storage_attachments WHERE record_type = 'Template' AND record_id = {template_id}""")
    first, second = documents[0], documents[1]
    created = []
    archived_id = None
    try:
        builder_pages(sessions, template_id, "")
        key = rails_runner(f"puts ActiveStorage::Attachment.find({first['id']}).signed_key")

        archived_id = int(rails_runner(
            f"t = Template.find({template_id}); c = Templates::Clone.call(t, author: t.author); "
            "c.archived_at = Time.current; c.save!; puts c.id"))
        builder_pages(sessions, archived_id, " (archived)")
        compare_get("preview page of another account", sessions, f"/templates/{other_id}/preview")
        compare_get("preview page of a missing template", sessions, "/templates/999999999/preview")
        compare_get("preview page signed out", anonymous, f"/templates/{template_id}/preview")
        status, body, _ = sessions["rust"].get(f"/templates/{template_id}/preview",
                                               {"User-Agent": "Hotwire Native iOS"})
        report(status == 422 and b"not supported yet" in body,
               f"preview page in the Hotwire Native app: Rust answers 422 [{status}]")

        raw = lambda text: text
        objects = f"/templates/{template_id}/documents_page_objects"
        for label, query in [
            ("form page", f"attachment_uuid={first['uuid']}&page=0"),
            ("rotated page", f"attachment_uuid={first['uuid']}&page=1"),
            ("plain page", f"attachment_uuid={first['uuid']}&page=2"),
            ("last page", f"attachment_uuid={first['uuid']}&page=17"),
            ("no page param", f"attachment_uuid={first['uuid']}"),
            ("second document", f"attachment_uuid={second['uuid']}&page=0"),
            ("page out of range", f"attachment_uuid={first['uuid']}&page=99"),
            ("unknown attachment", "attachment_uuid=nope&page=0"),
        ]:
            compare_get(f"page objects: {label}", sessions, f"{objects}?{query}", body=raw, content_type=True)
        compare_get("page objects: another account", sessions,
                    f"/templates/{other_id}/documents_page_objects?attachment_uuid={first['uuid']}&page=0")
        compare_get("page objects: signed out", anonymous, f"{objects}?attachment_uuid={first['uuid']}&page=0")

        signed_uuid = rails_runner(
            f"puts ApplicationRecord.signed_id_verifier.generate('{first['uuid']}', expires_in: 6.hours, "
            "purpose: :attachment)")
        wrong_purpose = rails_runner(
            f"puts ApplicationRecord.signed_id_verifier.generate([{first['id']}, '{first['uuid']}'])")
        for label, path in [
            ("stored png", f"/preview/{key}/0.jpg"),
            ("stored png, no format", f"/preview/{key}/3"),
            ("non-numeric page", f"/preview/{key}/abc.jpg"),
            ("signed uuid", f"/preview/{signed_uuid}/2.jpg"),
            ("bad key", "/preview/nope/1.jpg"),
            ("bad key, png", "/preview/nope/1.png"),
            ("bad key, no format", "/preview/nope/1"),
            ("key without the purpose", f"/preview/{wrong_purpose}/1.jpg"),
            ("page out of range", f"/preview/{key}/99.jpg"),
        ]:
            compare_get(f"preview image: {label}", anonymous, path, content_type=True)

        for index in (16, 17):
            ids, _ = lazy_preview(anonymous, key, first["id"], index)
            created += ids
        compare_get("preview image: lazy preview through a signed uuid", anonymous,
                    f"/preview/{signed_uuid}/16.jpg", body=None)
        builder_pages(sessions, template_id, " (with lazy previews)")
    finally:
        purge(created)
        if archived_id:
            rails_runner(f"Template.find({archived_id}).destroy")

    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
