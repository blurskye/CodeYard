//! Repository read selection shared by browser and JSON adapters.
use crate::backend::{
    cluster::NodeContext,
    db,
    repositories::store::{repo_store, RepoError},
};
use std::collections::HashMap;

/// Accounts associated with Git email metadata, including Codeyard's generated noreply addresses.
/// Association is not signature verification and grants no repository authority.
pub async fn commit_authors(
    pool: &db::DbPool,
    emails: &[String],
) -> anyhow::Result<HashMap<String, db::models::User>> {
    anyhow::ensure!(emails.len() <= 512, "Too many commit email addresses");
    let usernames = emails
        .iter()
        .filter_map(|email| email.strip_suffix("@users.noreply").map(str::to_owned))
        .collect::<Vec<_>>();
    let mut users = db::users::accounts::get_users_by_emails(pool, emails).await?;
    users.extend(db::users::accounts::get_users_by_usernames(pool, &usernames).await?);
    let mut result = HashMap::new();
    for user in &users {
        if let Some(email) = &user.email {
            result.insert(email.clone(), user.clone());
        }
    }
    // The generated namespace belongs to the account, not to someone claiming it as an email.
    for user in users {
        result.insert(format!("{}@users.noreply", user.username), user);
    }
    Ok(result)
}

/// Prefer `stored` when Git has that branch, then `database`, then `main`, `master`, or the
/// first branch by name.
pub fn default_branch(refs: &HashMap<String, String>, stored: &str, database: &str) -> String {
    if refs.is_empty() || refs.contains_key(&format!("refs/heads/{stored}")) {
        return stored.to_owned();
    }
    if refs.contains_key(&format!("refs/heads/{database}")) {
        return database.to_owned();
    }
    let branches = refs
        .keys()
        .filter_map(|name| name.strip_prefix("refs/heads/"));
    preferred_branch(branches).map_or_else(|| stored.to_owned(), str::to_owned)
}

/// The branch to use when the stored name is missing: `main`, then `master`, then
/// the first branch by name.
pub fn preferred_branch<'a>(branches: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let branches: Vec<&str> = branches.into_iter().collect();
    ["main", "master"]
        .into_iter()
        .find(|name| branches.contains(name))
        .or_else(|| branches.iter().min().copied())
}

pub async fn refs_and_default(
    repository: &db::models::Repository,
    cluster: Option<&NodeContext>,
) -> Result<(HashMap<String, String>, String), RepoError> {
    repo_store(repository, cluster).refs_and_default().await
}

/// Repositories in one fork network may be compared, including sibling and nested forks.
pub async fn same_fork_network(
    pool: &db::DbPool,
    left: &db::models::Repository,
    right: &db::models::Repository,
) -> anyhow::Result<bool> {
    async fn root(pool: &db::DbPool, repository: &db::models::Repository) -> anyhow::Result<i64> {
        let mut current = repository.clone();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            anyhow::ensure!(seen.insert(current.id), "Cyclic fork network");
            let Some(parent) = current.forked_from_id else {
                return Ok(current.id);
            };
            current = db::repositories::records::get_repository_by_id(pool, parent)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Fork parent not found"))?;
        }
        anyhow::bail!("Fork network exceeds its limit")
    }
    if left.id == right.id {
        return Ok(true);
    }
    Ok(root(pool, left).await? == root(pool, right).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_branch_preserves_empty_and_named_ref_selection() {
        assert_eq!(default_branch(&HashMap::new(), "trunk", "main"), "trunk");
        let refs = HashMap::from([
            ("refs/heads/topic".into(), "sha".into()),
            ("refs/heads/main".into(), "sha".into()),
        ]);
        assert_eq!(default_branch(&refs, "topic", "main"), "topic");
        assert_eq!(default_branch(&refs, "missing", "topic"), "topic");
        assert_eq!(default_branch(&refs, "missing", "absent"), "main");
    }

    #[test]
    fn preferred_branch_is_main_then_master_then_first_by_name() {
        assert_eq!(preferred_branch(["trunk", "master", "main"]), Some("main"));
        assert_eq!(preferred_branch(["trunk", "master"]), Some("master"));
        assert_eq!(preferred_branch(["trunk", "feature"]), Some("feature"));
        assert_eq!(preferred_branch(Vec::<&str>::new()), None);
    }
}
