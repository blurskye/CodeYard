-- Stored log totals per run. A log append checks the run limit against these
-- columns instead of counting the run's lines on every append. Runs that already
-- exist start at zero.
ALTER TABLE ci_runs
    ADD COLUMN log_lines BIGINT NOT NULL DEFAULT 0 CHECK (log_lines >= 0),
    ADD COLUMN log_bytes BIGINT NOT NULL DEFAULT 0 CHECK (log_bytes >= 0);
