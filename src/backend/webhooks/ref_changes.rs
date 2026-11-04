//! Webhook events for published branch and tag changes: `create`, `delete` and `push`.

use crate::backend::webhooks::emit;
use crate::backend::webhooks::payloads::Scope;

use codeyard_db_core::models::Repository;
use codeyard_db_core::DbPool;
use std::path::Path;

/// One ref a push moved: `(reference, old, new)`, where an all-zero id means none.
pub type RefMove = (String, String, String);

fn is_zero(id: &str) -> bool {
    id.bytes().all(|byte| byte == b'0')
}

/// The ref kind a payload names, with the short name. Other refs are not reported.
fn ref_kind(reference: &str) -> Option<(&'static str, &str)> {
    if let Some(name) = reference.strip_prefix("refs/heads/") {
        Some(("branch", name))
    } else {
        reference
            .strip_prefix("refs/tags/")
            .map(|name| ("tag", name))
    }
}

/// The tips the refs had before the push: the refs now, with each moved ref put back. `None`
/// when the refs cannot be listed within their limits.
fn previous_tips(path: &Path, moves: &[RefMove]) -> Option<Vec<String>> {
    let mut refs = match codeyard_git::list_mirror_refs(path) {
        Ok(refs) => refs,
        Err(error) => {
            tracing::warn!(%error, "could not list refs for push webhook history");
            return None;
        }
    };
    // Internal refs hold server bookkeeping, not commits anyone pushed.
    refs.retain(|name, _| !name.starts_with("refs/codeyard/"));
    for (reference, old, _) in moves {
        if is_zero(old) {
            refs.remove(reference);
        } else {
            refs.insert(reference.clone(), old.clone());
        }
    }
    let mut tips: Vec<String> = refs.into_values().collect();
    tips.sort_unstable();
    tips.dedup();
    Some(tips)
}

/// Pushed-commit listings shared by the ref moves of one push.
#[derive(Default)]
struct History {
    bytes: usize,
    listed: std::collections::HashMap<
        (String, String),
        crate::backend::webhooks::payloads::PushedCommits,
    >,
    /// Read once, on the first listing; the outer `None` means not read yet.
    previous: Option<Option<Vec<String>>>,
}

/// Sends the events for the accepted ref moves of one push, after Git has updated the refs.
pub async fn emit_ref_moves(
    pool: &DbPool,
    repo: &Repository,
    pusher_id: i64,
    path: &Path,
    moves: &[RefMove],
) {
    let history = std::sync::Arc::new(std::sync::Mutex::new(History::default()));
    let all_moves: std::sync::Arc<[RefMove]> = moves.into();
    for (reference, old, new) in moves {
        let Some((ref_type, name)) = ref_kind(reference) else {
            continue;
        };
        let (reference, old, new) = (reference.clone(), old.clone(), new.clone());
        let name = name.to_owned();
        let check_request = (ref_type == "branch" && !is_zero(&new) && new != old)
            .then(|| (name.clone(), new.clone()));
        let push_path = path.to_owned();
        if is_zero(&old) {
            emit_ref_change(
                pool,
                repo,
                codeyard_model::webhook_events::CREATE,
                ref_type,
                name,
                pusher_id,
            )
            .await;
        } else if is_zero(&new) {
            emit_ref_change(
                pool,
                repo,
                codeyard_model::webhook_events::DELETE,
                ref_type,
                name,
                pusher_id,
            )
            .await;
        }
        let repo_for_hook = repo.clone();
        let (head_sender, head_receiver) = tokio::sync::oneshot::channel();
        let history = history.clone();
        let all_moves = all_moves.clone();
        emit(
            pool,
            repo,
            codeyard_model::webhook_events::PUSH,
            vec![pusher_id],
            move |owner, people| {
                let sender = people.get(pusher_id);
                let before = (!is_zero(&old)).then_some(old.as_str());
                let key = (old.clone(), new.clone());
                let mut history = history.lock().unwrap_or_else(|error| error.into_inner());
                let history = &mut *history;
                let commits = if is_zero(&new) {
                    crate::backend::webhooks::payloads::PushedCommits::default()
                } else if let Some(commits) = history.listed.get(&key) {
                    commits.clone()
                } else {
                    let previous = history
                        .previous
                        .get_or_insert_with(|| previous_tips(&push_path, &all_moves))
                        .as_deref();
                    let commits = match crate::backend::webhooks::payloads::pushed_commits(
                        &push_path, before, &new, previous,
                    ) {
                        Ok(commits) => commits,
                        Err(error) => {
                            tracing::warn!(%error, "could not read push webhook history");
                            return serde_json::Value::Null;
                        }
                    };
                    let bytes: usize = commits
                        .commits
                        .iter()
                        .chain(commits.head.iter())
                        .map(|commit| {
                            std::mem::size_of::<crate::backend::webhooks::payloads::PushedCommit>()
                                + commit.id.len()
                                + commit.tree_id.len()
                                + commit.message.len()
                                + commit.author_name.len()
                                + commit.author_email.len()
                                + commit.committer_name.len()
                                + commit.committer_email.len()
                                + commit.files.as_ref().map_or(
                                    0,
                                    crate::backend::webhooks::payloads::ChangedFiles::bytes,
                                )
                        })
                        .sum();
                    if history.bytes.saturating_add(bytes) <= 16 * 1024 * 1024 {
                        history.bytes += bytes;
                        history.listed.insert(key, commits.clone());
                    }
                    commits
                };
                let _ = head_sender.send(commits.head.clone());
                crate::backend::webhooks::payloads::push(
                    &reference,
                    &old,
                    &new,
                    &commits,
                    &repo_for_hook,
                    owner,
                    &sender,
                )
            },
        )
        .await;
        if let Some((branch, sha)) = check_request {
            match head_receiver.await {
                Ok(Some(head)) => {
                    crate::backend::api::github::checks::requested_after_push(
                        pool, repo, pusher_id, &sha, &branch, &head,
                    )
                    .await;
                }
                _ => tracing::warn!(
                    repo_id = repo.id,
                    "Accepted push head could not be retained for automatic checks"
                ),
            }
        }
    }
}

async fn emit_ref_change(
    pool: &DbPool,
    repo: &Repository,
    event: &'static str,
    ref_type: &'static str,
    name: String,
    pusher_id: i64,
) {
    let repo_for_hook = repo.clone();
    emit(pool, repo, event, vec![pusher_id], move |owner, people| {
        let sender = people.get(pusher_id);
        crate::backend::webhooks::payloads::ref_change(
            ref_type,
            &name,
            Scope {
                repo: &repo_for_hook,
                owner,
                sender: &sender,
            },
        )
    })
    .await;
}
