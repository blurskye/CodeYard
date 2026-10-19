-- Authenticator-app second factor. The 0001 table was never written by application code, so it is
-- replaced: the encrypted TOTP secret, the last accepted time step (a code works once) and the hashed
-- one-time recovery codes. users.two_factor_enabled is kept in step.
DROP TABLE two_factor_auth;

CREATE TABLE two_factor_auth (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    totp_secret_encrypted BLOB NOT NULL,
    totp_nonce BLOB NOT NULL,
    last_used_step INTEGER CHECK (last_used_step IS NULL OR typeof(last_used_step)='integer'),
    enabled_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
);

CREATE TABLE two_factor_recovery_codes (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    code_hash TEXT NOT NULL CHECK (length(code_hash)=64),
    PRIMARY KEY (user_id, code_hash)
);
