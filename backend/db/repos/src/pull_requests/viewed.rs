use anyhow::Result;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

native_operation!(crate::postgres::pull_requests::viewed::set_pull_request_file_viewed,
    crate::sqlite::pull_requests::viewed::set_pull_request_file_viewed,
    fn set_pull_request_file_viewed(pool: &DbPool, pr_id: i64, user_id: i64, path: &str, blob_sha: Option<&str>) -> Result<()>);

native_operation!(crate::postgres::pull_requests::viewed::list_pull_request_viewed_files,
    crate::sqlite::pull_requests::viewed::list_pull_request_viewed_files,
    fn list_pull_request_viewed_files(pool: &DbPool, pr_id: i64, user_id: i64, paths: &[String]) -> Result<Vec<(String, String)>>);
