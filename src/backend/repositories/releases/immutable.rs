//! Publication-time immutable releases share the primary Git lock with every tag writer.
use super::{
    integrity::{Attestation, Statement},
    ReleaseChanges, ReleaseError, ReleaseFields,
};
use crate::backend::api::http::config::link_origin;
use crate::backend::cluster::NodeContext;
use crate::backend::db;
use crate::backend::db::repositories::immutable_releases as records;
use crate::backend::db::repositories::immutable_releases::Refusal;
use crate::backend::repositories::access;
use crate::backend::repositories::git_sessions::validation::RefUpdate;
use crate::backend::repositories::git_sessions::validation::ZERO_SHA;
use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::ErrorKind;
use crate::backend::repositories::store::RepoError;
use anyhow::{ensure, Result};
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use serde::{Deserialize, Serialize};

pub const TAG_PROTECTED: &str = "This tag name is protected by an immutable release";
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Mutation {
    Create {
        tag: String,
        fields: ReleaseFields,
        /// The commit a draft is saved for when its tag does not exist yet.
        #[serde(default)]
        draft_target: Option<String>,
    },
    Update {
        id: i64,
        changes: ReleaseChanges,
    },
}
impl Mutation {
    pub(crate) fn validate(&self) -> Result<(), RepoError> {
        let result = match self {
            Self::Create { tag, fields, .. } => {
                super::validate_tag(tag).and_then(|()| super::validate_fields(fields))
            }
            Self::Update { changes, .. } => super::validate_fields(&ReleaseFields {
                name: changes.name.clone().flatten(),
                body: changes.body.clone().flatten(),
                ..Default::default()
            }),
        };
        result.map_err(|error| match error {
            ReleaseError::Invalid(message) => RepoError::new(ErrorKind::Invalid, message),
            _ => RepoError::new(ErrorKind::Unavailable, "Release validation unavailable"),
        })
    }
}
pub async fn enabled(
    pool: &codeyard_db_core::DbPool,
    viewer: Option<&User>,
    repository: &Repository,
) -> Result<bool> {
    let actor = viewer.ok_or(Refusal::Denied)?;
    let actor = db::users::accounts::get_user_by_id(pool, actor.id)
        .await?
        .ok_or(Refusal::Denied)?;
    let repository = db::repositories::records::get_repository_by_id(pool, repository.id)
        .await?
        .ok_or(Refusal::Denied)?;
    ensure!(
        access::can_admin(pool, &actor, &repository).await?,
        Refusal::Denied
    );
    records::enabled(pool, repository.id).await
}
pub async fn set_enabled(
    pool: &codeyard_db_core::DbPool,
    actor: impl Into<db::credentials::Actor>,
    repo: i64,
    enabled: bool,
) -> Result<()> {
    records::set_enabled(pool, actor, repo, enabled).await
}

/// Also used with a proposed namespace before renaming/transferring/copy publication.
pub(crate) async fn check_tag_updates(
    pool: &codeyard_db_core::DbPool,
    repo: &Repository,
    owner: &str,
    name: &str,
    updates: &[RefUpdate],
) -> Result<(), RepoError> {
    let tags: Vec<String> = updates
        .iter()
        .filter_map(|update| {
            update
                .reference
                .strip_prefix("refs/tags/")
                .map(str::to_owned)
        })
        .collect();
    let reservations = records::reservations(pool, repo.id, owner, name, &tags)
        .await
        .map_err(|_| RepoError::new(ErrorKind::Unavailable, "Immutable tag policy unavailable"))?;
    for reserved in reservations {
        for update in updates.iter().filter(|update| {
            update.reference.strip_prefix("refs/tags/") == Some(reserved.tag_name.as_str())
        }) {
            let allowed = if reserved.repo_id == repo.id && reserved.release_id.is_some() {
                update.new == reserved.tag_oid
            } else {
                update.new == ZERO_SHA
            };
            if !allowed {
                return Err(RepoError::new(ErrorKind::Conflict, TAG_PROTECTED));
            }
        }
    }
    Ok(())
}
pub(crate) async fn check_current_tag_updates(
    pool: &codeyard_db_core::DbPool,
    repo: &Repository,
    updates: &[RefUpdate],
) -> Result<(), RepoError> {
    if !updates
        .iter()
        .any(|update| update.reference.starts_with("refs/tags/"))
    {
        return Ok(());
    }
    let owner = db::repositories::records::repository_owner_names(pool, &[repo.id])
        .await
        .map_err(|_| RepoError::new(ErrorKind::Unavailable, "Repository owner unavailable"))?
        .remove(&repo.id)
        .ok_or_else(|| RepoError::new(ErrorKind::Unavailable, "Repository owner unavailable"))?;
    check_tag_updates(pool, repo, &owner, &repo.name, updates).await
}
/// Namespace changes preserve existing tags; only reservations belonging to another
/// repository can conflict. Retired tags of this same repository are not recreated.
pub(crate) async fn check_namespace(
    pool: &codeyard_db_core::DbPool,
    repo: &Repository,
    owner: &str,
    name: &str,
    references: &std::collections::HashMap<String, String>,
) -> Result<(), RepoError> {
    let tags: Vec<String> = references
        .keys()
        .filter_map(|reference| reference.strip_prefix("refs/tags/").map(str::to_owned))
        .collect();
    let reserved = records::reservations(pool, repo.id, owner, name, &tags)
        .await
        .map_err(|_| {
            RepoError::new(
                ErrorKind::Unavailable,
                "Immutable namespace policy unavailable",
            )
        })?;
    if reserved
        .iter()
        .any(|reservation| reservation.repo_id != repo.id)
    {
        return Err(RepoError::new(
            ErrorKind::Conflict,
            "The destination namespace has reserved immutable tag names",
        ));
    }
    Ok(())
}

static LOCAL_IDENTITIES: std::sync::LazyLock<
    std::sync::RwLock<std::collections::HashMap<std::path::PathBuf, std::path::PathBuf>>,
> = std::sync::LazyLock::new(Default::default);

/// The application, not a repository row or remote request, selects the backed-up key directory.
pub(crate) fn register_local_identity(state_dir: &std::path::Path, repos: &std::path::Path) {
    let root = std::path::absolute(repos).unwrap_or_else(|_| repos.to_owned());
    LOCAL_IDENTITIES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(root, state_dir.to_owned());
}

pub(crate) fn configure_local_identity(
    state_dir: &std::path::Path,
    repos: &std::path::Path,
) -> anyhow::Result<()> {
    crate::backend::cluster::identity::release_key(state_dir, repos)?;
    register_local_identity(state_dir, repos);
    Ok(())
}

async fn local_key(path: &std::path::Path) -> anyhow::Result<crate::backend::cluster::NodeKey> {
    let path = std::path::absolute(path)?;
    let (root, state) = LOCAL_IDENTITIES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())
        .map(|(root, state)| (root.clone(), state.clone()))
        .ok_or_else(|| anyhow::anyhow!("Release identity not configured"))?;
    tokio::task::spawn_blocking(move || {
        crate::backend::cluster::identity::release_key(&state, &root)
    })
    .await?
}

pub(crate) async fn verify_local(
    attestation: &Attestation,
    path: &std::path::Path,
) -> Result<records::Publication, RepoError> {
    let key = local_key(path)
        .await
        .map_err(|_| RepoError::new(ErrorKind::Unavailable, "Release identity unavailable"))?;
    verify_with_key(attestation, &key)
}

fn verify_with_key(
    attestation: &Attestation,
    key: &crate::backend::cluster::NodeKey,
) -> Result<records::Publication, RepoError> {
    records::Publication::verify(attestation, &key.public_hex()).map_err(|_| {
        RepoError::new(
            ErrorKind::Invalid,
            "Invalid or untrusted release attestation",
        )
    })
}

pub(crate) fn namespace_url(owner: &str, name: &str) -> String {
    format!(
        "{}/{}/{}",
        link_origin(),
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(name)
    )
}

pub(crate) async fn candidate(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    repository: &Repository,
    tag: &str,
    release: Option<i64>,
    publishing: bool,
) -> Result<Option<Attestation>, ReleaseError> {
    if !publishing
        || !records::enabled(pool, repository.id)
            .await
            .map_err(super::unavailable)?
    {
        return Ok(None);
    }
    let store = repo_store(repository, cluster);
    let reference =
        crate::backend::git::tag_ref(tag).ok_or(ReleaseError::Invalid("Invalid release tag"))?;
    let tag_oid = store
        .refs_from_primary()
        .await
        .map_err(ReleaseError::Git)?
        .remove(&reference)
        .ok_or(ReleaseError::Invalid(
            "Create the release tag before publishing",
        ))?;
    let commit = store
        .commit_from_primary(&tag_oid)
        .await
        .map_err(ReleaseError::Git)?;
    let assets = match release {
        Some(id) => records::assets(pool, id)
            .await
            .map_err(super::unavailable)?,
        None => vec![],
    };
    let owner = db::repositories::records::repository_owner_names(pool, &[repository.id])
        .await
        .map_err(super::unavailable)?
        .remove(&repository.id)
        .ok_or(ReleaseError::Unavailable)?;
    let url = namespace_url(&owner, &repository.name);
    let statement = Statement::new(repository.id, &url, tag, &tag_oid, &commit.sha, assets)
        .map_err(super::unavailable)?;
    let key = match cluster {
        Some(context) => context.key.clone(),
        None => local_key(std::path::Path::new(&repository.disk_path))
            .await
            .map_err(super::unavailable)?,
    };
    Attestation::sign(&statement, &key)
        .map(Some)
        .map_err(super::unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_publication_requires_the_persisted_forge_identity() {
        let root = tempfile::tempdir().unwrap();
        let repos = root.path().join("repos");
        let state = root.path().join("state");
        let legacy = crate::backend::cluster::load_or_create(&repos).unwrap();
        let key = crate::backend::cluster::identity::release_key(&state, &repos).unwrap();
        assert_eq!(legacy.public_hex(), key.public_hex());
        assert!(state.join("release.key").is_file());
        let statement = Statement::new(
            1,
            "https://forge.example/alice/repo",
            "v1",
            &"a".repeat(40),
            &"b".repeat(40),
            vec![],
        )
        .unwrap();
        let valid = Attestation::sign(&statement, &key).unwrap();
        assert!(verify_with_key(&valid, &key).is_ok());
        let forged =
            Attestation::sign(&statement, &crate::backend::cluster::NodeKey::generate()).unwrap();
        let error = verify_with_key(&forged, &key)
            .err()
            .expect("unknown signing identity must fail");
        assert_eq!(error.kind, ErrorKind::Invalid);
    }
}
