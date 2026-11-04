//! Git source imports retain no source credentials and publish only into empty repositories.
pub mod archive;
pub mod forge;
use crate::backend::app::AppState;

use crate::backend::repositories::store::repo_store;
use codeyard_db_repos::source_imports::SourceImport;

use codeyard_db_core::models::Repository;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};

pub const MAX_IMPORT_PACK_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitImport {
    pub generation: String,
    pub credential: ImportCredential,
    pub refs: HashMap<String, String>,
    pub default_branch: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ImportCredential {
    Token(i64),
    Session(String),
}

impl std::fmt::Debug for ImportCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ImportCredential([redacted])")
    }
}

impl ImportCredential {
    pub(crate) async fn user(
        &self,
        pool: &codeyard_db_core::DbPool,
    ) -> anyhow::Result<Option<codeyard_db_core::models::User>> {
        match self {
            Self::Token(id) => {
                codeyard_db_identity::users::tokens::token_user(pool, *id, true).await
            }
            Self::Session(id) => {
                codeyard_db_identity::users::sessions::get_session_user(pool, id).await
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("Invalid source URL")]
    Source,
    #[error("Invalid source credentials")]
    Credentials,
    #[error("Import requires an empty repository")]
    NotEmpty,
    #[error("Repository administration denied")]
    Denied,
    #[error("An import is already running")]
    Running,
    #[error("Import is temporarily unavailable")]
    Unavailable,
}

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

pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    pub fn valid(&self) -> bool {
        [&self.username, &self.password]
            .iter()
            .all(|value| value.len() <= 2048 && !value.contains(['\r', '\n', '\0']))
    }
}

pub fn allow_private() -> bool {
    std::env::var("MIGRATIONS_ALLOW_PRIVATE").is_ok_and(|value| value == "true")
}

pub fn source_url(text: &str) -> anyhow::Result<url::Url> {
    let url = crate::backend::webhooks::outbound::validate_url(text, allow_private())
        .map_err(|_| anyhow::anyhow!("Invalid source URL"))?;
    anyhow::ensure!(
        url.scheme() == "https" || allow_private(),
        "Source URL must use HTTPS"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "Source URL cannot contain a query or fragment"
    );
    Ok(url)
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
                    result = crate::backend::repositories::store::source::fetch(&import.vcs_url, credentials) => result,
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
