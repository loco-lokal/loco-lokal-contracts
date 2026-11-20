# Rust backend

A rewrite of the Rails backend in Rust. Until it reaches parity, Rails stays the production app and the reference implementation. The Rust server uses the same PostgreSQL schema and stored files; PostgreSQL is the only supported database.

## Toolchain (macOS)

```sh
brew install rustup cargo-nextest bacon cargo-deny sqlx-cli
rustup-init -y --no-modify-path        # or: rustup default stable
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"   # add to ~/.zshrc
```

`rust-toolchain.toml` pins the compiler version. rustup installs it on first use.

## Native libraries

```sh
rust/scripts/fetch-pdfium.sh   # pinned, checksum-verified libpdfium -> rust/vendor/pdfium/lib
rust/scripts/fetch-onnx.sh     # field detection model and ONNX Runtime 1.24.4 -> rust/vendor/onnx
```

Set `PDFIUM_LIB_DIR` to use a libpdfium from somewhere else. `ONNX_MODEL_PATH` and `ORT_DYLIB_PATH` override the model and the ONNX Runtime library. Without them, the server uses `$APP_ROOT/tmp/model.onnx` (where the Docker image puts the model), then `rust/vendor/onnx`, and the system's `libonnxruntime.so.1` (Alpine's `onnxruntime` package in the Docker image). ONNX Runtime is loaded when field detection first runs, so the server starts without it.

## Dev services

```sh
docker compose -f compose.rust-dev.yml up -d --build
```

| Service | Address | Purpose |
| --- | --- | --- |
| postgres | `localhost:5435` (docuseal / docuseal_dev) | Shared dev database |
| rails | `http://localhost:3000` | Reference app. Runs the Rails migrations on boot |
| mailpit | `http://localhost:8025`, SMTP `:1025` | Captured mail |

Rails owns the schema until cutover. After Rails migrations change, rebuild the `rails` service, then refresh the snapshots:

```sh
rust/scripts/dump-schema.sh                  # rust/schema/baseline.sql (used by CI)
cd rust && cargo sqlx prepare --workspace    # rust/.sqlx offline query data
```

Also update `EXPECTED_SCHEMA_VERSION` in `crates/db/src/lib.rs`.

Tables that only the Rust backend uses (the `rust_jobs` queue and `rust_sessions`) live in `migrations/`. The server applies them at boot, and they are kept out of `baseline.sql`. Apply them to the dev database by hand when query macros need them:

```sh
cd rust && sqlx migrate run --source migrations
```

## Golden fixtures

`crates/rails-compat/tests/fixtures/golden.json` holds values that Rails produced with the dev `SECRET_KEY_BASE`: encrypted columns, signed ids, an encrypted SMTP config, and a bcrypt hash. Tests check that Rust reads and writes the same formats. `crates/pdf/tests/fixtures/upload-golden.json` holds the form fields and links Rails extracts from the `upload-*.pdf` fixtures (the PDFs of `uploads_seed.rb`). To regenerate the files, with the `rails` service running:

```sh
rust/scripts/generate-golden.sh
```

## Running

```sh
cd rust
export DATABASE_URL=postgresql://docuseal:docuseal_dev@localhost:5435/docuseal
export SECRET_KEY_BASE=rust-dev-secret-key-base-not-for-production-use-0123456789abcdef0123456789abcdef
cargo run -p loco-server         # http://127.0.0.1:8080/up
bacon clippy                     # watch mode
```

The server reads the same variables as Rails:

- `DATABASE_URL`, or `DATABASE_HOST`/`_PORT`/`_USER`/`_PASSWORD`/`_NAME`
- `SECRET_KEY_BASE`, or `$WORKDIR/docuseal.env`
- `ENCRYPTION_SECRET`, `WORKDIR`, `APP_URL`

- Storage: `S3_ATTACHMENTS_BUCKET`/`AWS_*`/`S3_ENDPOINT`, `GCS_*`, `AZURE_*`. Without any of these, files go to `$WORKDIR/attachments`; in production the stored storage settings apply instead.
- Mail: `SMTP_*`, including `SMTP_FROM`. Without these, the stored SMTP settings apply; with neither, mail is discarded.
- `RAILS_ENV=production`, which turns off template and asset-manifest reloading.
- `PDF_FORMAT` (`pdf/a-3b` for PDF/A-3b result documents and audit trails) and `CERTS` (signing certificates that replace the accounts' own).

It also reads these, which Rails doesn't use:

- `PORT` (default 8080) and `BIND_HOST` (default 127.0.0.1)
- `DATABASE_POOL`
- `JOB_CONCURRENCY` (default 5; 0 disables the in-process job worker)
- `APP_ROOT`: the directory with `app/`, `config/`, `public/`, and `rust/templates`; defaults to this checkout

To send development mail to Mailpit, add `SMTP_ADDRESS=localhost SMTP_PORT=1025 SMTP_ENABLE_STARTTLS=false`.

Pages are minijinja ports of `app/views` in `rust/templates/`. They use the Shakapacker asset build from `public/packs/manifest.json`, the icons in `app/views/icons/`, and the translations in `config/locales/`. Build the assets once (and again after JavaScript or CSS changes) with Node and Yarn; no Ruby is needed:

```sh
scripts/build-assets.sh              # development; pass `production` for minified bundles
```

## Checks (same as CI)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace       # needs DATABASE_URL and pdfium; MAILPIT_URL=http://localhost:8025 enables the SMTP test
cargo sqlx prepare --workspace --check
cargo deny check
```

Set `SQLX_OFFLINE=true` to build without a database.

## Parity checks

`scripts/parity/` runs the same requests against Rails (`RAILS_URL`, default `http://127.0.0.1:3000`) and the Rust server (`RUST_URL`, default `http://127.0.0.1:8080`), then compares the results. Ids, slugs, uuids, times and signed file tokens are normalized. Both servers share the dev database. The `rails` service bind-mounts `rust/tmp/rails-workdir`, so run the Rust server with `WORKDIR=rust/tmp/rails-workdir` to serve the same stored files.

```sh
eval "$(rust/scripts/parity/seed.sh)"     # PARITY_TOKEN, PARITY_TEMPLATE_ID (a two-party template)
cd rust/scripts/parity
./compare.py "$PARITY_TOKEN" GET /api/templates GET /api/submissions   # reads: method/path pairs, optional JSON body
./writes.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"    # template clone/update/archive, submission update/archive
./create.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"    # submission create modes and errors, submitter updates; compares stored rows
./signing.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"   # signing form /s/:slug: pages, view, upload, submit, decline; stored rows, result PDFs (also of the two-image template, with right-to-left text, with stamps, and of two documents with interactive forms), audit trail
./signing_configs.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"  # signed results and audit trails with timestamp servers and custom certificates
./emails.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"    # invitation, declined, next-party, completed, delegation and copy emails via Mailpit, email_events rows
./webhooks.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"  # webhook bodies, headers, signatures, event/attempt rows (API and signing form)
./downloads.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID" # /s/:slug/download, /s/:slug/documents, /submitters/:key/download; the linked files
./start_form.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID" # share link /d/:slug: pages, start/reopen, missing details, completed page, resubmit; stored rows
./form_extras.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID" # invite, metadata, remembered signatures, prefill_attachments, start_form_self; stored rows
./tools.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"     # /api/tools/merge (rendered pages) and /api/tools/verify (exact responses)
./combined.py                                        # combine_pdf_result_key, in its own account: combined PDF, rows, emails, downloads, API
./preview.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"   # submission preview /e/:slug (signed out, sig, signed in), /e/:slug/completed, /e/:slug/download, /p/:slug
./email_2fa.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"  # email one-time codes on /s/:slug and /d/:slug: pages, redirects, emails, otp_challenges and events
./uploads.py "$PARITY_TOKEN"                         # template uploads and added documents: responses, stored rows, preview PNGs
./previews.py                                        # /preview/:signed_key/:page, /templates/:id/preview, documents_page_objects (seeds its own template)
./documents.py "$PARITY_TOKEN"                       # documents_modify, GET documents, clone_and_replace: responses, stored rows, rendered PDFs, previews
./detect_fields.py                                   # /templates/:id/detect_fields event streams (seeds its own template)
```

`documents.py` uses the user and the PDFs of `uploads.py` (with `documents_seed.rb`'s files) and needs Pillow and poppler (`pdftoppm`, `pdftotext`). `DOCUMENTS_KEEP=1` keeps the templates it creates.

`uploads.py` signs in on both servers as a user that `uploads_seed.rb` creates with a known password, and uploads PDFs (form fields, links, 18 pages, a password) and images that the seed builds with HexaPDF and libvips. It needs Pillow, and reads the stored previews through the `rails` service. `UPLOADS_EXTRA_PDFS` (paths separated by `:`) adds uploads of more PDFs, such as forms found in the wild.

`signing.py` compares HTML pages after normalizing tokens, nonces, slugs, origins and indentation; pack tags differ because the Rust server reads the dev asset manifest. `signing.py`, `emails.py` and `webhooks.py` need both job workers: Sidekiq runs inside the `rails` service, and the Rust server needs `JOB_CONCURRENCY` > 0. For the email check, the Rust server needs the same SMTP settings as the `rails` service (`SMTP_ADDRESS=localhost SMTP_PORT=1025 SMTP_ENABLE_STARTTLS=false SMTP_FROM='Contracts <contracts@example.com>'`). `webhooks.py` listens on this machine's `en0` address (`HOOK_HOST`/`HOOK_PORT` override it), which both servers can reach.

`signing_configs.py` stores each signing setting through the `rails` service and restores it afterwards. Its timestamp server (`openssl ts -reply`) listens on `TSA_HOST:TSA_PORT` (default `HOOK_HOST` or 127.0.0.1, port 4598), which both servers must reach. To check PDF/A output, start both servers with `PDF_FORMAT=pdf/a-3b` and run `signing.py` again; it compares the XMP metadata and output intents of the result documents and audit trails.

## Status

Phase 3 ported the JSON API under `/api`: templates, submissions, submitters, form views, the user endpoint, full-text search, invitation emails and webhooks.

Phase 4a ports the recipient signing form at `/s/:slug`:

- The page itself.
- The awaiting, archived, expired, declined and completed pages, and `/success`.
- The view, click_email, values and upload endpoints.
- Saving and completing values (`Submitters::SubmitValues` with conditions, defaults, validation and invite-by-field).
- Declining, with the declined email.
- The completion job: completed submitters, next-party invitations, and the `form.*` and `submission.completed` webhooks.
- Active Storage disk URLs (`/disk/:encoded_key/*filename`).

Phase 4b generates the result documents in the completion job (`signing/results.rs`, a port of `Submissions::GenerateResultAttachments` and `EnsureResultGenerated`). The PDF editing behind it is in `loco_pdf::edit`:

- HexaPDF's text layout, with the same metrics, line wrapping and alignment.
- GoNoto embedded as subset fonts, and Times, Helvetica and Courier with a custom encoding.
- Rotating and flattening pages, annotations and form fields.
- Incremental updates that keep earlier signatures valid.
- `adbe.pkcs7.detached` signatures with the account's `esign_certs`.

`scripts/fetch-fonts.sh` downloads the fonts into `rust/vendor/fonts`. Without them, text falls back to Helvetica, as it does in Rails. The PDFs differ from HexaPDF's byte for byte, so `signing.py` compares rendered pages and checks the signatures with poppler.

Phase 4c adds the audit trail and the completed emails:

- `signing/audit.rs` ports `Submissions::GenerateAuditTrail` and `EnsureAuditGenerated`: documents and checksums, the parties with their verification details and values, and the event log. It is PDF/A-3u with GoNoto, signed like the results, and attached to the submission as `audit_trail`, so `audit_log_url` is filled in the API and webhooks.
- `loco_pdf::edit::compose` is the part of HexaPDF's composer the audit trail uses: frames with floats, text and image boxes, rules, column and table boxes that split across pages.
- `signing/emails.rs` sends `SubmitterMailer#completed_email` to the sender and the BCC addresses, and `#documents_copy_email` to the parties, with the result documents, the audit trail and uploaded files attached (up to 10 MB, `document_filename_format` applied).

`signing.py` compares the audit trails word by word (positions to 0.01 pt; ids, checksums and addresses may differ in text only), as rendered pages (with those words blanked), by metadata and by signature. It compares the words, metadata and signatures of the result PDFs too. Saved documents carry HexaPDF's `Producer` and its zone-less UTC dates, as the Rails documents do.

The rest of Phase 4 adds the links around the signing form:

- `signing/downloads.rs`: `/s/:slug/download` (documents signed so far), `/s/:slug/documents` and `/submitters/:slug/download` (with the `sig` link of documents copy emails), and `/submitters/:id/download` for signed-in users. Links are `/file/…` blob proxy paths with `document_filename_format` applied.
- `signing/start_form.rs`: the share link `/d/:slug` (start, reopen with the `start_form_slug` cookie, the private and completed pages) and resubmitting from the completed pages (`/resubmit_form`). New submissions get the defined and linked parties and the `submission.created` webhook.
- `signing/send_email.rs`: `/send_submission_email`, a documents copy email with a signed download link, once per 30 minutes.
- Delegating (`/s/:slug/delegate`), with the `submitter_versions` row, the invitation to the new email, and the `/s/:slug/delegated` page.
- `loco_rails_compat::cookies` reads and writes Rails' `cookies.encrypted` values, so the `start_form_slug` and `email_2fa_slug` cookies work across both servers. Like Rails, the Rust server does not write these secure cookies on plain-http requests in production.
- Devise's sign-in redirect is now an absolute URL, as in Rails.

- `signing/preview.rs`: the submission preview `/e/:slug` (the target of documents copy links and `{{documents.link}}`), signed out, with the `sig` link, and with the extra controls a signed-in user of the account sees (edit, send email/SMS, sign in person, resubmit, unarchive, the event log, revealing masked values); the 40 minute window and two-factor parties that redirect to `/e/:slug/completed`; the completed page with the copy-to-email form (`?status=error`, `email`); `/e/:slug/download`; and `/p/:slug`, the page a phone opens to draw a signature.

`downloads.py`, `start_form.py` and `preview.py` cover these; `emails.py` also covers delegating and `/send_submission_email`. `start_form.py` sends `X-Forwarded-Proto: https` so that Rails, which runs in production mode, writes its cookies. `preview.py` signs in on both servers as `parity-viewer@example.com`, a user that `seed.rb` creates (run `seed.sh` again on an existing database).

Email two-factor verification (`signing/email_2fa.rs`, `otp.rs`):

- The signing form with `require_email_2fa` renders `submit_form/email_2fa`. `PUT /submit_form_email_2fa` queues `SubmitterMailer#otp_verification_email` (and the `send_2fa_email` event), at most once per 15 seconds; `POST /submit_form_email_2fa` checks the code, records `email_verified` and sets the `email_2fa_slug` cookie.
- The share link asks for a code with `shared_link_2fa`, and when a pending party is reopened without the `start_form_slug` cookie: `start_form/email_verification`, `TemplateMailer#otp_verification_email`, the resend button (`POST /start_form_email_2fa_send`) and `?email_verification`. Resubmitting (`/resubmit_form`) with `shared_link_2fa` asks for a code too.
- `OtpChallenges`: the same HOTP codes, nonces and `otp_challenges` rows as Rails, so a code sent by one server verifies on the other. Codes are reused for 5 minutes and expire after 10; 20 failed attempts in 23 hours (100 overall) answer "too many attempts".
- The `RateLimit` calls of these endpoints (two per 45 seconds, per process) apply as in Rails, which enables them outside multitenant mode too. The bounced-email checks and the `custom_otp_email` sender are multitenant-only and not ported, so `start_form/error` (shown for a bounced address) is never rendered.

`email_2fa.py` covers these. It reads the codes from Mailpit and waits out the send limits between cases.

Result documents and their signatures, continued (issue #5):

- Two or more image documents are also merged into one more result PDF named after the submission, with the pages of the signed image PDFs (and so their signature fields) as HexaPDF imports them (`Editor::merge`).
- `PDF_FORMAT=pdf/a-3b` makes results and audit trails PDF/A-3b (`loco_pdf::edit::pdfa`): HexaPDF's XMP metadata from the information dictionary, the sRGB output intent, and the rebuilt `hmtx` tables of the TrueType fonts in loaded documents (`fix_glyph_widths`). The audit trail's level 3u uses the same code.
- Timestamp servers (`signing/timestamp.rs`, `Submissions::TimestampHandler`): each signature carries the server's RFC 3161 token, with the fallback after a comma and basic-auth credentials from the URL. A failed request embeds the current time instead, as in Rails.
- Custom signing certificates: the PKCS#12 file selected as the default (with OpenSSL's choice of key and certificate), and the `CERTS` environment variable.
- A later party's fonts and images no longer reuse the resource names of an earlier party's. That replaced the earlier text and signature images in multi-party results.
- Interactive forms in the documents get their field appearances as `maybe_flatten_pdf` has HexaPDF create them (`loco_pdf::edit::appearances`), then are flattened: a forced pass with `NeedAppearances`, then the regular one of `flatten` (text fields without appearances, every combo and list box, check boxes and radio buttons without two state appearances). Single-line, comb and multiline text, list boxes, check marks and radio dots, borders, backgrounds, `MK /R` rotation and the `AFNumber_Format`/`AFPercent_Format` actions follow HexaPDF and DocuSeal's patches (no push button appearances).
- Stamp fields (`signing/stamp.rs`, `Submitters::CreateStampAttachment`): completing a party adds its stamp image (`stamp.png`, reused by checksum) as the field's value. The logo and the half transparent white layer are composited as libvips does, and the text is laid out like `Vips::Image.text` (`loco_pdf::text_image`): Pango's runs, wrapping (hyphens inside words), line metrics and the autofit search, shaped with `harfrust` and hinted as fontconfig's `hintslight` has FreeType do. Its font is GoNoto Kurrent, which fontconfig picks for `sans` in the Docker image. The anti-aliasing differs slightly from FreeType's, so the PNG is close to Rails' but not identical: `signing.py` compares stamps with a tolerance and the rest of the result pages exactly.
- Right-to-left text (`loco_domain::bidi`, `TextUtils.maybe_rtl_reverse`) in result documents and the audit trail: Arabic letters in their presentation forms (`ArabicLetterConnector`), then TwitterCldr's own Bidi algorithm with its quirks, and right alignment where Rails uses it. The bidi classes and the `RTL_REGEXP` code points come from Rails (`scripts/dump-bidi-classes.sh`, which also refreshes the test fixture of Rails' output for sample texts).

Combined result documents (`combine_pdf_result_key`):

- `signing/combined.rs` ports `Submissions::GenerateCombinedAttachment` and `EnsureCombinedGenerated`: the last party's documents as `generate_pdfs` fills them, in schema order, then the audit trail in the account's locale, in one PDF signed like the results and attached to the submission as `combined_document`. The completion job generates it before the audit trail, so `combined_document_url` is filled in the API and webhooks.
- `loco_pdf::edit::Editor::import_pages` copies pages with what they reference into another document, as `HexaPDF::Importer` does.
- Downloads (`Submitters.select_attachments_for_download` and `?combined=true` with `build_combined_url`, also on `/e/:slug/download`) and the completed and documents copy emails use the combined document; the emails then leave out the separate audit trail.
- `GET`/`PUT /api/submissions/:id` and `GET /api/submissions` with `include=combined_document_url` generate missing combined documents.
- Unported branches reached from a request (custom certificates, timestamp servers, `PDF_FORMAT=pdf/a-3b`, right-to-left text in the audit trail, …) answer 422 with their "not supported yet" message.

`combined.py` covers these, in its own account so that the setting does not change what the other checks see.

Issue #7 ports the remaining signing-form endpoints and API gaps:

- `POST /s/:slug/invite`, `GET /s/:slug/metadata` (text runs from PDFium, cached in `document_metadata`), `POST /s/:slug/prefill_attachments` and `PUT /start_form_self/:id`.
- The remember-signature cookie (`signature_uuids`, written on upload with `remember_signature=true`) and `signed_signature_uuids` on the form page (`Submitters::FindRememberedSignature`).
- `completed` recipients in `POST /api/submissions` and `PUT /api/submitters/:id`, with default, formula and date values, invite-by-field, the `api_complete_form` event and the completion jobs.
- Prefilled attachment fields: base64 data or `https` URLs (with `DownloadUtils`' checks) of PNG and JPEG images and PDF files; blobs are reused by checksum.
- Share link starts without the requested details render the form with Rails' `field_error_proc` markup, and resubmitting copies the attachments the `default_values` refer to.
- Email message bodies of 2 KB or more have their styles and base64 data stored as `email_message_assets`.
- `POST /api/tools/merge` and `POST /api/tools/verify` (`loco_pdf::signatures` and `loco_pdf::cms`, a PKCS#7 reader that checks RSA signatures as `OpenSSL::PKCS7#verify` does).
- HTML errors without an error page (the 400 of a missing parameter) answer Rails' plain-text failsafe 500.

`create.py` covers the API changes, `start_form.py` the share link changes, and `form_extras.py` and `tools.py` the rest.

Template uploads port `TemplatesUploadsController#create` (`POST /templates_upload`), `TemplateDocumentsController#create` (`POST /templates/:id/documents`), `Templates::CreateAttachments` and `Templates::ProcessDocument` (`templates/`, `loco_pdf::upload`):

- PDFs and images (PNG, JPEG, ICO, BMP; other image types fail as in Rails). Password-protected PDFs get the password prompt and are stored decrypted.
- Form fields extracted from the PDF (`Templates::FindPdfiumAcroFields`), and its `http(s)` links in `metadata.pdf.annotations` (`Templates::BuildPdfiumAnnotations`). See the AcroForm detection paragraph below.
- Blob and attachment rows with Rails' metadata, key order included. Uploaded files are identified by content, as Marcel does.
- `preview_images`: up to 16 pages, with select placeholders hidden and form fields flattened, rendered 1400 px wide. They are written as libvips writes them: quantized by libimagequant 4.2.2 (the version in Rails' image) to a 2-, 4- or 8-bit palette, with the same `eXIf` and `pHYs` chunks. Images are turned upright and scaled to 1400 px.
- The `template.created` webhook and the search entry. A failed upload keeps the template and documents created so far, as in Rails.
- `Templates::BuildImagePagePdf` (`loco_pdf::image_page`), byte for byte. Nothing calls it yet; Rails uses it to edit documents in the template builder.
- `POST /api/templates` does not exist in this edition: like Rails, the Rust server answers 404, and `{"message": "This feature is not available in this edition"}` for `/api/templates/pdf`, `…/docx` and the other enterprise paths. Unrouted methods now answer 404 instead of 405, and unrouted paths pick HTML or JSON from the `Accept` header, as `ErrorsController` does.

AcroForm field detection (issue #10, `loco_pdf::upload`) gives the same fields and links as Rails:

- Field types: text, cells (comb fields with `MaxLen`, as `cell_w`), dates (`AFDate_` actions, with the format from the format or keystroke script), checkboxes, radio groups (from `/Opt` or from the export values; numeric and single export values become empty options; an `/Opt` that does not match the widgets is replaced with empty options), selects (combo boxes without placeholders; the value only counts when the appearance draws it), signatures and initials. List boxes and push buttons are skipped, and so are fields with a default value.
- Areas: widgets grouped into fields as Rails groups them (by field, by widget for detached widgets and unnamed fields), ordered by control index, on rotated pages and on media and crop boxes that do not start at 0,0. Zero-sized widgets are dropped.
- Names, descriptions (`TU`), `required`, alignment (`Q`), and widgets that are not in the form's field tree (read from their own dictionary when the file has an AcroForm or contains `/Form`).
- Links: `http(s)` URIs only, with the 250-link limit checked before each page, as in Rails (so a page can take the count past 250).
- Rails' `PDFium` build reads checkbox and radio states and export values through its own "raw" functions; the Rust server loads the page's form view so that the standard functions return the same values.

`uploads.py` compares these through uploads and added documents, including the HexaPDF fixtures for each branch and, with `UPLOADS_EXTRA_PDFS`, real forms (IRS W-9, W-4, W-8BEN, 1040, 4506-T, SS-4, 8822, GSA SF1449, OPM SF2809 matched). `crates/pdf/tests/upload.rs` checks the fixtures against Rails' stored JSON, float digits included. Replacing documents with `clone_and_replace` uses it too (see below); MCP's `create_template` is ported with its endpoint (#21).

Preview pages of PDFs come out pixel for pixel as in Rails, although the two `PDFium` builds differ. Rails' build reads checkbox and radio states through its own "raw" functions; the Rust server loads the page's form view instead, so the standard functions see them too. Previews of uploaded images differ slightly (mean difference below 0.3 of 255), because `image`'s filters replace libvips' resize, so their palettes can have a few more or fewer colors. The PNG compression differs as well, so preview byte sizes and checksums are not the same as Rails'.

The document page endpoints around the template builder:

- `template_preview.rs`: the lazy `/preview/:signed_key/:page` images (`PreviewDocumentPageController`). A page without a stored preview is rendered with PDFium and stored as `"#{page}.jpg"` with Rails' blob metadata; stored previews redirect to their disk URL.
- `loco_pdf::jpeg` writes these JPEGs as libvips with libjpeg-turbo does (`interlace: true, Q: PAGE_QUALITY`): its EXIF block, 4:2:0, the islow DCT, the simple progression script and optimized Huffman tables. The files are byte for byte Rails'.
- `/templates/:id/preview` (`TemplatesPreviewController`): the read-only builder in the `plain` layout, with `Templates.serialize_for_builder` as `data-template`.
- `/templates/:id/documents_page_objects` (`TemplateDocumentsPageObjectsController`): `loco_pdf::pages` flattens the page, unwraps form XObjects and applies the rotation through PDFium, as `lib/pdfium.rb` does, and sorts the characters with musl's `qsort_r` (what Ruby's `sort` calls in the Rails image).
- `loco_rails_compat::json` writes floats as the `json` gem (2.21, `fpconv`) does, so `render json:` bodies match Rails' digits. `api::rails_json` uses it for every response.

`previews.py` covers these. `/templates/:id/preview` in the Hotwire Native apps answers 422.

The template builder's document edits (issue #12):

- `templates/modify.rs` ports `TemplateDocumentsModifyController` and `Templates::ModifyDocuments`. The layout is validated as Rails validates it (422 `unable_to_save`; 400 without `documents`). Unchanged documents are kept, and changed ones are built again. Pages are reordered, moved between documents (`replaced_page`), turned and redacted. Field areas follow their pages (turned with them), fields left without areas are removed, and so are the conditions on them.
- `loco_pdf::modify` builds the PDFs with `PDFium` as `lib/pdfium.rb` does: `FPDF_ImportPages` per run of pages, then `Pdfium::Page#redact` (flatten, unwrap the form XObjects under a box, remove the covered characters and rewrite the rest of their text objects in Helvetica, redact image objects, draw the black boxes, drop blank text), then the turns.
- Rails' `PDFium` build adds `FPDF_ImportAcroForm` and `FPDF_RemoveOrphanObjects`, which the public builds lack. `loco_pdf::acroform` does their work on the saved file with lopdf. It copies the field trees whose `/Parent` the public `FPDF_ImportPages` leaves pointing into the source. It registers the root fields in page order (`name_1` for a taken name) with `/DA`, `/DR`, `/Q` and `/NeedAppearances` from the sources. Flattened fields leave `/Fields`, and after a redaction unreferenced objects (the redacted text's fonts) are removed. Without widgets there is no `/AcroForm`, as in Rails.
- Images follow `ModifyDocuments` and libvips (`templates/images.rs`). An image page in a PDF entry is its first preview, flattened, redacted and turned, as a palette PNG in a page (`BuildImagePagePdf`). The page takes the entry's PDF paper size, the image's own shape, or the account's paper for scans. An image alone is turned or redacted and written again as JPEG (Q 90) or PNG. Redacted image objects in PDFs become JPEGs (Q 50). `loco_pdf::jpeg::encode_baseline` writes these JPEGs byte for byte as libvips does with `strip: true`: baseline, standard Huffman tables, 4:2:0 below Q 90.
- `GET /templates/:id/documents` (`TemplateDocumentsController#index`): the schema documents' signed proxy paths, valid for five minutes.
- `templates/clone_replace.rs` ports `TemplatesCloneAndReplaceController`, `Templates::Clone`, `Templates::ReplaceAttachments` and `Templates::CloneAttachments`. The new template is named after the first file. The uploads replace the first documents and take over their areas, and an upload whose pages no field uses brings its form fields (inserted at Rails' index, `pending_fields` for the first). The other documents are copied. It answers HTML (redirect to the builder, or the password prompt) or JSON.

`documents.py` covers these. The PDFs differ from Rails' in their bytes (another `PDFium` build, and lopdf's rewrite), so it compares their rendered pages, text, field names, and the objects that still hold redacted text. After a redaction, Rails' `FPDF_RemoveOrphanObjects` can leave a cross-reference entry without its object; readers rebuild the table, and the Rust files don't have this. Previews of image documents are resized by `image` instead of libvips (as for uploads), so a 2-bit (four color) preview can get another palette from the same image. The script reports that as known.

Field detection (`POST /templates/:id/detect_fields`, issue #11) ports `TemplatesDetectFieldsController`, `Templates::DetectFields` and `Templates::ImageToFields` (`templates/detect_fields.rs`, the `detect` crate):

- The same ONNX model (`model_704_int8`, docusealco/fields-detection 2.0.0) runs through `ort`, with the inputs Rails gives it: pages rendered by PDFium at the model's resolution, or the image document's first preview, trimmed, padded and resized as libvips does it.
- The detections are filtered, merged and sorted as in Rails (non-maximum suppression, overlapping boxes, Numo's sort order). They are then joined with the page's characters and lines (`loco_pdf::pages::detect_page`), which give fields their labels, types (date, signature, number, checkbox, …) and underscore blanks.
- The response is the same server-sent event stream, one `{attachment_uuid, page, fields}` event per page and then `{completed: true}`. As with `ActionController::Live`, an error ends the stream without `completed`, and signed-out requests get the 500 page.
- `scripts/fetch-onnx.sh` provides the model and runtime for development and CI. The Dockerfile now checks the model's checksum.

`detect_fields.py` compares the event streams with Rails' for whole templates, single pages, image documents, parameter variants, errors in the stream and authorization. `crates/detect/tests/golden.rs` checks the fields against Rails' output for fixture files. ONNX Runtime picks its int8 kernels by CPU, so there each box only has to overlap Rails' by an intersection over union of at least 0.95; the number, order and types of the fields are compared exactly, and the fixtures' detections score well clear of the confidence thresholds.

These parts return 422 ("not supported yet") or are still missing:

- Field detection with a model other than the v2 one (without an `orig_target_sizes` input), and image documents whose first preview is a JPEG, a grayscale PNG or a 16-bit PNG.

- These result documents fail the completion job with "not supported yet":
  - dynamic documents;
  - form field appearances where HexaPDF would fail and Rails would leave the form unflattened: text the field's font cannot encode, embedded TrueType or symbol fonts, CMYK text, `AFTime_Format`, combo boxes with several values, and list box values that are not options;
  - signing with the `docuseal_aatl` certificate without `CERTS`, with a certificate without its private key, or with a non-RSA key.
- Share link email two-factor verification for a party without an email (a `link_form_fields` without `email`) answers 422.
- The signed-in download link of the dashboard (`/submissions/:id/download`), including its `?combined=true`.
- The submission preview `/e/:slug` in the mobile app (`Hotwire Native` user agents) and for dynamic documents answers 422.
- The signed-out landing page at `/` (Rails renders it; the Rust server redirects to sign-in).
- Dynamic documents (422 on the page; Phase 5).
- `/templates/:id/preview` in the Hotwire Native apps (422).
- Presigned URLs for cloud storage services (cloud blobs go through the blob proxy instead).
- Prefilling attachment fields with typed text (Rails renders it in a font) or with file types other than PNG, JPEG and PDF.
- Stamp fields of a recipient that `POST /api/submissions` creates as `completed` answer 422.
- `/api/tools/merge` with documents that have form fields; `/api/tools/verify` with RFC 3161 timestamp signatures, signatures without RSA keys, revisions after the last signature that end with `%%EOF` (`FPDF_HasOnlyDSSChanges`), and trusted certificates other than the generated ones (custom certificates, `CERTS`, `TRUSTED_CERTS`).
- Expiration jobs (Phase 7).
- Extracting email assets from message bodies of 2 KB or more.
- Uploading Word documents (Rails converts them with the closed-source `WordCraft` gem), ZIP archives, and documents from a URL (`/templates_upload/new`, `params[:url]`) answer 422.
- The template builder itself (`/templates/:id/edit`, where uploads redirect) and MCP's `create_template`.
- Cropping photographed pages (`GET` and `POST /templates/:id/documents_crop`, `Templates::CreateDocumentCrop`): Rails finds the corners, warps, turns and dewarps with Leptonica, and resizes and decodes with libvips. Both answer 422 once the template and the document are found.
- `GET /templates/:id/documents` with dynamic documents, and `clone_and_replace` of templates with dynamic documents, answer 422.

## Layout

| Crate | Replaces |
| --- | --- |
| `server` | Controllers, routes, middleware, error pages, boot |
| `views` | ERB views (minijinja), `I18n.t`, pack tags, `svg_icon` |
| `mailer` | ActionMailer delivery and the SMTP settings interceptor |
| `db` | ActiveRecord models and queries |
| `domain` | `lib/submissions`, `lib/submitters`, `lib/templates` |
| `rails-compat` | AR encryption, `MessageVerifier`, Devise bcrypt, CSRF |
| `storage` | Active Storage |
| `jobs` | Sidekiq (Postgres-backed queue) |
| `pdf` | PDFium, HexaPDF (rendering, modification, signing, audit trail) |
| `detect` | `Templates::DetectFields`, `Templates::ImageToFields` (ONNX field detection) |
