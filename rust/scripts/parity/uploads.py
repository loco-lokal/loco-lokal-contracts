#!/usr/bin/env python3
"""Compares template uploads of the Rails reference app and the Rust server.

    rust/scripts/parity/uploads.py TOKEN

`uploads_seed.rb` (run in the `rails` service) creates a user with a known password in
the first user's account, and the files: PDFs with form fields, links, 18 pages and a
password (HexaPDF), and PNG and JPEG images (libvips). Signed in as that user on each
backend, the script then:

1. uploads each file through `POST /templates_upload` (the dashboard's upload form), a
   password-protected PDF without and with its password, two files at once, a text file
   and no file;
2. adds documents to the new templates with `POST /templates/:id/documents` (the
   template builder);
3. sends `POST /api/templates` and the enterprise-only `POST /api/templates/pdf`, which
   Rails answers with 404s.

Responses are compared after normalizing ids, uuids, tokens and origins, and so are the
stored rows: the template (fields extracted from the PDF form, schema, parties, folder),
its `documents` attachments and blobs (metadata, checksums), their `preview_images` and
the search entry. The preview PNGs must have the same size, bit depth, palette size and
chunks; their pixels are compared with a tolerance, since the `PDFium` builds differ and
uploaded images are resized with another filter.

The PDF fixtures cover the branches of `Templates::FindPdfiumAcroFields` (field types,
grouping of widgets, export values and `/Opt`, detached widgets, rotated and offset pages)
and of `Templates::BuildPdfiumAnnotations` (the link limit, other actions and URIs).
`UPLOADS_EXTRA_PDFS` (paths separated by `:`) adds uploads of other PDFs, such as forms
found in the wild.

Word documents, ZIP archives and uploads from a URL are not ported: Rust must answer them
with 422 "… is not supported yet". The created templates are deleted at the end.
"""

import base64
import io
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid as uuidlib
import zipfile
from pathlib import Path

from compare import RAILS, RUST, diff
from signing import Browser, json_text, page
from writes import scrub

ROOT = Path(__file__).resolve().parents[3]
DATABASE_URL = os.environ.get(
    "DATABASE_URL", "postgres://docuseal:docuseal_dev@127.0.0.1:5435/docuseal"
)
FAILURES = 0
CREATED = []
DOCX = "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
# Mean absolute RGB difference of preview pixels, and the share of pixels off by more
# than 64 in a channel, that still count as the same picture.
MAX_MEAN_DIFF = 3.0
MAX_OFF_PIXELS = 0.01


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
    a = json.dumps(a, indent=1, ensure_ascii=False, sort_keys=False)
    b = json.dumps(b, indent=1, ensure_ascii=False, sort_keys=False)
    report(a == b, label, None if a == b else diff(a, b))


def rails_runner(code):
    out = subprocess.run(
        ["docker", "compose", "-f", "compose.rust-dev.yml", "exec", "-T", "-w", "/app", "rails",
         "bin/rails", "runner", code],
        cwd=ROOT, check=True, capture_output=True, text=True)
    return out.stdout.strip().splitlines()[-1]


def psql_json(sql):
    out = subprocess.run(["psql", DATABASE_URL, "-Atc", sql], check=True, capture_output=True, text=True)
    return json.loads(out.stdout.strip() or "null")


def multipart(fields, files):
    boundary = uuidlib.uuid4().hex
    parts = []
    for name, value in fields:
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode())
    for name, filename, mime, data in files:
        parts.append(
            f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
            f"Content-Type: {mime}\r\n\r\n".encode() + data + b"\r\n"
        )
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


class Session(Browser):
    """A signed-in browser that can post multipart forms with several files."""

    def __init__(self, base, email, password):
        super().__init__(base)
        self.request("GET", "/sign_in")
        status, _, _ = self.request("POST", "/sign_in", [
            ("authenticity_token", self.csrf), ("user[email]", email), ("user[password]", password)])
        if status not in (302, 303):
            sys.exit(f"{base}: signing in failed ({status})")
        self.request("GET", "/")

    def post_files(self, path, fields, files, json_response=False):
        data, content_type = multipart([("authenticity_token", self.csrf), *fields], files)
        request = urllib.request.Request(self.base + path, data=data, method="POST")
        request.add_header("Content-Type", content_type)
        request.add_header("User-Agent", "parity-uploads")
        if json_response:
            request.add_header("Accept", "application/json")
            request.add_header("X-CSRF-Token", self.csrf)
        try:
            with self.opener.open(request) as response:
                return (response.status, response.read().decode(), response.headers.get("Location"),
                        response.headers.get("Content-Type"))
        except urllib.error.HTTPError as err:
            return err.code, err.read().decode(), err.headers.get("Location"), err.headers.get("Content-Type")


def template_id(location):
    match = re.search(r"/templates/(\d+)/edit$", location or "")
    return int(match.group(1)) if match else None


def latest_template(user_id):
    return psql_json(f"SELECT max(id) FROM templates WHERE author_id = {user_id}")


def stored(template_id):
    """The template, its documents and previews as rows, with volatile values scrubbed."""
    template = psql_json(
        "SELECT row_to_json(t) FROM (SELECT t.name, t.fields, t.schema, t.submitters, t.source, "
        "t.preferences, t.shared_link, t.external_id, t.archived_at, t.variables_schema, "
        "t.author_id, f.name AS folder, p.name AS parent_folder "
        "FROM templates t JOIN template_folders f ON f.id = t.folder_id "
        f"LEFT JOIN template_folders p ON p.id = f.parent_folder_id WHERE t.id = {template_id}) t")
    for key in ("fields", "schema", "submitters", "preferences", "variables_schema"):
        if isinstance(template.get(key), str):
            template[key] = json.loads(template[key])
    documents = psql_json(
        "SELECT coalesce(json_agg(d ORDER BY d.id), '[]') FROM (SELECT a.id, a.name, b.filename, "
        "b.content_type, b.metadata, b.byte_size, b.checksum, b.service_name FROM "
        "active_storage_attachments a JOIN active_storage_blobs b ON b.id = a.blob_id "
        f"WHERE a.record_type = 'Template' AND a.record_id = {template_id}) d")
    for document in documents:
        document["metadata"] = json.loads(document["metadata"])
        document["previews"] = psql_json(
            "SELECT coalesce(json_agg(p ORDER BY p.id), '[]') FROM (SELECT a.id, a.name, b.key, "
            "b.filename, b.content_type, b.metadata, b.service_name FROM active_storage_attachments a "
            "JOIN active_storage_blobs b ON b.id = a.blob_id WHERE a.record_type = "
            f"'ActiveStorage::Attachment' AND a.record_id = {document['id']}) p")
        for preview in document["previews"]:
            preview["metadata"] = json.loads(preview["metadata"])
    return template, documents


def blob_file(key):
    """A stored file of the shared disk service, read through Rails."""
    return base64.b64decode(rails_runner(
        f"puts Base64.strict_encode64(ActiveStorage::Blob.find_by!(key: {json.dumps(key)}).download)"))


def png_info(data):
    chunks, pos = [], 8
    while pos + 8 <= len(data):
        length = int.from_bytes(data[pos:pos + 4], "big")
        chunks.append((data[pos + 4:pos + 8].decode(), data[pos + 8:pos + 8 + length]))
        pos += 12 + length
    names = [name for name, _ in chunks if name != "IDAT"]
    body = dict(chunks)
    return {
        "chunks": names,
        "ihdr": body["IHDR"].hex(),
        "colors": len(body.get("PLTE", b"")) // 3,
        "trns": len(body.get("tRNS", b"")),
        "phys": body.get("pHYs", b"").hex(),
        "exif": body.get("eXIf", b"").hex(),
    }


def pixel_difference(a, b):
    from PIL import Image, ImageChops

    first = Image.open(io.BytesIO(a)).convert("RGB")
    second = Image.open(io.BytesIO(b)).convert("RGB")
    if first.size != second.size:
        return None
    delta = ImageChops.difference(first, second)
    histogram = delta.convert("L").histogram()
    pixels = first.size[0] * first.size[1]
    mean = sum(i * n for i, n in enumerate(histogram)) / pixels
    off = sum(histogram[65:]) / pixels
    return mean, off


UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


def number_uuids(value):
    """Replaces each uuid with its order of first appearance, so that references (an
    area's `option_uuid`, `attachment_uuid`, `submitter_uuid`) are compared too."""
    numbers = {}

    def walk(item):
        if isinstance(item, dict):
            return {k: walk(v) for k, v in item.items()}
        if isinstance(item, list):
            return [walk(v) for v in item]
        if isinstance(item, str):
            return UUID.sub(lambda m: f"<uuid {numbers.setdefault(m.group(0), len(numbers))}>", item)
        return item

    return walk(value)


def compare_templates(label, rails_id, rust_id, encrypted=False):
    rails_template, rails_docs = stored(rails_id)
    rust_template, rust_docs = stored(rust_id)
    same(f"{label}: template row", scrub(number_uuids(rails_template)), scrub(number_uuids(rust_template)))

    def rows(docs):
        out = []
        for doc in docs:
            row = {k: v for k, v in doc.items() if k not in ("id", "previews")}
            if encrypted:
                # The decrypted copy is written by each backend's PDFium build.
                row = {k: v for k, v in row.items() if k not in ("byte_size", "checksum")}
                row["metadata"] = {k: ("<sha256>" if k == "sha256" else v) for k, v in row["metadata"].items()}
            row["previews"] = [{k: v for k, v in p.items() if k not in ("id", "key")} for p in doc["previews"]]
            out.append(row)
        return out

    same(f"{label}: documents and preview rows", scrub(rows(rails_docs)), scrub(rows(rust_docs)))
    for rails_doc, rust_doc in zip(rails_docs, rust_docs):
        for index, (a, b) in enumerate(zip(rails_doc["previews"], rust_doc["previews"])):
            rails_png, rust_png = blob_file(a["key"]), blob_file(b["key"])
            rails_info, rust_info = png_info(rails_png), png_info(rust_png)
            if not rails_doc["content_type"].endswith("/pdf"):
                # Resized uploads differ in their in-between colors, so the palettes may
                # have other sizes (up to the same bit depth).
                print(f"     palette colors: rails {rails_info['colors']}, rust {rust_info['colors']}")
                rails_info["colors"] = rust_info["colors"] = None
            same(f"{label}: {rails_doc['filename']} preview {index} PNG structure", rails_info, rust_info)
            result = pixel_difference(rails_png, rust_png)
            if result is None:
                report(False, f"{label}: {rails_doc['filename']} preview {index} pixels", "  sizes differ")
            else:
                mean, off = result
                report(mean <= MAX_MEAN_DIFF and off <= MAX_OFF_PIXELS,
                       f"{label}: {rails_doc['filename']} preview {index} pixels "
                       f"(mean difference {mean:.2f}, {off:.2%} off)")


def search_entry(template_id):
    deadline = time.time() + 20
    while time.time() < deadline:
        entry = psql_json(
            "SELECT row_to_json(e) FROM (SELECT tsvector::text AS tsvector, ngram::text AS ngram "
            f"FROM search_entries WHERE record_type = 'Template' AND record_id = {template_id}) e")
        if entry:
            return entry
        time.sleep(0.5)
    return None


def response(result):
    status, body, location, content_type = result
    if "html" in (content_type or ""):
        body = page(body)
    else:
        try:
            body = json.dumps(number_uuids(volatile(json.loads(body))), indent=1, ensure_ascii=False)
        except ValueError:
            pass
        body = json_text(body)
    location = page(location) if location else None
    location = re.sub(r"/templates/\d+/", "/templates/ID/", location or "") or None
    return {"status": status, "location": location, "type": content_type if status == 200 else None,
            "body": body}


def volatile(value):
    """Record ids and signed keys, which differ between the backends' records."""
    if isinstance(value, dict):
        return {k: "<volatile>" if k in ("blob_id", "record_id", "signed_key") else volatile(v)
                for k, v in value.items()}
    if isinstance(value, list):
        return [volatile(v) for v in value]
    return value


def both(sessions, label, path, fields=(), files=(), json_response=False):
    results = {}
    for name, session in sessions.items():
        results[name] = session.post_files(path(name) if callable(path) else path, list(fields), list(files),
                                           json_response)
    same(f"{label}: response", response(results["rails"]), response(results["rust"]))
    return results


def upload_case(sessions, users, label, files, fields=(), encrypted=False):
    before = {name: latest_template(users[name]) for name in sessions}
    results = both(sessions, label, "/templates_upload",
                   [("form_id", "parity-form"), ("folder_name", "Parity / Uploads"), *fields], files)
    ids = {}
    for name in sessions:
        ids[name] = template_id(results[name][2])
        latest = latest_template(users[name])
        if ids[name] is None and latest != before[name]:
            ids[name] = latest
        if ids[name]:
            CREATED.append(ids[name])
    if ids["rails"] and ids["rust"]:
        compare_templates(label, ids["rails"], ids["rust"], encrypted)
    elif ids["rails"] or ids["rust"]:
        report(False, f"{label}: only one backend created a template", f"  {ids}")
    return ids


def unsupported_case(sessions, label, files, fields=()):
    """Rails handles these; the Rust server must refuse them with a 422."""
    rust = sessions["rust"].post_files("/templates_upload", [("form_id", "f"), *fields], files)
    ok = rust[0] == 422 and "is not supported yet" in rust[1]
    report(ok, f"{label}: Rust answers 422 not supported yet", None if ok else f"  {rust[:2]}")


def main():
    token = sys.argv[1]
    seed = json.loads(rails_runner((Path(__file__).parent / "uploads_seed.rb").read_text()))
    files = {name: base64.b64decode(data) for name, data in seed["files"].items()}
    sessions = {"rails": Session(RAILS, seed["email"], seed["password"]),
                "rust": Session(RUST, seed["email"], seed["password"])}
    user_id = psql_json(f"SELECT id FROM users WHERE email = '{seed['email']}'")
    users = {"rails": user_id, "rust": user_id}

    def pdf(name):
        return ("files[]", name, "application/pdf", files[name])

    def image(name, mime):
        return ("files[]", name, mime, files[name])

    created = {}
    created["form"] = upload_case(sessions, users, "PDF form", [pdf("form.pdf")])
    upload_case(sessions, users, "PDF links", [pdf("links.pdf")])
    upload_case(sessions, users, "AcroForm fields", [pdf("acro_fields.pdf")])
    upload_case(sessions, users, "widgets without a form", [pdf("orphan_widgets.pdf")])
    upload_case(sessions, users, "widgets without a form, with a form XObject", [pdf("orphan_xobject.pdf")])
    upload_case(sessions, users, "260 links and other actions", [pdf("many_links.pdf")])
    upload_case(sessions, users, "two PDF forms", [pdf("form.pdf"), pdf("acro_fields.pdf")])
    for path in filter(None, os.environ.get("UPLOADS_EXTRA_PDFS", "").split(os.pathsep)):
        upload_case(sessions, users, f"extra PDF {Path(path).name}",
                    [("files[]", Path(path).name, "application/pdf", Path(path).read_bytes())])
    upload_case(sessions, users, "18-page PDF", [pdf("pages.pdf")])
    upload_case(sessions, users, "PNG", [image("color.png", "image/png")])
    upload_case(sessions, users, "grayscale PNG", [image("gray.png", "image/png")])
    upload_case(sessions, users, "rotated JPEG", [image("rotated.jpg", "image/jpeg")])
    upload_case(sessions, users, "small PNG", [image("wide.png", "image/png")])
    upload_case(sessions, users, "PNG sent as JPEG", [image("wide.png", "image/jpeg")])
    created["two"] = upload_case(sessions, users, "two files",
                                 [pdf("links.pdf"), image("color.png", "image/png")])
    upload_case(sessions, users, "encrypted PDF", [pdf("encrypted.pdf")])
    upload_case(sessions, users, "encrypted PDF with password", [pdf("encrypted.pdf")],
                [("password", "secret")], encrypted=True)
    upload_case(sessions, users, "text file", [("files[]", "notes.txt", "text/plain", b"hello")])
    upload_case(sessions, users, "no file", [])

    for name, session in sessions.items():
        status, body, _ = session.request("GET", "/")
        report("Unable to upload file" in body,
               f"{name}: dashboard shows the upload error after a failed upload")

    if all(created["form"].values()):
        ids = created["form"]
        both(sessions, "documents: PDF and PNG", lambda n: f"/templates/{ids[n]}/documents",
             files=[pdf("form.pdf"), image("color.png", "image/png")], json_response=True)
        compare_templates("documents: added", ids["rails"], ids["rust"])
        both(sessions, "documents: AcroForm fields and links", lambda n: f"/templates/{ids[n]}/documents",
             files=[pdf("acro_fields.pdf"), pdf("many_links.pdf")], json_response=True)
        compare_templates("documents: AcroForm fields and links added", ids["rails"], ids["rust"])
        both(sessions, "documents: missing file", lambda n: f"/templates/{ids[n]}/documents",
             json_response=True)
        both(sessions, "documents: encrypted PDF", lambda n: f"/templates/{ids[n]}/documents",
             files=[pdf("encrypted.pdf")], json_response=True)
        same("search entry", search_entry(ids["rails"]), search_entry(ids["rust"]))

    docx = (ROOT / "spec/fixtures/fieldtags.docx").read_bytes()
    unsupported_case(sessions, "Word document", [("files[]", "fieldtags.docx", DOCX, docx)])
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w") as zf:
        zf.writestr("links.pdf", files["links.pdf"])
    unsupported_case(sessions, "ZIP archive", [("files[]", "docs.zip", "application/zip", archive.getvalue())])
    unsupported_case(sessions, "URL upload", [], [("url", "https://example.com/a.pdf")])
    status, body, _ = sessions["rust"].request("GET", "/templates_upload/new?url=https%3A%2F%2Fexample.com%2Fa.pdf")
    report(status == 422 and "is not supported yet" in body, "URL upload page: Rust answers 422 not supported yet")

    from writes import call
    for method, path in (("POST", "/api/templates"), ("POST", "/api/templates/pdf"),
                         ("POST", "/api/templates/docx"), ("PUT", "/api/templates")):
        rails = call(RAILS, token, method, path, {})
        rust = call(RUST, token, method, path, {})
        same(f"{method} {path}", list(rails), list(rust))

    if CREATED:
        rails_runner(f"Template.where(id: {sorted(set(CREATED))}).each(&:destroy)")
    print(f"\n{FAILURES} failure(s)")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
