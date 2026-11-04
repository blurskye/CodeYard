//! Credentials shared by today's act worker and future runners.

use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use codeyard_db_core::DbPool;
pub use codeyard_support::token_hash::token_hash;
use rand::RngCore;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RuntimeCredentials {
    pub token: String,
    pub results_url: String,
}

pub fn secret() -> String {
    let mut bytes = [0; codeyard_support::tokens::TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn oidc_url() -> String {
    format!(
        "{}/api/ci/oidc/token?",
        codeyard_model::http_config::link_origin().trim_end_matches('/')
    )
}

pub fn results_url(settings: &crate::backend::db::actions::ActionsSettings) -> String {
    settings
        .results_url
        .clone()
        .unwrap_or_else(codeyard_model::http_config::link_origin)
        .trim_end_matches('/')
        .to_owned()
}

pub async fn issue(
    pool: &DbPool,
    run: &codeyard_db_core::models::CiRun,
    settings: &crate::backend::db::actions::ActionsSettings,
) -> Result<RuntimeCredentials> {
    let backend = run.run_id.to_string();
    // The artifact toolkit decodes this claim locally. Authentication is the hash of the
    // entire random bearer credential, not trust in these unsigned claims.
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
        "scp": format!("Actions.Results:{backend}:{backend}"),
        "jti": secret()
    }))?);
    let token = format!("{header}.{payload}.{}", secret());
    crate::backend::db::action_objects::issue_token(
        pool,
        run.id,
        run.worker.as_deref().unwrap_or(""),
        &token_hash(&token),
        &backend,
    )
    .await?;
    Ok(RuntimeCredentials {
        token,
        results_url: results_url(settings),
    })
}
