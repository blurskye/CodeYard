//! Safe runtime clone endpoints for SSR and JSON. No host paths or credentials.
//! SSH endpoints follow the startup-selected transport; unavailable SSH is hidden.

use axum::{Extension, Json};
use serde::Serialize;
use std::sync::Arc;

/// The HTTP clone endpoint when neither PUBLIC_HTTP_CLONE_BASE nor PUBLIC_URL is set.
pub const DEFAULT_HTTP_CLONE_BASE: &str = "http://localhost:3000";
/// The shipped built-in SSH listener uses port 2222.
pub const DEFAULT_SSH_CLONE_BASE: &str = "ssh://git@localhost:2222";

/// Immutable HTTP settings captured when the router is constructed.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub public: PublicConfig,
    pub cors_origins: Vec<String>,
}

impl WebConfig {
    pub fn from_env() -> Self {
        Self {
            public: PublicConfig {
                http_clone_base: http_clone_base(),
                ssh_clone_base: ssh_clone_base(),
                ssh_mode: crate::backend::ssh::mode::selected()
                    .map_or("builtin", |s| s.mode.as_str()),
                ssh_setup_required: crate::backend::ssh::mode::selected()
                    .is_some_and(|s| s.setup_required),
            },
            cors_origins: std::env::var("CORS_ORIGINS")
                .unwrap_or_else(|_| "http://localhost:8080".to_string())
                .split(',')
                .map(|origin| origin.trim().to_string())
                .filter(|origin| !origin.is_empty())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicConfig {
    pub http_clone_base: String,
    pub ssh_clone_base: String,
    pub ssh_mode: &'static str,
    pub ssh_setup_required: bool,
}

/// Blank clone settings use the default endpoint.
pub fn value_or(value: Option<String>, default: &str) -> String {
    match value {
        Some(value) if !value.trim().is_empty() => value,
        _ => default.to_string(),
    }
}

pub fn http_clone_base() -> String {
    configured_http_base(
        std::env::var("PUBLIC_HTTP_CLONE_BASE").ok(),
        std::env::var("PUBLIC_URL").ok(),
    )
}

/// `PUBLIC_HTTP_CLONE_BASE`, then `PUBLIC_URL`, then the default. Blank values are unset.
/// The value is returned as configured; the repository page trims a trailing slash.
fn configured_http_base(clone_base: Option<String>, public_url: Option<String>) -> String {
    let public_or_default = value_or(public_url, DEFAULT_HTTP_CLONE_BASE);
    value_or(clone_base, &public_or_default)
}

pub fn ssh_clone_base() -> String {
    configured_ssh_base(
        std::env::var("PUBLIC_SSH_CLONE_BASE").ok(),
        crate::backend::ssh::mode::selected(),
    )
}

fn configured_ssh_base(
    value: Option<String>,
    selected: Option<&crate::backend::ssh::mode::Selection>,
) -> String {
    use crate::backend::ssh::mode::SshMode;
    let Some(selected) = selected else {
        return value_or(value, DEFAULT_SSH_CLONE_BASE);
    };
    if selected.setup_required || selected.mode == SshMode::Disabled {
        return String::new();
    }
    let fallback = match selected.mode {
        SshMode::Openssh => format!("ssh://{}@localhost:22", selected.git_user),
        _ => format!("ssh://git@localhost:{}", selected.builtin_port),
    };
    value_or(value, &fallback)
}

/// `GET /api/config`
pub async fn get_public_config(Extension(config): Extension<Arc<WebConfig>>) -> Json<PublicConfig> {
    Json(config.public.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_env_falls_back_like_the_original_or_operator() {
        assert_eq!(value_or(None, "d"), "d");
        assert_eq!(value_or(Some(String::new()), "d"), "d");
        assert_eq!(value_or(Some("   ".to_string()), "d"), "d");
        assert_eq!(value_or(Some("x".to_string()), "d"), "x");
    }

    #[test]
    fn http_clone_setting_wins_over_public_url() {
        assert_eq!(
            configured_http_base(
                Some("https://clone.example.com".into()),
                Some("https://git.example.com".into()),
            ),
            "https://clone.example.com"
        );
    }

    #[test]
    fn blank_http_clone_setting_falls_back_to_public_url() {
        assert_eq!(
            configured_http_base(Some("  ".into()), Some("https://git.example.com".into())),
            "https://git.example.com"
        );
        assert_eq!(
            configured_http_base(Some(String::new()), Some("https://git.example.com".into())),
            "https://git.example.com"
        );
    }

    #[test]
    fn public_url_with_trailing_slash_is_left_for_callers_to_trim() {
        assert_eq!(
            configured_http_base(None, Some("https://git.example.com/".into())),
            "https://git.example.com/"
        );
    }

    #[test]
    fn http_clone_base_defaults_to_localhost_without_either_setting() {
        assert_eq!(configured_http_base(None, None), DEFAULT_HTTP_CLONE_BASE);
        assert_eq!(
            configured_http_base(Some(String::new()), Some(" ".into())),
            DEFAULT_HTTP_CLONE_BASE
        );
    }

    #[test]
    fn clone_urls_follow_the_selected_transport_without_exposing_unready_ssh() {
        use crate::backend::ssh::mode::{Selection, SshMode};
        let mut selected = Selection {
            mode: SshMode::Openssh,
            setup_required: false,
            git_user: "git".into(),
            socket: "/run/private.sock".into(),
            builtin_port: 2323,
        };
        assert_eq!(
            configured_ssh_base(None, Some(&selected)),
            "ssh://git@localhost:22"
        );
        assert_eq!(
            configured_ssh_base(Some("ssh://git@host:23".into()), Some(&selected)),
            "ssh://git@host:23"
        );
        selected.setup_required = true;
        assert_eq!(
            configured_ssh_base(Some("ssh://git@host".into()), Some(&selected)),
            ""
        );
        selected.setup_required = false;
        selected.mode = SshMode::Disabled;
        assert_eq!(configured_ssh_base(None, Some(&selected)), "");
        selected.mode = SshMode::Builtin;
        assert_eq!(
            configured_ssh_base(None, Some(&selected)),
            "ssh://git@localhost:2323"
        );
    }

    #[test]
    fn defaults_match_the_original_server_load() {
        assert_eq!(DEFAULT_HTTP_CLONE_BASE, "http://localhost:3000");
        assert_eq!(DEFAULT_SSH_CLONE_BASE, "ssh://git@localhost:2222");
    }
}
