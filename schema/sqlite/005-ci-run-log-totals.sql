-- Stored log totals per run. A log append checks the run limit against these
-- columns instead of counting the run's lines on every append. Runs that already
-- exist start at zero.
ALTER TABLE ci_runs ADD COLUMN log_lines INTEGER NOT NULL DEFAULT 0 CHECK (log_lines >= 0);
ALTER TABLE ci_runs ADD COLUMN log_bytes INTEGER NOT NULL DEFAULT 0 CHECK (log_bytes >= 0);
