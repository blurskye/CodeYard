-- Repository wiki. A page keeps its head revision number; its current content is the revision with
-- that number. Renamed pages keep their old slugs as redirects.
ALTER TABLE repositories ADD COLUMN IF NOT EXISTS wiki_enabled BOOLEAN NOT NULL DEFAULT TRUE;

CREATE TABLE IF NOT EXISTS wiki_pages (
    id BIGSERIAL PRIMARY KEY,
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    slug VARCHAR(200) NOT NULL CHECK (char_length(slug) >= 1),
    revision_count BIGINT NOT NULL DEFAULT 0 CHECK (revision_count >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo_id, slug)
);

CREATE TABLE IF NOT EXISTS wiki_revisions (
    id BIGSERIAL PRIMARY KEY,
    page_id BIGINT NOT NULL REFERENCES wiki_pages(id) ON DELETE CASCADE,
    number BIGINT NOT NULL CHECK (number >= 1),
    title VARCHAR(200) NOT NULL CHECK (char_length(title) >= 1),
    body TEXT NOT NULL CHECK (octet_length(body) <= 1048576),
    message VARCHAR(500) NOT NULL DEFAULT '',
    author_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(page_id, number)
);

CREATE TABLE IF NOT EXISTS wiki_redirects (
    repo_id BIGINT NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    old_slug VARCHAR(200) NOT NULL CHECK (char_length(old_slug) >= 1),
    page_id BIGINT NOT NULL REFERENCES wiki_pages(id) ON DELETE CASCADE,
    PRIMARY KEY (repo_id, old_slug)
);
