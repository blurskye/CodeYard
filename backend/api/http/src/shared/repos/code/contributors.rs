use codeyard_db_core::models::User;
use codeyard_db_core::DbPool;
use codeyard_git::ContributorInfo;
use std::collections::HashMap;
/// The account behind each contributor's email, by email, under the same rules as commit
/// attribution.
pub async fn contributor_accounts(
    pool: &DbPool,
    contributors: &[ContributorInfo],
) -> anyhow::Result<HashMap<String, User>> {
    let emails: Vec<_> = contributors
        .iter()
        .map(|contributor| contributor.email.clone())
        .collect();
    let mut users = HashMap::new();
    for chunk in emails.chunks(512) {
        users.extend(codeyard_repos_core::repositories::reads::commit_authors(pool, chunk).await?);
    }
    Ok(users)
}
