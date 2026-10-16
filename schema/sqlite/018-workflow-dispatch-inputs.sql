-- The inputs a manual run was started with: a JSON object of strings, as GitHub's event.inputs.
ALTER TABLE ci_runs ADD COLUMN workflow_inputs TEXT CHECK (workflow_inputs IS NULL OR json_valid(workflow_inputs));
