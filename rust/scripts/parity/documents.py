#!/usr/bin/env python3
"""Compares the template builder's document edits on the Rails reference app and the Rust
server.

    rust/scripts/parity/documents.py TOKEN

`documents_seed.rb` (run in the `rails` service after `uploads_seed.rb`'s definitions)
signs in as the uploads user and builds the files. For each case both backends get a new
template from the same uploads, the same extra fields (areas on every page, conditions),
and then the same request:

1. `POST /templates/:id/documents_modify` (`Templates::ModifyDocuments`): pages reordered,
   removed, moved between documents (with `replaced_page`), turned, and redacted (text,
   an image, a form XObject, white boxes); images merged into PDFs (the PDF's paper
   size, the image's own size, a letter-shaped scan) and turned or redacted alone; PDF
   forms whose fields are imported; and the layouts Rails rejects.
2. `GET /templates/:id/documents`: the signed proxy paths.
3. `POST /templates/:id/clone_and_replace` (HTML and JSON): fewer and more files than
   documents, a PDF form, an encrypted PDF, no file.
4. `documents_crop` is not ported: Rust must answer 422 "… is not supported yet".

Responses and stored rows are compared as in uploads.py. The PDFs that each backend's
PDFium writes differ in their bytes, so new documents are compared by their rendered
pages (`pdftoppm`), their text (`pdftotext`, which shows that redacted text is gone),
their form field names and the number of objects that hold the redacted text.
"""

import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import uploads
from compare import RAILS, RUST
from uploads import Session, both, number_uuids, psql_json, rails_runner, report, response, same, scrub, stored

PARITY_DIR = Path(__file__).parent
# poppler's tools with the system libraries, also from a bundled Python.
TOOL_ENV = {k: v for k, v in os.environ.items() if k != "LD_LIBRARY_PATH"}
CREATED = []


def seed():
    code = ("UPLOADS_SEED_DEFINITIONS_ONLY = true\n" + (PARITY_DIR / "uploads_seed.rb").read_text()
            + "\n" + (PARITY_DIR / "documents_seed.rb").read_text())
    return json.loads(rails_runner(code))


def max_attachment_id():
    return psql_json("SELECT coalesce(max(id), 0) FROM active_storage_attachments")


def schema_uuids(template_id):
    template, _ = stored(template_id)
    return [item["attachment_uuid"] for item in template["schema"]]


def set_fields(template_id, fields):
    escaped = json.dumps(fields).replace("'", "''")
    subprocess.run(["psql", uploads.DATABASE_URL, "-qc",
                    f"UPDATE templates SET fields = '{escaped}' WHERE id = {template_id}"], check=True)


def page_fields(template_id):
    """A text field on each page of each document, one spanning the first two documents,
    and a condition on the first field."""
    template, _ = stored(template_id)
    pages = {}
    for row in psql_json(
            "SELECT coalesce(json_agg(r), '[]') FROM (SELECT a.uuid, b.metadata FROM active_storage_attachments a "
            f"JOIN active_storage_blobs b ON b.id = a.blob_id WHERE a.record_type = 'Template' AND a.record_id = {template_id}) r"):
        pages[row["uuid"]] = (json.loads(row["metadata"]).get("pdf") or {}).get("number_of_pages", 1)
    party = template["submitters"][0]["uuid"]
    fields = list(template["fields"])
    first = None
    for doc_index, item in enumerate(template["schema"]):
        uuid = item["attachment_uuid"]
        for page in range(pages.get(uuid, 1)):
            field_uuid = f"00000000-0000-4000-8000-{doc_index:06d}{page:06d}"
            first = first or field_uuid
            fields.append({
                "uuid": field_uuid, "submitter_uuid": party, "name": f"Doc {doc_index} page {page}",
                "type": "text", "required": False,
                "areas": [{"x": 0.1, "y": 0.2, "w": 0.3, "h": 0.05, "attachment_uuid": uuid, "page": page}],
            })
    if len(template["schema"]) > 1:
        fields.append({
            "uuid": "00000000-0000-4000-8000-999999999999", "submitter_uuid": party, "name": "Spanning",
            "type": "text", "conditions": [{"field_uuid": first, "value": "x", "action": "equal"}],
            "areas": [{"x": 0, "y": 0, "w": 1, "h": 1, "attachment_uuid": item["attachment_uuid"], "page": 0}
                      for item in template["schema"][:2]],
        })
    set_fields(template_id, fields)


def new_template(sessions, users, label, files, extra=()):
    ids = uploads.upload_case(sessions, users, f"{label}: upload", files)
    if not (ids["rails"] and ids["rust"]):
        return None
    CREATED.extend(ids.values())
    for name in sessions:
        if extra:
            status, body, _, _ = sessions[name].post_files(f"/templates/{ids[name]}/documents", [], list(extra),
                                                           json_response=True)
            # The builder saves the new schema items with the template.
            template, _ = stored(ids[name])
            schema = template["schema"] + json.loads(body)["schema"]
            escaped = json.dumps(schema).replace("'", "''")
            subprocess.run(["psql", uploads.DATABASE_URL, "-qc",
                            f"UPDATE templates SET schema = '{escaped}' WHERE id = {ids[name]}"], check=True)
        page_fields(ids[name])
    # Documents created after this are written by each backend's own libraries.
    ids["setup"] = {name: max_attachment_id() for name in sessions}
    return ids


def post_json(session, path, body):
    data = json.dumps(body).encode()
    import urllib.request
    import urllib.error
    request = urllib.request.Request(session.base + path, data=data, method="POST")
    request.add_header("Content-Type", "application/json")
    request.add_header("Accept", "application/json")
    request.add_header("X-CSRF-Token", session.csrf)
    try:
        with session.opener.open(request) as res:
            return res.status, res.read().decode(), res.headers.get("Location"), res.headers.get("Content-Type")
    except urllib.error.HTTPError as err:
        return err.code, err.read().decode(), err.headers.get("Location"), err.headers.get("Content-Type")


def resolve(layout, uuids):
    """Replaces document indexes (`"@0"`) with each backend's attachment uuids."""
    def walk(value):
        if isinstance(value, dict):
            return {k: walk(v) for k, v in value.items()}
        if isinstance(value, list):
            return [walk(v) for v in value]
        if isinstance(value, str) and re.fullmatch(r"@\d+", value):
            index = int(value[1:])
            return uuids[index] if index < len(uuids) else value
        return value
    return walk(layout)


def new_pdfs(template_id, after_id):
    return psql_json(
        "SELECT coalesce(json_agg(r ORDER BY r.id), '[]') FROM (SELECT a.id, b.key, b.content_type, b.filename "
        "FROM active_storage_attachments a JOIN active_storage_blobs b ON b.id = a.blob_id "
        f"WHERE a.record_type = 'Template' AND a.record_id = {template_id} AND a.id > {after_id}) r")


def blob_path(key):
    return uploads.ROOT / "rust/tmp/rails-workdir/attachments" / key[:2] / key[2:4] / key


def pdf_facts(key):
    """Form field names and objects with the seed's redacted words, through HexaPDF."""
    out = rails_runner(
        "doc = HexaPDF::Document.new(io: StringIO.new(ActiveStorage::Blob.find_by!(key: "
        f"{json.dumps(key)}).download)); "
        "names = doc.acro_form&.root_fields&.map { |f| f[:T].inspect }; "
        "leaks = doc.each.count { |o| o.is_a?(HexaPDF::Stream) && "
        "(o.stream rescue '').to_s.match?(/Jane|12345678/) }; "
        "puts JSON.generate(fields: names, pages: doc.pages.count, leaks: leaks)")
    return json.loads(out)


def render(path, directory, prefix):
    subprocess.run(["pdftoppm", "-r", "40", "-png", str(path), str(Path(directory) / prefix)], check=True,
                   env=TOOL_ENV)
    return sorted(Path(directory).glob(f"{prefix}*.png"))


def compare_pdfs(label, ids, before):
    rails_docs, rust_docs = new_pdfs(ids["rails"], before["rails"]), new_pdfs(ids["rust"], before["rust"])
    same(f"{label}: new documents", [(d["content_type"], d["filename"]) for d in rails_docs],
         [(d["content_type"], d["filename"]) for d in rust_docs])
    for a, b in zip(rails_docs, rust_docs):
        if a["content_type"] != "application/pdf":
            result = uploads.pixel_difference(blob_path(a["key"]).read_bytes(), blob_path(b["key"]).read_bytes())
            report(result is not None and result[0] <= uploads.MAX_MEAN_DIFF,
                   f"{label}: {a['filename']} pixels {result}")
            continue
        text = [subprocess.run(["pdftotext", "-layout", str(blob_path(d["key"])), "-"], capture_output=True,
                               text=True, env=TOOL_ENV).stdout for d in (a, b)]
        same(f"{label}: {a['filename']} text", text[0], text[1])
        same(f"{label}: {a['filename']} fields and redacted objects", pdf_facts(a["key"]), pdf_facts(b["key"]))
        with tempfile.TemporaryDirectory() as directory:
            pages = [render(blob_path(d["key"]), directory, name) for d, name in ((a, "rails"), (b, "rust"))]
            report(len(pages[0]) == len(pages[1]), f"{label}: {a['filename']} page count")
            for index, (x, y) in enumerate(zip(*pages)):
                result = uploads.pixel_difference(x.read_bytes(), y.read_bytes())
                report(result is not None and result[0] <= uploads.MAX_MEAN_DIFF and result[1] <= uploads.MAX_OFF_PIXELS,
                       f"{label}: {a['filename']} page {index} pixels {result}")


def compare_rows(label, ids, before):
    rails_template, rails_docs = stored(ids["rails"])
    rust_template, rust_docs = stored(ids["rust"])
    same(f"{label}: template row", scrub(number_uuids(rails_template)), scrub(number_uuids(rust_template)))

    def rows(docs, after):
        out = []
        for doc in docs:
            row = {k: v for k, v in doc.items() if k not in ("id", "previews")}
            if doc["id"] > after:
                # Written by each backend's PDFium build, libvips or `image`.
                row = {k: v for k, v in row.items() if k not in ("byte_size", "checksum")}
                row["metadata"] = {k: ("<sha256>" if k == "sha256" else v) for k, v in row["metadata"].items()}
            row["previews"] = [{k: v for k, v in p.items() if k not in ("id", "key")} for p in doc["previews"]]
            out.append(row)
        return out

    same(f"{label}: documents and preview rows", scrub(rows(rails_docs, before["rails"])),
         scrub(rows(rust_docs, before["rust"])))
    for rails_doc, rust_doc in zip(rails_docs, rust_docs):
        if rails_doc["id"] <= before["rails"]:
            continue
        for index, (a, b) in enumerate(zip(rails_doc["previews"], rust_doc["previews"])):
            rails_png, rust_png = uploads.blob_file(a["key"]), uploads.blob_file(b["key"])
            result = uploads.pixel_difference(rails_png, rust_png)
            ok = result is not None and result[0] <= uploads.MAX_MEAN_DIFF and result[1] <= uploads.MAX_OFF_PIXELS
            name = f"{label}: {rails_doc['filename']} preview {index} pixels {result}"
            if not ok and not rails_doc["content_type"].endswith("/pdf") and result and result[1] == 0 \
                    and uploads.png_info(rails_png)["colors"] <= 4:
                # Resized images (libvips' reduce, `image`'s Lanczos3) quantized to 2 bits can
                # get other palettes from the same document; see the README.
                print(f"known {name}: 2-bit palettes of a resized image")
                continue
            report(ok, name)


def modify_case(sessions, ids, label, layout, check_pdfs=True):
    before = {name: max_attachment_id() for name in sessions}
    setup = ids["setup"]
    results = {}
    for name, session in sessions.items():
        body = {"documents": resolve(layout, schema_uuids(ids[name]))} if layout is not None else {}
        results[name] = post_json(session, f"/templates/{ids[name]}/documents_modify", body)
    same(f"{label}: response", masked(response(results["rails"])), masked(response(results["rust"])))
    compare_rows(label, ids, setup)
    if check_pdfs and results["rails"][0] == 200:
        compare_pdfs(label, ids, before)
    return results


def masked(result):
    """The checksums of the files each backend writes."""
    return {**result, "body": re.sub(r'(\\?"sha256\\?": \\?")[^"\\]+', r"\1<sha256>", result["body"] or "")}


def page(doc, number, **options):
    return {"attachment_uuid": f"@{doc}", "page": number, **options}


def documents_index(sessions, ids, label):
    results = {}
    for name, session in sessions.items():
        status, body, _ = session.request("GET", f"/templates/{ids[name]}/documents",
                                          headers={"Accept": "application/json"})
        results[name] = (status, re.sub(r"/file/[^/]+/", "/file/<signed>/", body))
    same(f"{label}: GET documents", results["rails"], results["rust"])


def main():
    data = seed()
    files = {name: uploads.base64.b64decode(blob) for name, blob in data["files"].items()}
    sessions = {"rails": Session(RAILS, data["email"], data["password"]),
                "rust": Session(RUST, data["email"], data["password"])}
    user_id = psql_json(f"SELECT id FROM users WHERE email = '{data['email']}'")
    users = {"rails": user_id, "rust": user_id}

    def pdf(name):
        return ("files[]", name, "application/pdf", files[name])

    def image(name, mime="image/png"):
        return ("files[]", name, mime, files[name])

    # Two PDFs and an image: pages reordered, removed, moved, turned.
    ids = new_template(sessions, users, "pages", [pdf("pages.pdf")],
                       [pdf("links.pdf"), image("color.png")])
    if ids:
        documents_index(sessions, ids, "pages")
        modify_case(sessions, ids, "unchanged layout", [
            {"attachment_uuid": "@0", "pages": [page(0, n) for n in range(4)]},
            {"attachment_uuid": "@1", "pages": [page(1, 0)]},
            {"attachment_uuid": "@2", "pages": [page(2, 0)]}])
        modify_case(sessions, ids, "reorder, remove and turn", [
            {"attachment_uuid": "@0", "pages": [page(0, 3), page(0, 0, rotate=90), page(0, 1, rotate=180)]},
            {"attachment_uuid": "@1", "pages": [page(1, 0, rotate=270)]},
            {"attachment_uuid": "@2", "pages": [page(2, 0)]}])
        modify_case(sessions, ids, "move pages between documents", [
            {"attachment_uuid": "@0", "pages": [page(0, 0), page(1, 0, replaced_page={"attachment_uuid": "@0", "page": 1}),
                                                page(0, 2)]},
            {"attachment_uuid": "@1", "pages": []},
            {"attachment_uuid": "@2", "pages": [page(2, 0)]}])
        documents_index(sessions, ids, "after moving pages")

    # Images merged into a PDF entry (its paper size), and on their own (own size, scans).
    ids = new_template(sessions, users, "images", [pdf("pages.pdf")],
                       [image("photo.png"), image("letter_scan.png"), image("rotated.jpg", "image/jpeg"),
                        image("gray.png")])
    if ids:
        modify_case(sessions, ids, "images into a PDF", [
            {"attachment_uuid": "@0", "pages": [page(0, 0), page(1, 0), page(2, 0, rotate=90), page(0, 1)]},
            {"attachment_uuid": "@1", "pages": []}, {"attachment_uuid": "@2", "pages": []},
            {"attachment_uuid": "@3", "pages": [page(3, 0, rotate=90)]},
            {"attachment_uuid": "@4", "pages": [page(4, 0, redact=[{"x": 0.1, "y": 0.1, "w": 0.3, "h": 0.2}])]}])

    ids = new_template(sessions, users, "image pages", [image("photo.png")],
                       [image("letter_scan.png"), image("gray.png")])
    if ids:
        modify_case(sessions, ids, "images merged without a PDF", [
            {"attachment_uuid": "@0", "pages": [page(0, 0), page(1, 0, rotate=180),
                                                page(2, 0, redact=[{"x": 0.5, "y": 0.5, "w": 0.2, "h": 0.2,
                                                                    "color": "white"}])]},
            {"attachment_uuid": "@1", "pages": []}, {"attachment_uuid": "@2", "pages": []}])

    # Redactions: text, an image, a form XObject, white boxes.
    ids = new_template(sessions, users, "redact", [pdf("image_text.pdf")])
    if ids:
        modify_case(sessions, ids, "redact text, image and form", [
            {"attachment_uuid": "@0", "pages": [
                page(0, 0, redact=[{"x": 0.05, "y": 0.08, "w": 0.6, "h": 0.06},
                                   {"x": 0.1, "y": 0.4, "w": 0.15, "h": 0.1},
                                   {"x": 0.08, "y": 0.6, "w": 0.2, "h": 0.05, "color": "white"}]),
                page(0, 1, rotate=90, redact=[{"x": 0, "y": 0, "w": 0.2, "h": 0.2}])]}])

    # PDF forms: fields imported (and renamed when the same field is imported twice).
    ids = new_template(sessions, users, "forms", [pdf("form.pdf")], [pdf("acro_fields.pdf")])
    if ids:
        modify_case(sessions, ids, "forms reordered and merged", [
            {"attachment_uuid": "@0", "pages": [page(0, 1), page(1, 0), page(0, 0)]},
            {"attachment_uuid": "@1", "pages": [page(1, 2), page(1, 1)]}])
        modify_case(sessions, ids, "form page redacted", [
            {"attachment_uuid": "@0", "pages": [page(0, 0, redact=[{"x": 0, "y": 0, "w": 0.5, "h": 0.2}]),
                                                page(0, 1), page(0, 2)]},
            {"attachment_uuid": "@1", "pages": [page(1, 0), page(1, 1)]}])

    # Layouts Rails rejects.
    ids = new_template(sessions, users, "invalid", [pdf("pages.pdf")], [pdf("links.pdf")])
    if ids:
        both_docs = [{"attachment_uuid": "@1", "pages": [page(1, 0)]}]
        for label, layout in [
            ("no documents parameter", None),
            ("no pages at all", [{"attachment_uuid": "@0", "pages": []}, {"attachment_uuid": "@1"}]),
            ("a document left out", [{"attachment_uuid": "@0", "pages": [page(0, 0)]}]),
            ("a duplicate document", [{"attachment_uuid": "@0", "pages": [page(0, 0)]},
                                      {"attachment_uuid": "@0", "pages": [page(0, 1)]}, *both_docs]),
            ("an unknown document", [{"attachment_uuid": "@0", "pages": [page(0, 0)]}, *both_docs,
                                     {"attachment_uuid": "nope", "pages": [page(0, 1)]}]),
            ("a page out of range", [{"attachment_uuid": "@0", "pages": [page(0, 4)]}, *both_docs]),
            ("a float page", [{"attachment_uuid": "@0", "pages": [page(0, 1.0)]}, *both_docs]),
            ("a page twice", [{"attachment_uuid": "@0", "pages": [page(0, 1), page(0, 1)]}, *both_docs]),
            ("a 45 degree turn", [{"attachment_uuid": "@0", "pages": [page(0, 1, rotate=45)]}, *both_docs]),
            ("a redaction off the page", [{"attachment_uuid": "@0", "pages": [
                page(0, 1, redact=[{"x": 3, "y": 0, "w": 0.1, "h": 0.1}])]}, *both_docs]),
            ("a redaction without a size", [{"attachment_uuid": "@0", "pages": [
                page(0, 1, redact=[{"x": 0.1, "y": 0}])]}, *both_docs]),
            ("a float turn", [{"attachment_uuid": "@0", "pages": [page(0, 1, rotate=90.0)]}, *both_docs]),
        ]:
            modify_case(sessions, ids, f"invalid: {label}", layout, check_pdfs=label == "a float turn")

        # Cropping is not ported.
        uuid = schema_uuids(ids["rust"])[0]
        status, body, _ = sessions["rust"].request(
            "GET", f"/templates/{ids['rust']}/documents_crop?attachment_uuid={uuid}",
            headers={"Accept": "application/json"})
        report(status == 422 and "is not supported yet" in body, "documents_crop: Rust answers 422 not supported yet")
        result = post_json(sessions["rust"], f"/templates/{ids['rust']}/documents_crop",
                           {"attachment_uuid": uuid, "corners": [{"x": 0, "y": 0}] * 4})
        report(result[0] == 422 and "is not supported yet" in result[1],
               "documents_crop create: Rust answers 422 not supported yet")
        statuses = []
        for name, session in sessions.items():
            status, _, _ = session.request("GET", f"/templates/{ids[name]}/documents_crop?attachment_uuid=nope",
                                           headers={"Accept": "application/json"})
            statuses.append(status)
        same("documents_crop with an unknown attachment", statuses[0], statuses[1])

    # Clone and replace.
    ids = new_template(sessions, users, "clone source", [pdf("links.pdf")], [image("color.png")])
    if ids:
        def clone_case(label, files_, json_response=False, fields=()):
            before = {name: psql_json(f"SELECT coalesce(max(id), 0) FROM templates WHERE author_id = {user_id}")
                      for name in sessions}
            results = both(sessions, label, lambda n: f"/templates/{ids[n]}/clone_and_replace",
                           [("form_id", "parity-form"), *fields], files_, json_response)
            clones = {}
            for name in sessions:
                clones[name] = psql_json(f"SELECT max(id) FROM templates WHERE author_id = {user_id}")
                if clones[name] == before[name]:
                    clones[name] = None
            if clones["rails"] and clones["rust"]:
                CREATED.extend(clones.values())
                compare_rows(label, clones, {"rails": 0, "rust": 0})
            elif clones["rails"] or clones["rust"]:
                report(False, f"{label}: only one backend created a template", f"  {clones}")
            return results

        clone_case("clone: one PDF form for two documents", [pdf("form.pdf")])
        clone_case("clone: three files (JSON)", [pdf("acro_fields.pdf"), image("gray.png"), pdf("pages.pdf")],
                   json_response=True)
        clone_case("clone: encrypted PDF", [pdf("encrypted.pdf")])
        clone_case("clone: encrypted PDF (JSON)", [pdf("encrypted.pdf")], json_response=True)
        clone_case("clone: no file", [], json_response=True)

    if CREATED and not os.environ.get("DOCUMENTS_KEEP"):
        rails_runner(f"Template.where(id: {sorted(set(filter(None, CREATED)))}).each(&:destroy)")
    print(f"\n{uploads.FAILURES} failure(s)")
    sys.exit(1 if uploads.FAILURES else 0)


if __name__ == "__main__":
    main()
