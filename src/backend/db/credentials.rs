//! Original credentials retained through database admission. Authority remains feature-owned.

/// Bare IDs are trusted in-process calls. HTTP adapters must retain their original credential.
#[derive(Clone)]
pub struct Actor {
    pub(crate) user_id: i64,
    pub(crate) credential: Option<Credential>,
}
#[derive(Clone)]
pub(crate) enum Credential {
    Session(String),
    Token { id: i64, hash: String },
}
impl From<i64> for Actor {
    fn from(user_id: i64) -> Self { Self { user_id, credential: None } }
}
impl From<(i64, String)> for Actor {
    fn from((user_id, session): (i64, String)) -> Self {
        Self { user_id, credential: Some(Credential::Session(session)) }
    }
}
impl Actor {
    pub fn token(user_id: i64, id: i64, hash: String) -> Self {
        Self { user_id, credential: Some(Credential::Token { id, hash }) }
    }
    pub(crate) fn token_id(&self) -> Option<i64> {
        match &self.credential { Some(Credential::Token { id, .. }) => Some(*id), _ => None }
    }
}

pub(crate) fn app_permission(permissions: &str, repositories: &str, repo: i64, permission: &str, write: bool) -> anyhow::Result<bool> {
    let selected: Vec<i64> = serde_json::from_str(repositories)?;
    let permissions: serde_json::Value = serde_json::from_str(permissions)?;
    Ok((selected.is_empty() || selected.contains(&repo))
        && crate::backend::identity::github_apps::allows(&permissions, permission, write))
}

pub(crate) fn oauth_organization_path(permission: &str) -> String {
    let resource = match permission {
        "members" => "members",
        "organization_hooks" => "hooks",
        "organization_secrets" => "actions/secrets",
        "organization_actions_variables" => "actions/variables",
        "organization_self_hosted_runners" => "actions/runners",
        "organization_user_blocking" => "blocks",
        "organization_custom_properties" => "properties",
        "organization_projects" => "projectsV2",
        _ => "settings",
    };
    format!("/orgs/organization/{resource}")
}

pub(crate) fn oauth_repository_path(permission: &str) -> String {
    let resource = match permission {
        "metadata" | "administration" => "",
        "contents" => "contents/file",
        "actions" => "actions/runs",
        "checks" => "check-runs",
        "vulnerability_alerts" => "dependabot/alerts",
        "security_events" => "code-scanning/alerts",
        "environments" => "environments/production",
        other => other,
    };
    format!("/repos/owner/repo/{resource}")
}
