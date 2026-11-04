//! The `status` event, sent after a commit status is saved through REST or the native API.

use crate::backend::webhooks::emit;
use crate::backend::webhooks::payloads::Person;
use crate::backend::webhooks::payloads::Scope;

use crate::backend::repositories::store::CommitInfo;
use crate::backend::repositories::store::RepoStore;
use codeyard_db_core::models::CommitStatus;
use codeyard_db_core::models::Repository;
use codeyard_db_core::DbPool;

/// GitHub names at most this many branches in a status event.
const MAX_BRANCHES: usize = 10;

/// Sends the `status` event for `status`, saved by `actor` on `commit`. Lookups that fail
/// leave their fields empty; the status itself is already saved.
pub async fn emit_status(
    pool: &DbPool,
    store: &RepoStore,
    repo: &Repository,
    status: &CommitStatus,
    commit: &CommitInfo,
    actor: i64,
) {
    let branches = match store.refs().await {
        Ok(refs) => {
            let mut names: Vec<String> = refs
                .into_iter()
                .filter(|(_, sha)| *sha == commit.sha)
                .filter_map(|(name, _)| name.strip_prefix("refs/heads/").map(str::to_owned))
                .collect();
            names.sort_unstable();
            names.truncate(MAX_BRANCHES);
            names
        }
        Err(error) => {
            tracing::warn!(repo_id = repo.id, %error, "could not list branches for a status webhook");
            Vec::new()
        }
    };
    let emails = [commit.author_email.clone(), commit.committer_email.clone()];
    let accounts = crate::backend::repositories::reads::commit_authors(pool, &emails)
        .await
        .inspect_err(|error| tracing::warn!(repo_id = repo.id, %error, "could not read status commit authors"))
        .unwrap_or_default();
    let author = accounts.get(&commit.author_email).map(Person::of);
    let committer = accounts.get(&commit.committer_email).map(Person::of);
    let comments = crate::backend::db::repositories::commit_comments::comment_counts(
        pool,
        repo.id,
        std::slice::from_ref(&commit.sha),
    )
    .await
    .inspect_err(
        |error| tracing::warn!(repo_id = repo.id, %error, "could not count status commit comments"),
    )
    .unwrap_or_default()
    .into_iter()
    .find_map(|(sha, count)| (sha == commit.sha).then_some(count))
    .unwrap_or(0);
    let (status, commit, repo_for_hook) = (status.clone(), commit.clone(), repo.clone());
    emit(
        pool,
        repo,
        codeyard_model::webhook_events::STATUS,
        vec![actor],
        move |owner, people| {
            let sender = people.get(actor);
            let detail = crate::backend::webhooks::payloads::git_commit(
                &commit,
                owner,
                &repo_for_hook.name,
                repo_for_hook.id,
            );
            let commit = crate::backend::webhooks::payloads::commit(
                &commit,
                detail,
                owner,
                &repo_for_hook.name,
                repo_for_hook.id,
                comments,
                author.as_ref(),
                committer.as_ref(),
            );
            crate::backend::webhooks::payloads::status(
                &status,
                commit,
                &branches,
                Scope {
                    repo: &repo_for_hook,
                    owner,
                    sender: &sender,
                },
            )
        },
    )
    .await;
}
