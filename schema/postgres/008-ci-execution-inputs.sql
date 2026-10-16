ALTER TABLE ci_runs ADD COLUMN pull_request_event JSONB;

-- Older runs have no historical snapshot; freeze the metadata still available.
UPDATE ci_runs AS run SET pull_request_event = jsonb_build_object(
    'number', pull.number, 'title', pull.title, 'head_ref', pull.head_branch,
    'base_ref', pull.base_branch, 'base_sha', pull.base_sha)
FROM pull_requests AS pull
WHERE run.event = 'pull_request' AND run.pull_request_id = pull.id;

UPDATE ci_runs SET mode = 'unassigned' WHERE status = 'queued';
