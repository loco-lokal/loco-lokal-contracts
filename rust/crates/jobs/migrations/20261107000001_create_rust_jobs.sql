-- Background job queue for the Rust backend (replaces Sidekiq + Redis).
-- Rust-owned table; not part of the Rails schema (rust/schema/baseline.sql excludes it).
CREATE TABLE rust_jobs (
    id           bigserial PRIMARY KEY,
    queue        text        NOT NULL DEFAULT 'default',
    kind         text        NOT NULL,
    payload      jsonb       NOT NULL DEFAULT '{}'::jsonb,
    attempts     integer     NOT NULL DEFAULT 0,
    max_attempts integer     NOT NULL DEFAULT 25,
    run_at       timestamptz NOT NULL DEFAULT now(),
    locked_at    timestamptz,
    locked_by    text,
    failed_at    timestamptz,
    last_error   text,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX index_rust_jobs_ready
    ON rust_jobs (queue, run_at, id)
    WHERE locked_at IS NULL AND failed_at IS NULL;

CREATE INDEX index_rust_jobs_locked_at
    ON rust_jobs (locked_at)
    WHERE locked_at IS NOT NULL;
