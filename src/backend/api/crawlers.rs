//! robots.txt and ai.txt follow the site's AI crawler setting (`node_configuration.ai_crawlers`).
//! Block is the default. Both files then disallow the AI crawlers; allow lets every agent in.
use crate::backend::{app::AppState, db};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::sync::Arc;

/// A missing setting means block: the default is to block AI crawlers.
pub const ALLOW: &str = "allow";
pub const BLOCK: &str = "block";

const ALLOW_ALL: &str = "User-agent: *\nDisallow:\n";

/// Known AI training crawlers. Other agents keep the allow-all group below.
const BLOCKED_ROBOTS_TXT: &str = "\
User-agent: GPTBot
User-agent: ChatGPT-User
User-agent: CCBot
User-agent: ClaudeBot
User-agent: anthropic-ai
User-agent: Google-Extended
User-agent: PerplexityBot
User-agent: Bytespider
User-agent: Applebot-Extended
User-agent: cohere-ai
User-agent: Meta-ExternalAgent
User-agent: Amazonbot
User-agent: Diffbot
User-agent: ImagesiftBot
User-agent: omgili
User-agent: FacebookBot
Disallow: /

User-agent: *
Disallow:
";

const BLOCKED_AI_TXT: &str = include_str!("../../../ai.txt");

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/robots.txt", get(robots_txt))
        .route("/ai.txt", get(ai_txt))
}

/// The stored value for a submitted setting, or None when it is neither `allow` nor `block`.
pub(crate) fn setting_value(raw: &str) -> Option<&'static str> {
    match raw {
        ALLOW => Some(ALLOW),
        BLOCK => Some(BLOCK),
        _ => None,
    }
}

/// Whether AI training crawlers are blocked. No setting row means block; an unreadable
/// setting is an error, not allow.
pub(crate) async fn ai_crawlers_blocked(pool: &db::DbPool) -> anyhow::Result<bool> {
    Ok(db::node_config::get_node_configuration(pool)
        .await?
        .is_none_or(|config| config.ai_crawlers != ALLOW))
}

async fn robots_txt(State(state): State<Arc<AppState>>) -> Response {
    match ai_crawlers_blocked(&state.pool).await {
        Ok(true) => plain_text(BLOCKED_ROBOTS_TXT),
        Ok(false) => plain_text(ALLOW_ALL),
        Err(_) => unavailable(),
    }
}

async fn ai_txt(State(state): State<Arc<AppState>>) -> Response {
    match ai_crawlers_blocked(&state.pool).await {
        Ok(true) => plain_text(BLOCKED_AI_TXT),
        Ok(false) => plain_text(ALLOW_ALL),
        Err(_) => unavailable(),
    }
}

fn plain_text(body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

fn unavailable() -> Response {
    tracing::warn!("Crawler policy unavailable");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Crawler policy is temporarily unavailable.",
    )
        .into_response()
}
