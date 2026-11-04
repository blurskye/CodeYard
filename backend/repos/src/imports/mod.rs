use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    pub async fn user(
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


pub mod archive;
pub mod forge;
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
    let url = codeyard_support::outbound::validate_url(text, allow_private())
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

