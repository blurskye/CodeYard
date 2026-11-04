//! Repository read selection shared by browser and JSON adapters.
use crate::cluster::context::NodeContext;
use crate::repositories::store::repo_store;
use codeyard_git::error::RepoError;
use std::collections::HashMap;

/// Accounts associated with Git email metadata, including Codeyard's generated noreply addresses.
/// Association is not signature verification and grants no repository authority.
pub async fn commit_authors(
    pool: &codeyard_db_core::DbPool,
    emails: &[String],
) -> anyhow::Result<HashMap<String, codeyard_db_core::models::User>> {
    anyhow::ensure!(emails.len() <= 512, "Too many commit email addresses");
    Ok(
        codeyard_db_identity::users::accounts::commit_authors(pool, emails)
            .await?
            .into_iter()
            .map(|author| (author.address, author.user))
            .collect(),
    )
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
    repository: &codeyard_db_core::models::Repository,
    cluster: Option<&NodeContext>,
) -> Result<(HashMap<String, String>, String), RepoError> {
    repo_store(repository, cluster).refs_and_default().await
}

/// Repositories in one fork network may be compared, including sibling and nested forks.
pub async fn same_fork_network(
    pool: &codeyard_db_core::DbPool,
    left: &codeyard_db_core::models::Repository,
    right: &codeyard_db_core::models::Repository,
) -> anyhow::Result<bool> {
    async fn root(
        pool: &codeyard_db_core::DbPool,
        repository: &codeyard_db_core::models::Repository,
    ) -> anyhow::Result<i64> {
        let mut current = repository.clone();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            anyhow::ensure!(seen.insert(current.id), "Cyclic fork network");
            let Some(parent) = current.forked_from_id else {
                return Ok(current.id);
            };
            current = codeyard_db_repos::repositories::records::get_repository_by_id(pool, parent)
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
