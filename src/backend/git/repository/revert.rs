//! Build an inverse change without moving any branch; normal publication checks still apply.
use super::checked::{commit_is_ancestor, commit_tree, merge_tree_with_base, tree_of};
use super::*;

/// The inverse of `merged`, as a commit on top of `current`. The merge runs in a Git child with
/// the memory limit and timeout, because the blobs it compares can be huge.
pub(crate) async fn prepare_revert(
    path: &Path,
    (current, merged, before): (&str, &str, &str),
    (message, name, email): (&str, &str, &str),
) -> Result<String> {
    for sha in [current, merged, before] {
        validate_merge_revision("main", sha)?;
    }
    ensure!(
        current.eq_ignore_ascii_case(merged) || commit_is_ancestor(path, merged, current).await?,
        UnsupportedMerge("The merged change is no longer in the base branch".into())
    );
    let tree = merge_tree_with_base(path, merged, current, before).await?;
    ensure!(
        !tree.eq_ignore_ascii_case(&tree_of(path, current).await?),
        UnsupportedMerge("The change is already reverted".into())
    );
    let person = Person {
        name,
        email,
        date: None,
    };
    commit_tree(
        path,
        &tree,
        &[current],
        message.as_bytes(),
        &person,
        &person,
    )
    .await
}
