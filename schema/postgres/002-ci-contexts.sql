-- Full workflow paths must not be shortened into colliding check names.
ALTER TABLE commit_statuses ALTER COLUMN context TYPE TEXT;
