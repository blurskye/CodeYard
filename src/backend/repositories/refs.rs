//! Branch and tag mutations shared by JSON and native forms.
use crate::backend::{
    ci::{CiRuns, RefUpdate},
    cluster::NodeContext,
    db, git,
    repositories::{
        access,
        protection::{
            check_branch_write, BranchWrite, History, Refusal, WriteVia, ANY_TIP,
            MERGE_COMMIT_BLOCKED,
        },
        pull_requests,
        store::{repo_store, ErrorKind, RepoError, TagInfo},
    },
};

#[derive(Debug)]
pub enum RefError {
    Access(access::AccessError),
    Invalid(&'static str),
    /// A rule of the branch refuses the change; the text says which.
    Protected(&'static str),
    Unavailable,
    Git(RepoError),
    AppliedFollowupFailed,
}

async fn writable(
    pool: &db::DbPool,
    actor: i64,
    repo_id: i64,
) -> Result<(db::models::Repository, db::models::User), RefError> {
    access::check_write(pool, repo_id, actor)
        .await
        .map_err(RefError::Access)
}

fn publication_error(error: RepoError) -> RefError {
    if error.is_applied_followup_failed() {
        RefError::AppliedFollowupFailed
    } else if error.is_protected_branch() {
        RefError::Protected("This branch is protected")
    } else if error.is_merge_commit_blocked() {
        RefError::Protected(MERGE_COMMIT_BLOCKED)
    } else if error.is_write_denied() {
        RefError::Access(access::AccessError::WriteDenied)
    } else {
        RefError::Git(error)
    }
}

fn invalid_ref(kind: &str) -> RefError {
    RefError::Git(RepoError::new(
        ErrorKind::Invalid,
        format!("Invalid {kind} name"),
    ))
}

/// Refuses a branch create or delete that a protection rule forbids. The branch API adds
/// no commits of its own; the store reads the history of a created branch when it publishes it.
async fn allowed(
    pool: &db::DbPool,
    repository: &db::models::Repository,
    actor: &db::models::User,
    write: BranchWrite<'_>,
) -> Result<(), RefError> {
    match check_branch_write(pool, repository, actor, &write, &History::SingleCommit).await {
        Ok(_) => Ok(()),
        Err(Refusal::Blocked(reason)) => Err(RefError::Protected(reason)),
        Err(Refusal::Unavailable) => Err(RefError::Unavailable),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn create_branch(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor: i64,
    owner: &str,
    repo_id: i64,
    branch: &str,
    source: &str,
) -> Result<String, RefError> {
    let (repository, user) = writable(pool, actor, repo_id).await?;
    if branch.is_empty() {
        return Err(RefError::Invalid("Branch name is required"));
    }
    if source.is_empty() {
        return Err(RefError::Invalid("Source SHA is required"));
    }
    if git::branch_ref(branch).is_none() {
        return Err(invalid_ref("branch"));
    }
    allowed(
        pool,
        &repository,
        &user,
        BranchWrite {
            branch,
            old: None,
            new: Some(source),
            via: WriteVia::Direct,
        },
    )
    .await?;
    let sha = repo_store(&repository, cluster)
        .create_branch_as(pool, repository.id, actor, branch, source)
        .await
        .map_err(publication_error)?;
    let refreshed = pull_requests::refresh_open_pull_requests(
        pool,
        &repository,
        Some(&[branch][..]),
        ci,
        actor,
    )
    .await
    .is_ok();
    ci.queue_push_runs(
        &repository,
        owner,
        actor,
        RefUpdate {
            reference: format!("refs/heads/{branch}"),
            old: None,
            new: sha.clone(),
            changed_paths: None,
        },
    )
    .await;
    if !refreshed {
        return Err(RefError::AppliedFollowupFailed);
    }
    Ok(sha)
}

/// Makes an existing branch the repository's default. The caller checks that the actor administers
/// the repository. Clones made afterwards get this branch.
pub async fn set_default_branch(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    actor: i64,
    repository: &db::models::Repository,
    branch: &str,
) -> Result<(), RefError> {
    if git::branch_ref(branch).is_none() {
        return Err(invalid_ref("branch"));
    }
    repo_store(repository, cluster)
        .set_default_branch_as(pool, repository.id, actor, branch)
        .await
        .map_err(publication_error)
}

pub async fn delete_branch(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor: i64,
    repo_id: i64,
    branch: &str,
) -> Result<(), RefError> {
    let (repository, user) = writable(pool, actor, repo_id).await?;
    if git::branch_ref(branch).is_none() {
        return Err(invalid_ref("branch"));
    }
    // The old tip is read when the branch is deleted; the rules only ask that there is one.
    allowed(
        pool,
        &repository,
        &user,
        BranchWrite {
            branch,
            old: Some(ANY_TIP),
            new: None,
            via: WriteVia::Direct,
        },
    )
    .await?;
    // Keep the stored default protected too; the store checks the advertised default
    // without listing all refs, including repositories with an unborn HEAD.
    if branch == repository.default_branch {
        return Err(RefError::Invalid("Cannot delete the default branch"));
    }
    repo_store(&repository, cluster)
        .delete_branch_as(pool, repository.id, actor, branch)
        .await
        .map_err(publication_error)?;
    pull_requests::refresh_open_pull_requests(pool, &repository, Some(&[branch][..]), ci, actor)
        .await
        .map_err(|_| RefError::AppliedFollowupFailed)?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn create_tag(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor: i64,
    owner: &str,
    repo_id: i64,
    tag: &str,
    source: &str,
    message: Option<&str>,
) -> Result<TagInfo, RefError> {
    let (repository, _) = writable(pool, actor, repo_id).await?;
    if tag.is_empty() {
        return Err(RefError::Invalid("Tag name is required"));
    }
    if source.is_empty() {
        return Err(RefError::Invalid("Source SHA is required"));
    }
    if git::tag_ref(tag).is_none() {
        return Err(invalid_ref("tag"));
    }
    let result = repo_store(&repository, cluster)
        .create_tag_as(
            pool,
            repository.id,
            actor,
            tag,
            source,
            message,
            "Codeyard",
            "noreply@codeyard.local",
        )
        .await
        .map_err(publication_error)?;
    ci.queue_push_runs(
        &repository,
        owner,
        actor,
        RefUpdate {
            reference: format!("refs/tags/{tag}"),
            old: None,
            new: result.sha.clone(),
            changed_paths: None,
        },
    )
    .await;
    Ok(result)
}

pub async fn delete_tag(
    pool: &db::DbPool,
    cluster: Option<&NodeContext>,
    actor: i64,
    repo_id: i64,
    tag: &str,
) -> Result<(), RefError> {
    let (repository, _) = writable(pool, actor, repo_id).await?;
    if git::tag_ref(tag).is_none() {
        return Err(invalid_ref("tag"));
    }
    repo_store(&repository, cluster)
        .delete_tag_as(pool, repository.id, actor, tag)
        .await
        .map_err(publication_error)
}
