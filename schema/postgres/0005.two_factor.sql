-- Authenticator-app second factor. The 0001 table was never written by application code, so it is
-- replaced: the encrypted TOTP secret, the last accepted time step (a code works once) and the hashed
-- one-time recovery codes. users.two_factor_enabled is kept in step.
DROP TABLE IF EXISTS two_factor_auth;

CREATE TABLE two_factor_auth (
    user_id BIGINT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    totp_secret_encrypted BYTEA NOT NULL,
    totp_nonce BYTEA NOT NULL,
    last_used_step BIGINT,
    enabled_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE two_factor_recovery_codes (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL CHECK (char_length(code_hash) = 64),
    PRIMARY KEY (user_id, code_hash)
);
