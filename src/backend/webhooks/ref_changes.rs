//! Webhook events for branch and tag changes pushed over Git: `create`, `delete` and `push`,
//! as GitHub sends them. Branch and tag actions made in the web UI or the API are not wired
//! to these events yet.
use super::payloads::{self, Scope};
use super::{emit, events};
use crate::backend::db::models::Repository;
use crate::backend::db::DbPool;
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

/// Sends the events for the accepted ref moves of one push, after Git has updated the refs.
pub fn emit_ref_moves(
    pool: &DbPool,
    repo: &Repository,
    pusher_id: i64,
    path: &Path,
    moves: &[RefMove],
) {
    for (reference, old, new) in moves {
        let Some((ref_type, name)) = ref_kind(reference) else {
            continue;
        };
        let (reference, old, new) = (reference.clone(), old.clone(), new.clone());
        let name = name.to_owned();
        let path = path.to_owned();
        if is_zero(&old) {
            emit_ref_change(pool, repo, events::CREATE, ref_type, name, pusher_id);
        } else if is_zero(&new) {
            emit_ref_change(pool, repo, events::DELETE, ref_type, name, pusher_id);
        }
        let repo_for_hook = repo.clone();
        emit(
            pool,
            repo,
            events::PUSH,
            vec![pusher_id],
            move |owner, people| {
                let sender = people.get(pusher_id);
                let before = (!is_zero(&old)).then_some(old.as_str());
                let commits = if is_zero(&new) {
                    Vec::new()
                } else {
                    payloads::pushed_commits(&path, before, &new)
                };
                payloads::push(
                    &reference,
                    &old,
                    &new,
                    &commits,
                    &repo_for_hook,
                    owner,
                    &sender,
                )
            },
        );
    }
}

fn emit_ref_change(
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
        payloads::ref_change(
            ref_type,
            &name,
            Scope {
                repo: &repo_for_hook,
                owner,
                sender: &sender,
            },
        )
    });
}
