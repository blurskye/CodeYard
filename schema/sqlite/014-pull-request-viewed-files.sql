-- Files a reader marked as viewed on a pull request. The mark keeps the file's blob id, so a
-- later push that changes the file leaves it unviewed.
CREATE TABLE pull_request_viewed_files (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE CHECK (typeof(user_id)='integer'),
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE CHECK (typeof(pull_request_id)='integer'),
    path TEXT NOT NULL,
    blob_sha TEXT NOT NULL,
    PRIMARY KEY (user_id, pull_request_id, path)
);
