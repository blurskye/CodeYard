-- Whether robots.txt and ai.txt block AI training crawlers, and whether HTML pages say so.
-- Blocked until an administrator allows them.
ALTER TABLE node_configuration ADD COLUMN ai_crawlers VARCHAR(10) NOT NULL DEFAULT 'block' CHECK (ai_crawlers IN ('allow', 'block'));
