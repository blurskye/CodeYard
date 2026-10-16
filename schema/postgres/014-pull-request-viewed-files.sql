-- Files a reader marked as viewed on a pull request. The mark keeps the file's blob id, so a
-- later push that changes the file leaves it unviewed.
CREATE TABLE pull_request_viewed_files (
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    pull_request_id BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    blob_sha TEXT NOT NULL,
    PRIMARY KEY (user_id, pull_request_id, path)
);
