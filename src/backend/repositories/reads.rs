//! Repository read selection shared by browser and JSON adapters.
use crate::backend::{
    cluster::NodeContext,
    db,
    repositories::store::{repo_store, RepoError},
};
use std::collections::HashMap;

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
    let store = repo_store(repository, cluster);
    let refs = store.refs().await?;
    let info = store.info().await?;
    Ok((refs, info.default_branch))
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
