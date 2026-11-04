//! Direct branch merges use normal ref-admission policy, not pull-request approval authority.
use super::{
    access, pull_requests,
    refs::{publication_error, RefError},
    store::{repo_store, BranchMergePublication, CommitInfo, MergeMethod, MergeRequest},
};
use crate::backend::git::error::{ErrorKind, RepoError};
use crate::backend::{
    ci::{CiRuns, RefUpdate},
    cluster::NodeContext,
    db, git,
};

/// Native forms pin both commit identities. REST callers without a preview use the identities
/// resolved by this operation, and publication still compares both mutable refs atomically.
pub struct BranchMerge<'a> {
    pub base: &'a str,
    pub head: &'a str,
    pub message: Option<&'a str>,
    pub expected_base: Option<&'a str>,
    pub expected_head: Option<&'a str>,
}

fn moved() -> RefError {
    RefError::Git(RepoError::new(
        ErrorKind::Conflict,
        "A branch changed since the merge was prepared",
    ))
}

/// None means that the base already contains the head; no object or ref is created in that case.
pub async fn merge_branch(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor: impl Into<db::credentials::Actor>,
    repo_id: i64,
    owner: &str,
    input: BranchMerge<'_>,
) -> Result<Option<CommitInfo>, RefError> {
    let authority = actor.into();
    let actor = authority.user_id;
    let (repository, user) = access::check_write(pool, repo_id, actor)
        .await
        .map_err(RefError::Access)?;
    let base_ref = git::branch_ref(input.base).ok_or(RefError::Invalid("Invalid base branch"))?;
    if input.head.is_empty() || input.head.len() > 4096 {
        return Err(RefError::Invalid("A branch name or commit SHA is required"));
    }
    if input.message.is_some_and(|message| {
        message.is_empty()
            || message.len() > codeyard_support::git_limits::MAX_MERGE_MESSAGE_BYTES
            || message.contains('\0')
    }) {
        return Err(RefError::Invalid(
            "Merge message must be nonempty, at most 8192 bytes and without NUL",
        ));
    }
    let store = repo_store(&repository, cluster);
    let refs = store.refs_from_primary().await.map_err(RefError::Git)?;
    let base = refs.get(&base_ref).ok_or_else(|| {
        RefError::Git(RepoError::new(ErrorKind::NotFound, "Base branch not found"))
    })?;
    let head_branch = git::branch_ref(input.head).filter(|reference| refs.contains_key(reference));
    let head = match head_branch.as_deref() {
        Some(reference) => refs.get(reference).cloned().ok_or(RefError::Unavailable)?,
        None if (4..=40).contains(&input.head.len())
            && input.head.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            store
                .commit_from_primary(input.head)
                .await
                .map_err(RefError::Git)?
                .sha
        }
        None => {
            return Err(RefError::Git(RepoError::new(
                ErrorKind::NotFound,
                "Head branch or commit not found",
            )))
        }
    };
    if input.expected_base.is_some_and(|expected| expected != base)
        || input.expected_head.is_some_and(|expected| expected != head)
    {
        return Err(moved());
    }
    if store
        .is_ancestor(&head, base)
        .await
        .map_err(RefError::Git)?
    {
        return Ok(None);
    }
    let message = input
        .message
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Merge branch '{}' into {}", input.head, input.base));
    let request = MergeRequest {
        head_branch: input.head.into(),
        base_branch: input.base.into(),
        head: head.clone(),
        base: base.clone(),
        message,
        squash_body: None,
        name: user.username.clone(),
        email: user
            .email
            .clone()
            .unwrap_or_else(|| format!("{}@users.noreply", user.username)),
        method: MergeMethod::Merge,
        author_name: None,
        author_email: None,
    };
    let result = store.prepare_merge(&request).await.map_err(|error| {
        if error.kind == ErrorKind::Invalid && !error.is_response_too_large() {
            RefError::Git(RepoError::new(
                ErrorKind::Conflict,
                "Branches cannot be merged",
            ))
        } else {
            RefError::Git(error)
        }
    })?;
    let commit = store
        .commit_from_primary(&result)
        .await
        .map_err(RefError::Git)?;
    // Publication and its follow-ups run in an owned task, so a cancelled request cannot move
    // the ref and then skip the pull-request refresh and push workflows; a retry would find the
    // branches already merged and do nothing.
    let (pool, cluster, ci) = (pool.clone(), cluster.cloned(), ci.clone());
    let (owner, base_name) = (owner.to_owned(), input.base.to_owned());
    let publication = BranchMergePublication {
        base_branch: base_name.clone(),
        base: base.clone(),
        head_branch,
        head,
        result: result.clone(),
        authority,
    };
    let base = base.clone();
    tokio::spawn(async move {
        // A failure after the ref moved still refreshes pull requests and queues workflows,
        // since a retry finds the branches already merged.
        let followup_failed = match repo_store(&repository, cluster.as_ref())
            .publish_branch_merge_as(&pool, repo_id, actor, &publication)
            .await
            .map_err(publication_error)
        {
            Ok(_) => false,
            Err(RefError::AppliedFollowupFailed) => true,
            Err(error) => return Err(error),
        };
        let refreshed = pull_requests::refresh_open_pull_requests(
            &pool,
            &repository,
            Some(&[base_name.as_str()]),
            &ci,
            actor,
            db::models::AccessLevel::Owner,
        )
        .await
        .is_ok();
        ci.queue_push_runs(
            &repository,
            &owner,
            actor,
            RefUpdate {
                reference: base_ref,
                old: Some(base),
                new: result,
                changed_paths: None,
            },
        )
        .await;
        if !refreshed || followup_failed {
            return Err(RefError::AppliedFollowupFailed);
        }
        Ok(Some(commit))
    })
    .await
    .map_err(|_| RefError::Unavailable)?
}
