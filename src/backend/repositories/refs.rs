//! Branch and tag mutations shared by JSON and native forms.
use crate::backend::{
    ci::{CiRuns, RefUpdate},
    cluster::NodeContext,
    db,
    repositories::{
        access,
        protection::{check_branch_write, BranchWrite, History, Refusal, WriteVia, ANY_TIP},
        pull_requests,
        store::{repo_store, TagInfo},
    },
};
use codeyard_git::{
    self as git,
    error::{ErrorKind, RepoError, MERGE_COMMIT_BLOCKED},
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
    pool: &codeyard_db_core::DbPool,
    actor: &db::credentials::Actor,
    repo_id: i64,
) -> Result<
    (
        codeyard_db_core::models::Repository,
        codeyard_db_core::models::User,
    ),
    RefError,
> {
    let (repository, user) = access::check_write(pool, repo_id, actor.user_id)
        .await
        .map_err(RefError::Access)?;
    access::check_publication_actor(pool, actor, &repository)
        .await
        .map_err(|error| publication_error(RepoError::from(error)))?;
    Ok((repository, user))
}

pub(super) fn publication_error(error: RepoError) -> RefError {
    if error.is_applied_followup_failed() {
        RefError::AppliedFollowupFailed
    } else if error.is_protected_branch() {
        RefError::Protected("This branch is protected")
    } else if error.message == super::releases::immutable::TAG_PROTECTED {
        RefError::Protected(super::releases::immutable::TAG_PROTECTED)
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

/// Refuses a branch delete that a protection rule forbids. A delete adds no commits, so the
/// history check is never needed here.
async fn allowed(
    pool: &codeyard_db_core::DbPool,
    repository: &codeyard_db_core::models::Repository,
    actor: &codeyard_db_core::models::User,
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
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor: impl Into<db::credentials::Actor>,
    owner: &str,
    repo_id: i64,
    branch: &str,
    source: &str,
) -> Result<String, RefError> {
    let authority = actor.into();
    let actor = authority.user_id;
    let (repository, _) = writable(pool, &authority, repo_id).await?;
    if branch.is_empty() {
        return Err(RefError::Invalid("Branch name is required"));
    }
    if source.is_empty() {
        return Err(RefError::Invalid("Source SHA is required"));
    }
    if git::branch_ref(branch).is_none() {
        return Err(invalid_ref("branch"));
    }
    // The store checks the protection rules for the new branch under the publication lock,
    // reading the history the branch brings in. That check is the one that can see signatures.
    let sha = repo_store(&repository, cluster)
        .create_branch_as(pool, repository.id, authority, branch, source)
        .await
        .map_err(publication_error)?;
    let refreshed = pull_requests::refresh_open_pull_requests(
        pool,
        &repository,
        Some(&[branch][..]),
        ci,
        actor,
        codeyard_db_core::models::AccessLevel::Owner,
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

#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn update_reference(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor: impl Into<db::credentials::Actor>,
    owner: &str,
    repo_id: i64,
    reference: &str,
    sha: &str,
    force: bool,
) -> Result<String, RefError> {
    let authority = actor.into();
    let actor = authority.user_id;
    let (repository, _) = writable(pool, &authority, repo_id).await?;
    let (old, sha) = repo_store(&repository, cluster)
        .update_reference_as(pool, repo_id, authority, reference, sha, force)
        .await
        .map_err(publication_error)?;
    if old == sha {
        return Ok(sha);
    }
    let refreshed = if let Some(branch) = reference.strip_prefix("refs/heads/") {
        pull_requests::refresh_open_pull_requests(
            pool,
            &repository,
            Some(&[branch][..]),
            ci,
            actor,
            codeyard_db_core::models::AccessLevel::Owner,
        )
        .await
        .is_ok()
    } else {
        true
    };
    ci.queue_push_runs(
        &repository,
        owner,
        actor,
        RefUpdate {
            reference: reference.into(),
            old: Some(old),
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
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    actor: impl Into<db::credentials::Actor>,
    repository: &codeyard_db_core::models::Repository,
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
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor: impl Into<db::credentials::Actor>,
    repo_id: i64,
    branch: &str,
) -> Result<(), RefError> {
    let authority = actor.into();
    let actor = authority.user_id;
    let (repository, user) = writable(pool, &authority, repo_id).await?;
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
        .delete_branch_as(pool, repository.id, authority, branch)
        .await
        .map_err(publication_error)?;
    pull_requests::refresh_open_pull_requests(
        pool,
        &repository,
        Some(&[branch][..]),
        ci,
        actor,
        codeyard_db_core::models::AccessLevel::Owner,
    )
    .await
    .map_err(|_| RefError::AppliedFollowupFailed)?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn create_tag(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    ci: &CiRuns,
    actor: impl Into<db::credentials::Actor>,
    owner: &str,
    repo_id: i64,
    tag: &str,
    source: &str,
    message: Option<&str>,
) -> Result<TagInfo, RefError> {
    let authority = actor.into();
    let actor = authority.user_id;
    let (repository, _) = writable(pool, &authority, repo_id).await?;
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
            authority,
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
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    actor: impl Into<db::credentials::Actor>,
    repo_id: i64,
    tag: &str,
) -> Result<(), RefError> {
    let authority = actor.into();
    let (repository, _) = writable(pool, &authority, repo_id).await?;
    if git::tag_ref(tag).is_none() {
        return Err(invalid_ref("tag"));
    }
    repo_store(&repository, cluster)
        .delete_tag_as(pool, repository.id, authority, tag)
        .await
        .map_err(publication_error)
}
