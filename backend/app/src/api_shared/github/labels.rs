use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
pub async fn item(
    pool: &codeyard_db_core::DbPool,
    repo_id: i64,
    number: &str,
) -> Result<codeyard_db_repos::assignees::AssignedItem> {
    let number = number.parse().map_err(|_| Error::missing())?;
    if let Some(issue) = codeyard_db_repos::issues::get_issue(pool, repo_id, number)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(codeyard_db_repos::assignees::AssignedItem::Issue(issue.id));
    }
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request(pool, repo_id, number)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(codeyard_db_repos::assignees::AssignedItem::PullRequest(
        pull.id,
    ))
}
