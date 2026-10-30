CREATE TABLE actions_step_summaries (
    job_id INTEGER NOT NULL REFERENCES actions_jobs(id) ON DELETE CASCADE,
    number INTEGER NOT NULL CHECK (number > 0 AND number <= 10000),
    markdown TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK (bytes >= 0 AND bytes <= 65536),
    PRIMARY KEY (job_id, number)
);
