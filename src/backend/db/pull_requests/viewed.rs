//! Shared named operation surface for the files a reader marked as viewed on a pull request.
use crate::backend::db::DbPool;
use anyhow::Result;

native_operation!(crate::backend::db::postgres::pull_requests::viewed::set_pull_request_file_viewed,
    crate::backend::db::sqlite::pull_requests::viewed::set_pull_request_file_viewed,
    fn set_pull_request_file_viewed(pool: &DbPool, pr_id: i64, user_id: i64, path: &str, blob_sha: Option<&str>) -> Result<()>);

native_operation!(crate::backend::db::postgres::pull_requests::viewed::list_pull_request_viewed_files,
    crate::backend::db::sqlite::pull_requests::viewed::list_pull_request_viewed_files,
    fn list_pull_request_viewed_files(pool: &DbPool, pr_id: i64, user_id: i64, paths: &[String]) -> Result<Vec<(String, String)>>);
