//! Shared lockfile parsing and verified security-update proposals.
pub mod lockfiles;
pub mod npm;
pub mod registry;
pub(crate) mod updates;
pub mod worker;
use crate::backend::{
    cluster::NodeContext,
    db,
    repositories::{access, store::repo_store},
};
use codeyard_git::error::{ErrorKind, RepoError};
use db::repositories::dependencies::{self as records, Feature};
#[derive(Debug)]
pub enum DependencyError {
    Access(access::AccessError),
    Store(RepoError),
}
#[derive(Clone, Copy, serde::Serialize)]
pub struct Status {
    pub enabled: bool,
    pub paused: bool,
}
async fn delegated_admin(
    pool: &codeyard_db_core::DbPool,
    repo: i64,
    actor: Option<i64>,
) -> Result<bool, DependencyError> {
    let Some(actor) = actor else {
        return Ok(false);
    };
    match access::check_admin(pool, repo, actor).await {
        Ok(_) => Ok(true),
        Err(access::AccessError::Unavailable) => {
            Err(DependencyError::Access(access::AccessError::Unavailable))
        }
        Err(_) => Ok(false),
    }
}
pub async fn status(
    pool: &codeyard_db_core::DbPool,
    actor: i64,
    repo: i64,
    feature: Feature,
) -> Result<Status, DependencyError> {
    let repository = access::check_admin(pool, repo, actor)
        .await
        .map_err(DependencyError::Access)?;
    let current = records::state(pool, repo)
        .await
        .map_err(|e| DependencyError::Store(store_error(e)))?;
    let Some(current) = current else {
        return Ok(Status {
            enabled: false,
            paused: false,
        });
    };
    let enabled = match feature {
        Feature::Alerts => current.alerts_enabled,
        Feature::Updates => current.updates_enabled,
    };
    if !enabled {
        return Ok(Status {
            enabled: false,
            paused: false,
        });
    }
    let mut paused = repository.is_archived
        || !current.same_owner(&repository)
        || (repository.is_private && !current.private_consent)
        || current.last_status == "unsupported";
    if let Some(settings) = db::code_security::repository_settings(pool, repo)
        .await
        .map_err(|error| DependencyError::Store(store_error(error)))?
    {
        let settings: serde_json::Value = serde_json::from_str(&settings.security_and_analysis)
            .map_err(|error| DependencyError::Store(store_error(error.into())))?;
        paused |= settings["dependabot_alerts"]["status"] == "disabled"
            || (matches!(feature, Feature::Updates)
                && settings["dependabot_security_updates"]["status"] == "disabled");
    }
    if !paused {
        paused = !delegated_admin(pool, repo, current.alerts_actor_id).await?;
    }
    if !paused && matches!(feature, Feature::Updates) {
        paused = !delegated_admin(pool, repo, current.updates_actor_id).await?;
    }
    Ok(Status { enabled, paused })
}
#[expect(
    clippy::too_many_arguments,
    reason = "Explicit actor and resources keep operations HTTP-free."
)]
pub async fn set_enabled(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    actor: i64,
    credential: db::users::accounts::OwnedAccountCredential,
    repo: i64,
    feature: Feature,
    enabled: bool,
    private_consent: bool,
) -> Result<(), DependencyError> {
    let repository = access::check_admin(pool, repo, actor)
        .await
        .map_err(DependencyError::Access)?;
    if repository.is_archived {
        return Err(DependencyError::Store(RepoError::new(
            ErrorKind::Conflict,
            "Archived repositories cannot change dependency security",
        )));
    }
    repo_store(&repository, cluster)
        .configure_dependency_security_as(
            pool,
            repo,
            actor,
            credential,
            feature,
            enabled,
            private_consent,
        )
        .await
        .map_err(DependencyError::Store)
}

pub(crate) async fn observe_refs(
    pool: &codeyard_db_core::DbPool,
    repo: &codeyard_db_core::models::Repository,
    moves: &[crate::backend::webhooks::ref_changes::RefMove],
) {
    let default = format!("refs/heads/{}", repo.default_branch);
    if !moves.iter().any(|(reference, _, _)| reference == &default) {
        return;
    }
    if !matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            records::enqueue(pool, repo.id)
        )
        .await,
        Ok(Ok(()))
    ) {
        tracing::warn!(
            repo_id = repo.id,
            "Dependency scan scheduling delayed; periodic scans remain enabled"
        );
    }
}
pub(crate) fn check_publication(
    actor: i64,
    repo: &codeyard_db_core::models::Repository,
    prepared: &super::store::PreparedFile,
    publication: &records::Publication,
    refs: &std::collections::HashMap<String, String>,
) -> Result<(), RepoError> {
    let update = &publication.update;
    update.validate().map_err(store_error)?;
    let valid = publication.job.repo_id == repo.id
        && publication.job.updates_actor_id == Some(actor)
        && update.requested_by == actor
        && update.pull_request_id.is_none()
        && update.branch != repo.default_branch
        && prepared.branch == update.branch
        && prepared.reference == format!("refs/heads/{}", update.branch)
        && prepared.expected.as_deref() == Some(update.base_sha.as_str())
        && prepared.commit.sha == update.commit_sha
        && prepared.commit.parents.as_slice() == std::slice::from_ref(&update.base_sha)
        && refs.get(&format!("refs/heads/{}", repo.default_branch)) == Some(&update.base_sha)
        && refs.get(&prepared.reference) == Some(&update.base_sha);
    if !valid {
        return Err(RepoError::new(
            ErrorKind::Conflict,
            "Dependency update source or branch changed; rescan before publishing",
        ));
    }
    Ok(())
}
pub(crate) fn store_error(error: anyhow::Error) -> codeyard_git::error::RepoError {
    use crate::backend::db::repositories::dependencies::Refusal;
    use codeyard_git::error::{ErrorKind, RepoError};
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return RepoError::write_denied();
    }
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => RepoError::write_denied(),
        Some(Refusal::Missing) => RepoError::new(ErrorKind::NotFound, "Dependency alert not found"),
        Some(Refusal::Stale) => RepoError::new(
            ErrorKind::Conflict,
            "Dependency scan or repository changed; retry",
        ),
        Some(error) => RepoError::new(ErrorKind::Invalid, error.to_string()),
        None => RepoError::new(
            ErrorKind::Unavailable,
            "Dependency security storage unavailable",
        ),
    }
}
