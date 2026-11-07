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
