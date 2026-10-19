-- Repository wiki. A page keeps its head revision number; its current content is the revision with
-- that number. Renamed pages keep their old slugs as redirects.
ALTER TABLE repositories ADD COLUMN wiki_enabled INTEGER NOT NULL DEFAULT 1 CHECK (wiki_enabled IN (0,1));

CREATE TABLE wiki_pages (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (typeof(repo_id)='integer'),
    slug TEXT NOT NULL CHECK (length(slug) BETWEEN 1 AND 200),
    revision_count INTEGER NOT NULL DEFAULT 0 CHECK (typeof(revision_count)='integer' AND revision_count >= 0),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(repo_id, slug)
);

CREATE TABLE wiki_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id IS NULL OR typeof(id)='integer'),
    page_id INTEGER NOT NULL REFERENCES wiki_pages(id) ON DELETE CASCADE CHECK (typeof(page_id)='integer'),
    number INTEGER NOT NULL CHECK (typeof(number)='integer' AND number >= 1),
    title TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 200),
    body TEXT NOT NULL CHECK (length(CAST(body AS BLOB)) <= 1048576),
    message TEXT NOT NULL DEFAULT '' CHECK (length(message) <= 500),
    author_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(author_id)='integer'),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000Z','now')),
    UNIQUE(page_id, number)
);

CREATE TABLE wiki_redirects (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE CHECK (typeof(repo_id)='integer'),
    old_slug TEXT NOT NULL CHECK (length(old_slug) BETWEEN 1 AND 200),
    page_id INTEGER NOT NULL REFERENCES wiki_pages(id) ON DELETE CASCADE CHECK (typeof(page_id)='integer'),
    PRIMARY KEY (repo_id, old_slug)
);
