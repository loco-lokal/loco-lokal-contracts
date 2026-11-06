# Repository guide for agents

## Purpose and scope

This repository contains the DocuSeal document filling and electronic signing application. Despite the checkout directory name `loco-lokal-docs`, it is a complete application, not only a documentation site. The README, application namespace, package metadata, and product constants retain DocuSeal branding; do not infer a Loco Lokal rebranding requirement from the directory name.

This guide reflects inspection of the checked-in source and configuration. Setup, builds, and application tests were not executed when writing it. Recheck dependency manifests and CI when changing the runtime or build workflow.

## Stack and execution model

- Ruby **4.0.9** (`Gemfile`, Dockerfile, CI), Rails **8.1.3.1** (`Gemfile.lock`; application defaults are 8.1).
- Server-rendered ERB and Turbo for the main application, with Vue 3 components for the template builder and signer interface. Smaller behaviors use custom elements in `app/javascript/elements/`.
- Shakapacker 9.5 and Webpack 5 build JavaScript and SCSS. Tailwind CSS 3 and DaisyUI provide styling. Yarn **1.22.22** is declared in `package.json`; the ESLint CI job uses Node **20.19.0**.
- PostgreSQL is the configured development/test database. Production also supports SQLite and MySQL/Trilogy through `config/database.yml`; production defaults to SQLite when no external database is configured.
- Active Storage manages original files, previews, signatures, generated documents, and audit trails. Backends include disk, S3, Google Cloud Storage, and Azure (`config/storage.yml`).
- Sidekiq handles completion, email, indexing, expiration, and webhook work. Puma embeds Sidekiq and enables the local Redis plugin when `MULTITENANT != 'true'` or `DEMO == 'true'`. If `REDIS_URL` is unset, configuration selects a local Redis instance on port **16379**, which the plugin starts using the `redis-server` executable.
- PDF/image processing uses HexaPDF, PDFium, libvips, and Leptonica. Automatic field detection also uses ONNX Runtime and an external model. Native libraries, fonts, and the model are provisioned by the Dockerfile; installing gems alone does not provide all of them.

## Code map

| Location | Responsibility |
| --- | --- |
| `app/controllers/` | Dashboard, template editing, submissions, signer flows, configuration, authentication, and downloads |
| `app/controllers/api/` | JSON API; `api_base_controller.rb` supplies authentication, authorization, pagination, and error handling |
| `app/controllers/mcp/`, `app/controllers/mcp_controller.rb` | MCP protocol dispatch and document/template tools |
| `app/models/` | Active Record entities, associations, serialized document data, and attachments |
| `lib/templates/`, `lib/submissions/`, `lib/submitters/` | Document preparation, submission creation, validation, signing, generation, and serialization logic |
| `lib/ability.rb`, `lib/abilities/` | CanCanCan authorization rules and template access conditions |
| `app/jobs/`, `app/mailers/` | Background processing, notifications, and webhook delivery |
| `app/views/` | ERB pages, layouts, mailer views, shared partials, and icons |
| `app/javascript/application.js` | Main application entry point, custom-element registration, and Vue template-builder mounting |
| `app/javascript/form.js`, `app/javascript/submission_form/` | Signer entry point and Vue signing steps |
| `app/javascript/template_builder/` | Vue document/field editor, dynamic document editor, and builder translations |
| `config/locales/i18n.yml` | Rails translations; client components also have their own `i18n.js` files |
| `config/routes.rb` | Web, API, signing, download, embedding-script, and MCP routes |
| `config/webpack/`, `config/shakapacker.yml`, `tailwind*.config.js` | Asset compilation and separate application, form, and dynamic-editor style scopes |
| `db/migrate/`, `db/schema.rb` | Migration history and schema snapshot |
| `docs/` | OpenAPI specification, language-specific API examples, embedding guides, and webhook payload examples |
| `spec/` | Request, browser/system, job, and library specs, plus factories and fixtures |
| `.github/workflows/` | Lint/security/test CI and tagged Docker release builds |

Substantial business logic lives in `lib/`, often as namespaced modules exposing `.call`. Follow these patterns instead of assuming there is an `app/services/` layer. Rails autoloads `lib/` through `config/application.rb`.

## Domain model and main workflows

`Account` owns users, templates, submissions, submitters, configuration, and webhook destinations. Linked testing accounts are distinct accounts; API access must preserve the separation between testing and production data.

`Template` is a reusable signing form. Its JSON-serialized text columns contain document schema, fields, recipient roles (`submitters`), preferences, and variable definitions. Documents use Active Storage; folders, sharing/access records, and versions support organization and reuse.

`Submission` is an individual signing request, associated with an account and optionally a template. It carries snapshots such as `template_fields`, `template_schema`, and `template_submitters`, plus preferences, variables, recipient ordering, expiration, and completion state. Do not assume its template is always present or that editing the current template should alter an existing request.

`Submitter` is one recipient in a submission. It holds recipient contact information, field values, preferences, metadata, a public slug, timestamps, and attachments. Field UUIDs and recipient UUIDs connect the serialized schema to recipient values; preserve these relationships when changing builder or signer data structures.

The principal flow is:

1. Upload/prepare documents and define fields and recipient roles in a template (`Templates` and `lib/templates/`).
2. Create a submission and recipients through the web interface, a template share link, or the API (`Submissions` and `lib/submissions/`).
3. A template share link starts at `/d/:slug`; a recipient signs at `/s/:slug`. `SubmitFormController` checks access, verification, ordering, expiration, archival, and completion conditions.
4. `Submitters::SubmitValues` normalizes and validates values, records events, saves completion data, and queues `ProcessSubmitterCompletionJob`.
5. Completion processing generates result documents and audit trails, records completed recipients/documents, advances ordered recipients, and queues emails and webhooks.

PDF result and audit generation are in `lib/submissions/`; PDF primitives and native bindings are in `lib/pdf_utils.rb`, `lib/pdfium.rb`, and related modules. Some API reads ensure missing generated artifacts exist, so document-generation changes can affect both requests and jobs.

## Authentication and integration boundaries

- Web authentication uses Devise and two-factor support. `ApplicationController` normally requires a signed-in user and authorization; public signing controllers deliberately apply separate access checks.
- API controllers inherit from `Api::ApiBaseController`, which accepts a signed-in user or an `X-Auth-Token` header. Token lookup hashes the supplied value with SHA-256. Authorization remains enforced through CanCanCan.
- Preserve account scoping and test/production account boundaries when changing queries, serializers, downloads, or jobs. Public signer slugs must continue to pass the existing signing authorization and verification checks.
- `WebhookUrl`, `WebhookEvent`, and `WebhookAttempt` track destinations, events, and attempts. `lib/send_webhook_request.rb` sends payloads and sets `X-Docuseal-Signature` using `lib/webhook_urls/signatures.rb` unless already provided. Preserve signing, retry behavior, and event tracking.
- The application exposes `/mcp` and `/js/:filename`. Inspect the controller implementation before assuming a documented integration is available in this checkout: `EmbedScriptsController` contains upgrade placeholders for embedded components.
- `Docuseal.multitenant?`, `Docuseal.demo?`, and Rails load hooks change behavior. The README includes Pro features, and `.gitignore` excludes `/ee`; do not assume all cloud/enterprise implementations are present.
- `docs/openapi.json` describes hosted API servers and paths; local API routes are under `/api`. Cross-check documentation against local routes and controllers when changing a contract.

## Local development

Provide Ruby/Bundler, Node/Yarn, PostgreSQL, Redis, and the native document-processing libraries first. Browser specs require Chrome/Chromium. Use the Dockerfile and RSpec CI job as dependency references; their package commands target Alpine Linux and should not be copied directly onto macOS.

Typical source-development commands, after dependencies and database access are configured:

```sh
bundle install
yarn install --frozen-lockfile
bin/rails db:prepare
bundle exec foreman start -f Procfile.dev
```

`Procfile.dev` starts Rails on **3000** and the Shakapacker dev server, configured on **3035**. The first web visit redirects to setup when there are no users. Development email is available through `/letter_opener`.

`bin/setup` installs missing gems, prepares the database, clears logs/temp files, and restarts Rails; it does not install JavaScript dependencies or native libraries. Development/test database names default to `docuseal_dev` and `docuseal_test`; provide a suitable `DATABASE_URL` when local PostgreSQL defaults do not apply.

`config/dotenv.rb` is custom boot configuration: production can load or generate `docuseal.env` under `WORKDIR`, or obtain secrets from AWS Secrets Manager. Do not assume a conventional development `.env` loader is configured. Other deployment settings are read directly from `ENV` and/or stored account configuration.

For a source asset build, use `bin/shakapacker`. The RSpec CI job precompiles test assets with `RAILS_ENV=test NODE_ENV=test bundle exec rake assets:precompile`.

The checked-in `docker-compose.yml` runs the published `docuseal/docuseal:latest` image with PostgreSQL 18 and Caddy. It does **not** build this checkout. To validate local source in a container, build the Dockerfile and explicitly select that image. Production boot runs database migrations unless `RUN_MIGRATIONS=false`.

For a persistent local instance of this checkout, run `docker compose -f compose.local.yml up -d --build` and open `http://localhost:3000`. This configuration builds the local source, runs PostgreSQL privately, and keeps documents/configuration and database data in named volumes. The app is bound to the local machine only. Complete initial account setup in the browser and configure SMTP in settings if email delivery is needed.

Use `docker compose -f compose.local.yml stop` to stop it, `docker compose -f compose.local.yml up -d` to restart it, and `docker compose -f compose.local.yml logs -f app` to inspect logs. Avoid `down -v` unless intentionally deleting the local contracts and database.

## Validation

Choose checks that exercise the changed behavior. The CI workflow is the authoritative reference for complete setup and checks.

```sh
# Focused examples; a line suffix can narrow an RSpec run further.
bundle exec rspec spec/requests/submissions_spec.rb
bundle exec rspec spec/system/signing_form_spec.rb
bundle exec rspec spec/jobs/process_submitter_completion_job_spec.rb

# Full Ruby suite and CI checks.
bundle exec rspec
bundle exec rubocop
bundle exec erb_lint ./app
./node_modules/.bin/eslint 'app/javascript/**/*.{js,vue}'
bundle exec brakeman -q --exit-on-warn
```

The ESLint CI job checks JavaScript files only; the command above also covers Vue components. `yarn eslint` runs with `--fix` and modifies files, so use the direct command for a read-only check.

RSpec automatically loads `rails_helper` through `.rspec`. Tests use PostgreSQL, FactoryBot, transactional fixtures, fake Sidekiq jobs by default, and WebMock with external connections disabled. Specs marked `sidekiq: :inline` execute jobs inline. System specs use Cuprite/Chrome, default to headless, and can be run with `HEADLESS=false`. CI prepares the test database, compiles assets, and then runs RSpec with `COVERAGE=true`.

If prerequisites prevent execution, report the missing dependency and the checks left unrun. Do not describe source inspection as a passing application test.

## Rust backend rewrite (`rust/`)

A Rust replacement for the Rails backend is in progress under `rust/`. It is a Cargo workspace with `axum`, `sqlx` (PostgreSQL only), and `minijinja`. Setup, the dev services (`compose.rust-dev.yml`: Postgres on 5435, Mailpit, and Rails as the reference app), and the checks are documented in `rust/README.md`.

- Rails migrations own the schema until cutover. After they change, regenerate `rust/schema/baseline.sql` (`rust/scripts/dump-schema.sh`) and `rust/.sqlx` (`cargo sqlx prepare --workspace`).
- Rust code must stay compatible with Rails-produced data: the existing tables, the Active Storage layout, encrypted columns, signed ids, and Devise hashes. `crates/rails-compat` is checked against values Rails generated (`rust/scripts/generate-golden.sh`).
- Rust-only tables (such as the `rust_jobs` queue) are added through `rust/crates/*/migrations`, never through Rails migrations.
- Templates in `rust/templates/` are ports of `app/views`. The Rust server reads `app/views/icons`, `app/views/errors`, `config/locales`, and `public/` directly, so keep those in place until cutover.
- CI runs the `Rust` job (fmt, clippy `-D warnings`, nextest, sqlx offline check, cargo-deny).

## Contribution conventions

- Follow `.rubocop.yml`, `.erb_lint.yml`, and `.eslintrc`. Ruby files generally use `# frozen_string_literal: true`; JavaScript uses Standard style and the Vue 3 recommended rules.
- Keep changes in the existing Rails/ERB, custom-element, and Vue structure. Respect custom-element connection/disconnection and Turbo cache cleanup when adding browser behavior.
- Keep builder fields, signer rendering, server validation, PDF output, API serializers, and webhook payloads consistent when changing a field type or document schema.
- Add database changes through migrations; do not rewrite historical migrations. Update the schema through Rails as appropriate. Retain compatibility with the supported production adapters when choosing database-specific behavior.
- Update relevant translations, API examples/specification, embedding guides, and webhook examples when changing a public contract or visible copy.
- Keep generated assets, attachments, database files, secrets, coverage output, and ONNX models out of commits. Check `.gitignore` before adding generated artifacts; retain `Gemfile.lock` and `yarn.lock` when intentionally changing dependencies.
- Preserve DocuSeal attribution required by `LICENSE_ADDITIONAL_TERMS` and consult `LICENSE` for the AGPLv3 terms. Follow `SECURITY.md` for vulnerability reporting.

## Useful starting points by task

| Task | Start here |
| --- | --- |
| Template builder or field editing | `app/javascript/template_builder/`, `app/controllers/templates_controller.rb`, `lib/templates/` |
| Recipient signing behavior | `app/javascript/submission_form/`, `app/controllers/submit_form_controller.rb`, `lib/submitters/submit_values.rb` |
| Submission API contract | `app/controllers/api/submissions_controller.rb`, `lib/params/`, `lib/submissions/serialize_for_api.rb`, `spec/requests/submissions_spec.rb` |
| PDF output or audit trail | `lib/submissions/`, `lib/pdf_utils.rb`, `lib/pdfium.rb`, `app/jobs/process_submitter_completion_job.rb` |
| Webhooks | `app/jobs/send_*_webhook_request_job.rb`, `lib/send_webhook_request.rb`, `lib/webhook_urls/`, `spec/jobs/`, `docs/webhooks/` |
| Permissions or account isolation | `lib/ability.rb`, `lib/abilities/`, API base controller, account models and request specs |
| Runtime/deployment issues | `config/dotenv.rb`, `config/puma.rb`, `lib/puma/plugin/`, Dockerfile, CI workflow |
