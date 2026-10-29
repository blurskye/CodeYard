-- CI retains full tag refs and short branch names admitted by the Git ref owner.
ALTER TABLE ci_runs ALTER COLUMN branch TYPE VARCHAR(1024);
