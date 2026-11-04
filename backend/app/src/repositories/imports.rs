//! Git source imports retain no source credentials and publish only into empty repositories.
use crate::app::AppState;
use crate::repositories::store::repo_store;
use codeyard_db_core::models::Repository;
use codeyard_db_repos::source_imports::SourceImport;
use std::sync::Arc;
use std::time::Duration;

pub async fn start(
    state: Arc<AppState>,
    repo: Repository,
    credential: ImportCredential,
    url: &str,
    credentials: Option<Credentials>,
) -> Result<SourceImport, ImportError> {
    if credentials
        .as_ref()
        .is_some_and(|credentials| !credentials.valid())
    {
        return Err(ImportError::Credentials);
    }
    let user = credential
        .user(&state.pool)
        .await
        .map_err(|_| ImportError::Unavailable)?
        .ok_or(ImportError::Denied)?;
    let url = source_url(url).map_err(|_| ImportError::Source)?;
    if !repo_store(&repo, state.cluster.as_ref())
        .refs_from_primary()
        .await
        .map_err(|_| ImportError::Unavailable)?
        .is_empty()
    {
        return Err(ImportError::NotEmpty);
    }
    let import = codeyard_db_repos::source_imports::start(
        &state.pool,
        user.id,
        repo.id,
        url.as_str(),
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
    .map_err(|error| {
        match error.downcast_ref::<codeyard_db_core::queries::source_imports::ImportRefusal>() {
            Some(codeyard_db_core::queries::source_imports::ImportRefusal::Denied) => {
                ImportError::Denied
            }
            Some(codeyard_db_core::queries::source_imports::ImportRefusal::Running) => {
                ImportError::Running
            }
            None => ImportError::Unavailable,
        }
    })?;
    if !spawn(
        state.clone(),
        repo.clone(),
        import.clone(),
        credential,
        credentials,
    ) {
        codeyard_db_repos::source_imports::finish(&state.pool, repo.id, &import.generation, false)
            .await
            .map_err(|_| ImportError::Unavailable)?;
        return Err(ImportError::Unavailable);
    }
    Ok(import)
}

async fn cancelled(pool: &codeyard_db_core::DbPool, import: &SourceImport) -> anyhow::Result<()> {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if !codeyard_db_repos::source_imports::get(pool, import.repo_id)
            .await?
            .is_some_and(|current| current.generation == import.generation && current.running())
        {
            return Ok(());
        }
    }
}

fn spawn(
    state: Arc<AppState>,
    repo: Repository,
    import: SourceImport,
    credential: ImportCredential,
    credentials: Option<Credentials>,
) -> bool {
    let Ok(permit) = state.repository_downloads.clone().try_acquire_owned() else {
        return false;
    };
    let tasks = state.git_tasks.clone();
    tasks.spawn(async move {
        let _permit = permit;
        let result = async {
            let (refs, default_branch, pack) = tokio::time::timeout(Duration::from_secs(900), async {
                tokio::select! {
                    result = codeyard_repos::source::fetch(&import.vcs_url, credentials) => result,
                    result = cancelled(&state.pool, &import) => {
                        result?;
                        anyhow::bail!("Import cancelled");
                    }
                }
            }).await??;
            let actor = credential
                .user(&state.pool)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Import credential expired"))?;
            repo_store(&repo, state.cluster.as_ref())
                .import_git_as(
                    &state.pool,
                    repo.id,
                    actor.id,
                    GitImport {
                        generation: import.generation.clone(),
                        credential,
                        refs,
                        default_branch,
                    },
                    pack,
                )
                .await?;
            anyhow::Ok(())
        }
        .await;
        let success = result.is_ok();
        // Only identifiers are logged: upstream errors may contain credentials or response text.
        if !success {
            tracing::warn!(repo_id = repo.id, "source import failed");
        }
        if let Err(error) =
            codeyard_db_repos::source_imports::finish(&state.pool, repo.id, &import.generation, success).await
        {
            tracing::error!(repo_id = repo.id, error = %error, "record source import result");
        }
    })
}

use codeyard_repos::imports::{Credentials, GitImport, ImportCredential, ImportError, source_url};
