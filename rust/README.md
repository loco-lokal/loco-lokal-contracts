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
./signing.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"   # signing form /s/:slug: pages, view, upload, submit, decline; stored rows, result PDFs, audit trail
./emails.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"    # invitation, declined, next-party, completed, delegation and copy emails via Mailpit, email_events rows
./webhooks.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID"  # webhook bodies, headers, signatures, event/attempt rows (API and signing form)
./downloads.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID" # /s/:slug/download, /s/:slug/documents, /submitters/:key/download; the linked files
./start_form.py "$PARITY_TOKEN" "$PARITY_TEMPLATE_ID" # share link /d/:slug: pages, start/reopen, completed page, resubmit; stored rows
./combined.py                                        # combine_pdf_result_key, in its own account: combined PDF, rows, emails, downloads, API
```

`signing.py` compares HTML pages after normalizing tokens, nonces, slugs, origins and indentation; pack tags differ because the Rust server reads the dev asset manifest. `signing.py`, `emails.py` and `webhooks.py` need both job workers: Sidekiq runs inside the `rails` service, and the Rust server needs `JOB_CONCURRENCY` > 0. For the email check, the Rust server needs the same SMTP settings as the `rails` service (`SMTP_ADDRESS=localhost SMTP_PORT=1025 SMTP_ENABLE_STARTTLS=false SMTP_FROM='Contracts <contracts@example.com>'`). `webhooks.py` listens on this machine's `en0` address (`HOOK_HOST`/`HOOK_PORT` override it), which both servers can reach.

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

`signing.py` compares the audit trails word by word (positions to 0.01 pt; ids, checksums and addresses may differ in text only), as rendered pages, by metadata and by signature. Saved documents carry HexaPDF's `Producer` and its zone-less UTC dates, as the Rails documents do.

The rest of Phase 4 adds the links around the signing form:

- `signing/downloads.rs`: `/s/:slug/download` (documents signed so far), `/s/:slug/documents` and `/submitters/:slug/download` (with the `sig` link of documents copy emails), and `/submitters/:id/download` for signed-in users. Links are `/file/…` blob proxy paths with `document_filename_format` applied.
- `signing/start_form.rs`: the share link `/d/:slug` (start, reopen with the `start_form_slug` cookie, the private and completed pages) and resubmitting from the completed pages (`/resubmit_form`). New submissions get the defined and linked parties and the `submission.created` webhook.
- `signing/send_email.rs`: `/send_submission_email`, a documents copy email with a signed download link, once per 30 minutes.
- Delegating (`/s/:slug/delegate`), with the `submitter_versions` row, the invitation to the new email, and the `/s/:slug/delegated` page.
- `loco_rails_compat::cookies` reads and writes Rails' `cookies.encrypted` values, so the `start_form_slug` and `email_2fa_slug` cookies work across both servers. Like Rails, the Rust server does not write these secure cookies on plain-http requests in production.
- Devise's sign-in redirect is now an absolute URL, as in Rails.

`downloads.py` and `start_form.py` cover these; `emails.py` also covers delegating and `/send_submission_email`. `start_form.py` sends `X-Forwarded-Proto: https` so that Rails, which runs in production mode, writes its cookies.

Combined result documents (`combine_pdf_result_key`):

- `signing/combined.rs` ports `Submissions::GenerateCombinedAttachment` and `EnsureCombinedGenerated`: the last party's documents as `generate_pdfs` fills them, in schema order, then the audit trail in the account's locale, in one PDF signed like the results and attached to the submission as `combined_document`. The completion job generates it before the audit trail, so `combined_document_url` is filled in the API and webhooks.
- `loco_pdf::edit::Editor::import_pages` copies pages with what they reference into another document, as `HexaPDF::Importer` does.
- Downloads (`Submitters.select_attachments_for_download` and `?combined=true` with `build_combined_url`) and the completed and documents copy emails use the combined document; the emails then leave out the separate audit trail.
- `GET`/`PUT /api/submissions/:id` and `GET /api/submissions` with `include=combined_document_url` generate missing combined documents.
- Unported branches reached from a request (custom certificates, timestamp servers, `PDF_FORMAT=pdf/a-3b`, right-to-left text in the audit trail, …) answer 422 with their "not supported yet" message.

`combined.py` covers these, in its own account so that the setting does not change what the other checks see.

These parts return 422 ("not supported yet") or are still missing:

- These result documents fail the completion job with "not supported yet":
  - dynamic documents;
  - two or more image documents (Rails merges them into one PDF);
  - custom signing certificates;
  - timestamp servers;
  - PDF/A-3b output (`PDF_FORMAT=pdf/a-3b`), also for the audit trail and the combined document.
- Right-to-left text is drawn in logical order, where Rails reorders it visually.
- Form fields that need their appearances generated (`NeedAppearances`) are left unflattened.
- Right-to-left text in the audit trail and a timestamp server for its signature fail the completion job with "not supported yet".
- Email two-factor verification: sending and checking codes on the signing form and the share link (`shared_link_2fa`, reopening a pending party without the start form cookie, `?email_verification`) answers 422. A valid `email_2fa_slug` cookie or two-factor token is accepted.
- The signed-in download links of the dashboard (`/submissions/:id/download`) and of the submission preview (`/submissions_preview/:slug/download`), including their `?combined=true`.
- The submission preview `/e/:slug` (the target of documents copy links) and `/p/:slug` (drawing a signature on a phone).
- Share link starts without the requested details (the browser requires them), and resubmitting with `default_values`, answer 422.
- The signed-out landing page at `/` (Rails renders it; the Rust server redirects to sign-in).
- Stamp fields (422 on completion), dynamic documents (422 on the page; Phase 5), and the lazy `/preview/:signed_key/:page` images.
- The remember-signature cookie, and presigned URLs for cloud storage services (cloud blobs go through the blob proxy instead).
- Creating `completed` submitters and completing them through `PUT /api/submitters/:id`, and prefilling attachment fields.
- Expiration jobs (Phase 7).
- Extracting email assets from message bodies of 2 KB or more.

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
