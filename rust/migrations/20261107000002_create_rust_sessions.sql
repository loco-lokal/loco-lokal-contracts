-- Browser sessions for the Rust backend (replaces the Rails cookie store and Devise's
-- remember-me cookie). Rust-owned table; not part of the Rails schema.
-- The cookie carries a random token; only its SHA-256 digest is stored. user_id has no
-- foreign key so Rust migrations do not depend on the Rails schema; lookups join users.
CREATE TABLE rust_sessions (
    id_digest    text        PRIMARY KEY,
    user_id      bigint,
    -- users.encrypted_password[0, 30] at sign-in (Devise's authenticatable_salt): changing
    -- the password ends every session.
    auth_salt    text,
    data         jsonb       NOT NULL DEFAULT '{}'::jsonb,
    expires_at   timestamptz NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX index_rust_sessions_on_user_id ON rust_sessions (user_id);
CREATE INDEX index_rust_sessions_on_expires_at ON rust_sessions (expires_at);
