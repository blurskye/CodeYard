//! Rebase stack: moves the open layers of a stack onto the layers below them. Each layer replays
//! its own commits onto the new head of the layer below, bottom-up. Every head moves in one ref
//! transaction after all layers have replayed, so a conflict or a failed check moves nothing.

use super::merge::{open_chain, prepare_error};
use super::workflows::MOVED_CI_WAIT;
use super::{actor, branches_moved, require_write, OperationError};
use crate::backend::ci::{CiRuns, RefUpdate};
use crate::backend::db;
use crate::backend::db::models::PullRequest;
use crate::backend::db::pull_requests::records::MAX_STACK_LAYERS;
use crate::backend::repositories::store::{
    repo_store, ErrorKind, RepoError, RepoStore, RestackMove,
};
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

/// The most time the replays of one rebase may take together. Each replay gets what is left of it.
const RESTACK_PREPARE_DEADLINE: Duration = Duration::from_secs(120);

/// How many layers a rebase moved.
#[derive(Serialize)]
pub struct RestackResponse {
    pub rebased: usize,
}

/// Rebases the open line of the stack that pull request `number` is in. The rebase starts at the
/// lowest layer of the line that is not on top of its base branch, and moves every open layer above
/// it, so any layer of the stack can ask for it. Layers below that layer do not move. When nothing
/// needs to move, nothing is written and the answer is zero.
pub async fn rebase_stack(
    pool: &db::DbPool,
    ci: &CiRuns,
    actor_id: i64,
    (owner, repo, number): (String, String, i32),
) -> Result<RestackResponse, OperationError> {
    let deadline = tokio::time::Instant::now() + RESTACK_PREPARE_DEADLINE;
    let repository = db::repositories::records::get_repository(pool, &owner, &repo)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })?;
    let user = actor(pool, actor_id).await?;
    require_write(pool, &user, &repository).await?;
    if repository.is_archived {
        return Err(OperationError::Denied {
            code: "archived",
            message: "Archived repositories cannot be restacked.".into(),
        });
    }
    let pull = db::pull_requests::records::get_pull_request(pool, repository.id, number)
        .await
        .map_err(database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Pull request not found".into(),
        })?;
    if pull.state != "open" {
        return Err(OperationError::Invalid {
            code: "invalid_state",
            message: "Pull request is not open".into(),
        });
    }
    let Some(stack_id) = pull.stack_id else {
        return Err(OperationError::Conflict {
            code: "not_stacked",
            message: "This pull request is not in a stack.".into(),
        });
    };
    // A closed or cyclic layer below refuses the rebase, as it refuses a merge.
    open_chain(pool, &pull).await?;
    let members = db::pull_requests::records::list_stack_members(pool, repository.id, stack_id)
        .await
        .map_err(database_error)?;
    let store = repo_store(&repository, ci.cluster.as_ref());
    let refs = store.refs().await.map_err(git_unavailable)?;
    let moving = layers_to_move(&store, &refs, &pull, &members).await?;
    let Some(first) = moving.first() else {
        return Ok(RestackResponse { rebased: 0 });
    };
    let base_tip = tip_of(&refs, &first.base_branch)
        .ok_or_else(base_branch_missing)?
        .clone();
    let mut onto = base_tip.clone();
    let email = user.commit_email();
    let mut moves: Vec<RestackMove> = Vec::new();
    for layer in &moving {
        if layer.head_repo_id != Some(repository.id) {
            return Err(OperationError::Conflict {
                code: "stack_fork_head",
                message: format!(
                    "The head of #{} is in a fork. Rebase stack moves only branches of this repository.",
                    layer.number
                ),
            });
        }
        let head = tip_of(&refs, &layer.head_branch)
            .ok_or_else(|| head_branch_missing(layer.number))?
            .clone();
        let new_head = if is_ancestor(&store, &onto, &head).await? {
            head.clone()
        } else {
            let from = own_commits_base(&store, layer, &members, &head).await?;
            let replay = store.prepare_restack(&from, &head, &onto, &user.username, &email);
            match tokio::time::timeout_at(deadline, replay).await {
                Ok(prepared) => prepared.map_err(|error| rebase_failure(layer.number, error))?,
                Err(_) => return Err(prepare_timeout()),
            }
        };
        if new_head != head {
            moves.push(RestackMove {
                branch: layer.head_branch.clone(),
                old: head,
                new: new_head.clone(),
            });
        }
        onto = new_head;
    }
    if moves.is_empty() {
        return Ok(RestackResponse { rebased: 0 });
    }

    store
        .publish_restack_as(
            pool,
            repository.id,
            actor_id,
            &first.base_branch,
            &base_tip,
            &moves,
        )
        .await
        .map_err(publish_failure)?;
    // The refs have moved, so the follow-up is best effort: the moved heads get the refresh and the
    // push workflows of a push, within the budget a merge gives its own queueing. A refresh that does
    // not finish is caught up by the next one.
    let follow_up = tokio::time::timeout(MOVED_CI_WAIT, async {
        branches_moved(pool, &repository, ci, actor_id).await;
        for restack in &moves {
            ci.queue_push_runs(
                &repository,
                &owner,
                actor_id,
                RefUpdate {
                    reference: format!("refs/heads/{}", restack.branch),
                    old: Some(restack.old.clone()),
                    new: restack.new.clone(),
                    changed_paths: None,
                },
            )
            .await;
        }
    })
    .await;
    if follow_up.is_err() {
        tracing::warn!(
            repo_id = repository.id,
            count = moves.len(),
            "the follow-up of a rebase did not finish in time"
        );
    }
    Ok(RestackResponse {
        rebased: moves.len(),
    })
}

/// The open line from `bottom` up: `bottom` and the open layers stacked above it, bottom first. A
/// closed layer ends the walk, since its own layers are not part of the open stack.
fn open_line(
    bottom: &PullRequest,
    members: &[PullRequest],
) -> Result<Vec<PullRequest>, OperationError> {
    let mut layers = vec![bottom.clone()];
    loop {
        let current = layers[layers.len() - 1].clone();
        let above: Vec<&PullRequest> = members
            .iter()
            .filter(|member| {
                member.state == "open"
                    && member.id != current.id
                    && member.base_branch == current.head_branch
            })
            .collect();
        match above.as_slice() {
            [] => return Ok(layers),
            [next]
                if layers.len() < MAX_STACK_LAYERS
                    && layers.iter().all(|layer| layer.id != next.id) =>
            {
                layers.push((*next).clone());
            }
            _ => return Err(stack_broken()),
        }
    }
}

/// The open layer at the bottom of the line that `pull` is in. It walks down through the open layers
/// that each layer is based on, and stops at a layer based on anything else.
fn bottom_of(pull: &PullRequest, members: &[PullRequest]) -> Result<PullRequest, OperationError> {
    let mut bottom = pull.clone();
    let mut seen = vec![pull.id];
    loop {
        let (id, base) = (bottom.id, bottom.base_branch.clone());
        let Some(below) = members
            .iter()
            .find(|member| member.state == "open" && member.id != id && member.head_branch == base)
        else {
            return Ok(bottom);
        };
        if seen.contains(&below.id) || seen.len() >= MAX_STACK_LAYERS {
            return Err(stack_broken());
        }
        seen.push(below.id);
        bottom = below.clone();
    }
}

/// The layers a rebase moves: the open line of `pull`, from its first layer that is not on top of its
/// base branch up to the top. Empty when every layer of the line is on top of its base.
async fn layers_to_move(
    store: &RepoStore,
    refs: &HashMap<String, String>,
    pull: &PullRequest,
    members: &[PullRequest],
) -> Result<Vec<PullRequest>, OperationError> {
    let line = open_line(&bottom_of(pull, members)?, members)?;
    for (index, layer) in line.iter().enumerate() {
        if is_stale(store, refs, layer).await? {
            return Ok(line[index..].to_vec());
        }
    }
    Ok(Vec::new())
}

/// Whether `layer`'s head lacks the current tip of its base branch, so that a rebase would move it.
/// A layer whose base or head is not among `refs` is not stale here: its callers refuse it on their own.
async fn is_stale(
    store: &RepoStore,
    refs: &HashMap<String, String>,
    layer: &PullRequest,
) -> Result<bool, OperationError> {
    let (Some(base), Some(head)) = (
        tip_of(refs, &layer.base_branch),
        tip_of(refs, &layer.head_branch),
    ) else {
        return Ok(false);
    };
    Ok(!is_ancestor(store, base, head).await?)
}

fn stack_broken() -> OperationError {
    OperationError::Conflict {
        code: "stack_broken",
        message: "These pull requests do not form one line, so they cannot be restacked.".into(),
    }
}

fn prepare_timeout() -> OperationError {
    OperationError::Unavailable {
        code: "git_unavailable",
        message:
            "Rebasing these layers took too long, so nothing moved. Rebase fewer layers, or push a rebased branch and rebase the rest."
                .into(),
    }
}

/// The commit that the own commits of `layer` start after. A layer based on the head of an open
/// layer starts after the base it recorded. A layer that was moved onto trunk after the layer below
/// it was merged by squash or rebase starts after the head of that merged layer, because the
/// merged commits are not in trunk under their own names.
async fn own_commits_base(
    store: &RepoStore,
    layer: &PullRequest,
    members: &[PullRequest],
    head: &str,
) -> Result<String, OperationError> {
    let stacked_on_open = members.iter().any(|member| {
        member.state == "open" && member.id != layer.id && member.head_branch == layer.base_branch
    });
    if stacked_on_open {
        // Once the base has moved past the one the layer was built on, the fork point is the base
        // it was built on. Replaying from the new base would replay the commits of the layer below.
        if let Some(fork) = &layer.fork_sha {
            if !is_ancestor(store, &layer.base_sha, head).await? {
                return Ok(fork.clone());
            }
        }
        return Ok(layer.base_sha.clone());
    }
    let mut merged_below: Option<&PullRequest> = None;
    for member in members
        .iter()
        .filter(|member| member.state == "merged" && member.id != layer.id)
    {
        if !is_ancestor(store, &member.head_sha, head).await? {
            continue;
        }
        let nearer = match merged_below {
            Some(best) => is_ancestor(store, &best.head_sha, &member.head_sha).await?,
            None => true,
        };
        if nearer {
            merged_below = Some(member);
        }
    }
    Ok(match merged_below {
        Some(merged) => merged.head_sha.clone(),
        None => layer.base_sha.clone(),
    })
}

/// Whether a squash or rebase of a merged layer below `bottom` left the open `bottom` behind trunk: a
/// merged layer is in its history, but its commits are not in trunk under their own names. A layer
/// that already contains the current tip of its base sits on trunk, and merging it only adds its own
/// changes, so it is not behind. The tips are read from `refs`, as `layers_to_move` reads them.
pub(super) async fn squashed_below(
    pool: &db::DbPool,
    store: &RepoStore,
    refs: &HashMap<String, String>,
    bottom: &PullRequest,
) -> Result<bool, OperationError> {
    if !is_stale(store, refs, bottom).await? {
        return Ok(false);
    }
    let Some(stack_id) = bottom.stack_id else {
        return Ok(false);
    };
    let Some(base_tip) = tip_of(refs, &bottom.base_branch) else {
        return Ok(false);
    };
    let members = db::pull_requests::records::list_stack_members(pool, bottom.repo_id, stack_id)
        .await
        .map_err(database_error)?;
    for member in members.iter().filter(|member| member.state == "merged") {
        if is_ancestor(store, &member.head_sha, &bottom.head_sha).await?
            && !is_ancestor(store, &member.head_sha, base_tip).await?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether Rebase stack would move a layer of the open line that `pull` is in. The page shows its
/// button for this only, so it is advisory: a failed read is logged and reads as no button, and the
/// page still renders. Publication checks the same things on the primary.
pub async fn restack_pending(
    ci: &CiRuns,
    repository: &db::models::Repository,
    pull: &PullRequest,
    members: &[PullRequest],
) -> bool {
    // Only an open layer can be restacked, so a closed or merged one offers no button, and reads no git.
    if pull.state != "open" {
        return false;
    }
    match pending(ci, repository, pull, members).await {
        Ok(hint) => hint,
        Err(error) => {
            tracing::debug!(pull_request_id = pull.id, %error, "restack hint unavailable; no Rebase stack button");
            false
        }
    }
}

async fn pending(
    ci: &CiRuns,
    repository: &db::models::Repository,
    pull: &PullRequest,
    members: &[PullRequest],
) -> Result<bool, OperationError> {
    let store = repo_store(repository, ci.cluster.as_ref());
    let refs = store.refs().await.map_err(git_unavailable)?;
    Ok(!layers_to_move(&store, &refs, pull, members)
        .await?
        .is_empty())
}

/// Whether `ancestor` is in the history of `descendant`, or is that commit.
async fn is_ancestor(
    store: &RepoStore,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, OperationError> {
    store
        .is_ancestor(ancestor, descendant)
        .await
        .map_err(git_unavailable)
}

fn tip_of<'a>(refs: &'a HashMap<String, String>, branch: &str) -> Option<&'a String> {
    refs.get(&format!("refs/heads/{branch}"))
}

fn base_branch_missing() -> OperationError {
    OperationError::Conflict {
        code: "base_branch_missing",
        message: "The base branch no longer exists. Restore it to rebase this stack.".into(),
    }
}

fn head_branch_missing(number: i32) -> OperationError {
    OperationError::Conflict {
        code: "head_branch_missing",
        message: format!(
            "The head branch of #{number} no longer exists. Restore it to rebase this stack."
        ),
    }
}

/// A replay conflict names the pull request and its files. Other replay failures are the same
/// as for a merge.
fn rebase_failure(number: i32, error: RepoError) -> OperationError {
    if error.kind == ErrorKind::Conflict {
        OperationError::Conflict {
            code: "rebase_conflict",
            message: format!(
                "Rebasing pull request #{number} conflicts. {}",
                error.message
            ),
        }
    } else {
        prepare_error(error)
    }
}

/// Maps a failed publication. A branch rule's refusal keeps the rule's own text, and a stale head or
/// base is a revision conflict. Nothing moved in either case.
fn publish_failure(error: RepoError) -> OperationError {
    if error.is_write_denied() {
        OperationError::Denied {
            code: "write_denied",
            message: "Repository write authority changed; rebase denied.".into(),
        }
    } else if error.is_protected_branch() || error.is_merge_commit_blocked() {
        OperationError::Denied {
            code: "branch_protection",
            message: error.message,
        }
    } else if error.is_storage_node_unavailable() {
        OperationError::Unavailable {
            code: "storage_unavailable",
            message: "storage node unavailable".into(),
        }
    } else {
        OperationError::Conflict {
            code: "revision_conflict",
            message: "A branch changed while rebasing. Reload the pull request and try again."
                .into(),
        }
    }
}

fn database_error<E>(_error: E) -> OperationError {
    OperationError::Unavailable {
        code: "database_error",
        message: "Collaboration storage is temporarily unavailable".into(),
    }
}

fn git_unavailable<E>(_error: E) -> OperationError {
    OperationError::Unavailable {
        code: "git_unavailable",
        message: "Repository storage is unavailable; rebase denied.".into(),
    }
}
