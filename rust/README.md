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
```

Set `PDFIUM_LIB_DIR` to use a libpdfium from somewhere else.

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

`crates/rails-compat/tests/fixtures/golden.json` holds values that Rails produced with the dev `SECRET_KEY_BASE`: encrypted columns, signed ids, an encrypted SMTP config, and a bcrypt hash. Tests check that Rust reads and writes the same formats. To regenerate the file, with the `rails` service running:

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
```

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

These parts return 422 ("not supported yet") or are still missing:

- These result documents fail the completion job with "not supported yet":
  - dynamic documents;
  - form field appearances where HexaPDF would fail and Rails would leave the form unflattened: text the field's font cannot encode, embedded TrueType or symbol fonts, CMYK text, `AFTime_Format`, combo boxes with several values, and list box values that are not options;
  - signing with the `docuseal_aatl` certificate without `CERTS`, with a certificate without its private key, or with a non-RSA key.
- Share link email two-factor verification for a party without an email (a `link_form_fields` without `email`) answers 422.
- The signed-in download link of the dashboard (`/submissions/:id/download`), including its `?combined=true`.
- The submission preview `/e/:slug` in the mobile app (`Hotwire Native` user agents) and for dynamic documents answers 422.
- The signed-out landing page at `/` (Rails renders it; the Rust server redirects to sign-in).
- Dynamic documents (422 on the page; Phase 5), and the lazy `/preview/:signed_key/:page` images.
- Presigned URLs for cloud storage services (cloud blobs go through the blob proxy instead).
- Prefilling attachment fields with typed text (Rails renders it in a font) or with file types other than PNG, JPEG and PDF.
- Stamp fields of a recipient that `POST /api/submissions` creates as `completed` answer 422.
- `/api/tools/merge` with documents that have form fields; `/api/tools/verify` with RFC 3161 timestamp signatures, signatures without RSA keys, revisions after the last signature that end with `%%EOF` (`FPDF_HasOnlyDSSChanges`), and trusted certificates other than the generated ones (custom certificates, `CERTS`, `TRUSTED_CERTS`).
- Expiration jobs (Phase 7).

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
