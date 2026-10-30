//! Gists reuse repository placement, Git objects and transport. SQL admission precedes the ref
//! publication; durable pending writes reconcile lost responses without claiming cross-store rollback.
use crate::backend::{
    app::AppState,
    db::{
        self,
        gists::{Gist, GistRefusal},
        models::Repository,
        repositories::creation::{RepositoryCreationOwner, RepositoryTarget},
    },
    repositories::{
        lifecycle,
        store::{
            repo_store, GistCredential, GistEdit, GistFileChange, GistSnapshot, RepoError,
            MAX_GIST_FILES,
        },
    },
};
use anyhow::{ensure, Result};
use std::{collections::BTreeMap, future::Future, sync::Arc};

static GIST_WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
pub struct LoadedGist {
    pub gist: Gist,
    pub repository: Repository,
    pub snapshot: GistSnapshot,
}
pub struct CreateGist {
    pub description: String,
    pub is_public: bool,
    pub files: BTreeMap<String, String>,
}
#[derive(Debug, thiserror::Error)]
#[error("The save outcome for gist {id} could not be confirmed; reload it before retrying")]
pub struct GistSaved {
    pub id: String,
}
#[derive(Debug, thiserror::Error)]
#[error("The gist was deleted but its storage could not be removed")]
pub struct GistDeleted;

pub fn text_preview(bytes: &[u8], limit: usize) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut length = limit.min(text.len());
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    Some(&text[..length])
}

pub async fn load(state: &AppState, id: &str, revision: Option<&str>) -> Result<LoadedGist> {
    let gist = db::gists::get(&state.pool, id)
        .await?
        .ok_or(GistRefusal::Missing)?;
    let repository = db::repositories::records::get_repository_by_id(&state.pool, gist.repo_id)
        .await?
        .filter(|repo| repo.is_gist)
        .ok_or(GistRefusal::Missing)?;
    let snapshot = repo_store(&repository, state.cluster.as_ref())
        .gist_snapshot(&state.pool, repository.id, revision)
        .await?;
    let gist = db::gists::get(&state.pool, id)
        .await?
        .filter(|gist| gist.ready)
        .ok_or(GistRefusal::Missing)?;
    Ok(LoadedGist {
        gist,
        repository,
        snapshot,
    })
}
/// Recover accepted initial publications even if a disconnected creator never received the ID.
/// Bound each owner-list request; storage failures remain failures, not an empty list.
pub async fn recover_owned(state: &AppState, owner: i64) -> Result<()> {
    for repo in db::gists::orphaned_for_owner(&state.pool, owner).await? {
        discard(state, repo, true).await?;
    }
    for id in db::gists::unready_for_owner(&state.pool, owner).await? {
        if let Err(error) = load(state, &id, None).await {
            let Some(gist) = db::gists::get(&state.pool, &id).await? else {
                continue;
            };
            if !gist.ready
                && db::gists::pending(&state.pool, gist.repo_id)
                    .await?
                    .is_none()
            {
                discard(state, gist.repo_id, false).await?;
            } else {
                tracing::warn!(gist_id = %id, %error, "Pending gist recovery deferred");
            }
        }
    }
    Ok(())
}
async fn confirm_saved(state: &AppState, id: String) -> Result<LoadedGist> {
    load(state, &id, None)
        .await
        .map_err(|_| GistSaved { id }.into())
}

async fn complete<T: Send + 'static>(
    state: &AppState,
    work: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    let permit = GIST_WORK
        .try_acquire()
        .map_err(|_| RepoError::sessions_busy())?;
    let (send, receive) = tokio::sync::oneshot::channel();
    ensure!(
        state.git_tasks.spawn(async move {
            let _permit = permit;
            let _ = send.send(work.await);
        }),
        RepoError::sessions_busy()
    );
    receive
        .await
        .map_err(|_| RepoError::authority_unavailable())?
}
async fn discard(state: &AppState, repo: i64, orphan_only: bool) -> Result<bool> {
    if let Some(repository) = db::gists::discard(&state.pool, repo, orphan_only).await? {
        lifecycle::remove_deleted_repository(state.cluster.as_ref(), &repository).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}
pub async fn create(
    state: Arc<AppState>,
    actor: i64,
    credential: GistCredential,
    input: CreateGist,
) -> Result<LoadedGist> {
    ensure!(
        !input.files.is_empty() && input.files.len() <= MAX_GIST_FILES,
        GistRefusal::Invalid
    );
    let edit = GistEdit {
        description: Some(input.description.clone()),
        files: input
            .files
            .into_iter()
            .map(|(name, content)| {
                (
                    name,
                    Some(GistFileChange {
                        filename: None,
                        content: Some(content),
                    }),
                )
            })
            .collect(),
        expected: None,
    };
    edit.validate()?;
    let work = state.clone();
    complete(&state, async move {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("gist-{id}");
        let repo = lifecycle::provision_gist(
            (&work.pool, work.cluster.as_ref()),
            &work.repos_path,
            RepositoryCreationOwner {
                actor_id: actor,
                target: RepositoryTarget::Personal(actor),
            },
            &name,
            None,
        )
        .await?;
        if let Err(error) = db::gists::reserve(
            &work.pool,
            actor,
            &credential.borrowed(),
            &db::gists::NewGist {
                repo_id: repo.id,
                id: &id,
                description: &input.description,
                is_public: input.is_public,
            },
        )
        .await
        {
            discard(&work, repo.id, false).await?;
            return Err(error);
        }
        let store = repo_store(&repo, work.cluster.as_ref());
        if let Err(error) = store
            .gist_write(&work.pool, repo.id, actor, credential, edit)
            .await
        {
            if let Ok(loaded) = load(&work, &id, None).await {
                return Ok(loaded);
            }
            if !discard(&work, repo.id, false).await? {
                return Err(GistSaved { id }.into());
            }
            return Err(error.into());
        }
        confirm_saved(&work, id).await
    })
    .await
}
pub async fn update(
    state: Arc<AppState>,
    actor: i64,
    credential: GistCredential,
    id: String,
    input: GistEdit,
) -> Result<LoadedGist> {
    input.validate()?;
    let work = state.clone();
    complete(&state, async move {
        let loaded = load(&work, &id, None).await?;
        ensure!(loaded.gist.owner_id == actor, GistRefusal::Denied);
        if let Err(error) = repo_store(&loaded.repository, work.cluster.as_ref())
            .gist_write(&work.pool, loaded.repository.id, actor, credential, input)
            .await
        {
            if error.is_applied_followup_failed() {
                return Err(GistSaved { id }.into());
            }
            return Err(error.into());
        }
        confirm_saved(&work, id).await
    })
    .await
}
pub async fn fork(
    state: Arc<AppState>,
    actor: i64,
    credential: GistCredential,
    id: String,
) -> Result<LoadedGist> {
    let work = state.clone();
    complete(&state, async move {
        recover_owned(&work, actor).await?;
        let source = load(&work, &id, None).await?;
        ensure!(source.gist.owner_id != actor, GistRefusal::Invalid);
        let id = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("gist-{id}");
        let repo = lifecycle::provision_gist(
            (&work.pool, work.cluster.as_ref()),
            &work.repos_path,
            RepositoryCreationOwner {
                actor_id: actor,
                target: RepositoryTarget::Personal(actor),
            },
            &name,
            Some(&source.repository),
        )
        .await?;
        if let Err(error) = db::gists::reserve(
            &work.pool,
            actor,
            &credential.borrowed(),
            &db::gists::NewGist {
                repo_id: repo.id,
                id: &id,
                description: &source.gist.description,
                is_public: source.gist.is_public,
            },
        )
        .await
        {
            discard(&work, repo.id, false).await?;
            return Err(error);
        }
        confirm_saved(&work, id).await
    })
    .await
}
pub async fn delete(
    state: Arc<AppState>,
    actor: i64,
    credential: GistCredential,
    id: String,
) -> Result<()> {
    let work = state.clone();
    complete(&state, async move {
        let gist = db::gists::get(&work.pool, &id)
            .await?
            .ok_or(GistRefusal::Missing)?;
        let repository =
            db::gists::delete(&work.pool, actor, &credential.borrowed(), gist.repo_id).await?;
        lifecycle::remove_deleted_repository(work.cluster.as_ref(), &repository)
            .await.map_err(|_| GistDeleted)?;
        Ok(())
    })
    .await
}
