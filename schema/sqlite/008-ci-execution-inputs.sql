ALTER TABLE ci_runs ADD COLUMN pull_request_event TEXT
    CHECK (pull_request_event IS NULL OR json_valid(pull_request_event));

-- Older runs have no historical snapshot; freeze the metadata still available.
UPDATE ci_runs SET pull_request_event = (
    SELECT json_object(
        'number', pull.number, 'title', pull.title, 'head_ref', pull.head_branch,
        'base_ref', pull.base_branch, 'base_sha', pull.base_sha)
    FROM pull_requests AS pull WHERE pull.id = ci_runs.pull_request_id)
WHERE event = 'pull_request' AND pull_request_id IS NOT NULL;

UPDATE ci_runs SET mode = 'unassigned' WHERE status = 'queued';
