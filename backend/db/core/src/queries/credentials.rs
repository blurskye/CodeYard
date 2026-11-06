use crate::queries::users::accounts::AccountCredential;
use crate::queries::users::accounts::OwnedAccountCredential;
/// Bare IDs are trusted in-process calls. HTTP adapters must retain their original credential.
/// The credential's own `Debug` redacts the session and token secret.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Actor {
    pub user_id: i64,
    pub credential: Option<OwnedAccountCredential>,
}

impl From<i64> for Actor {
    fn from(user_id: i64) -> Self {
        Self {
            user_id,
            credential: None,
        }
    }
}

impl From<(i64, String)> for Actor {
    fn from((user_id, session): (i64, String)) -> Self {
        Self {
            user_id,
            credential: Some(OwnedAccountCredential::Session(session)),
        }
    }
}

impl From<(i64, &AccountCredential<'_>)> for Actor {
    fn from((user_id, credential): (i64, &AccountCredential<'_>)) -> Self {
        let credential = OwnedAccountCredential::from(credential);
        Self {
            user_id,
            credential: Some(credential),
        }
    }
}

impl From<(i64, &OwnedAccountCredential)> for Actor {
    fn from((user_id, credential): (i64, &OwnedAccountCredential)) -> Self {
        Self {
            user_id,
            credential: Some(credential.clone()),
        }
    }
}

impl From<(i64, OwnedAccountCredential)> for Actor {
    fn from((user_id, credential): (i64, OwnedAccountCredential)) -> Self {
        Self {
            user_id,
            credential: Some(credential),
        }
    }
}

impl Actor {
    pub fn token_id(&self) -> Option<i64> {
        match &self.credential {
            Some(
                OwnedAccountCredential::Token(id, _) | OwnedAccountCredential::ForeignToken(id, _),
            ) => Some(*id),
            _ => None,
        }
    }
    pub fn credential(&self) -> Option<AccountCredential<'_>> {
        self.credential
            .as_ref()
            .map(OwnedAccountCredential::as_credential)
    }
}

pub(crate) fn app_permission(
    permissions: &str,
    repositories: &str,
    repo: i64,
    permission: &str,
    write: bool,
) -> anyhow::Result<bool> {
    let selected: Vec<i64> = serde_json::from_str(repositories)?;
    let permissions: serde_json::Value = serde_json::from_str(permissions)?;
    Ok((selected.is_empty() || selected.contains(&repo))
        && codeyard_model::github_apps::allows(&permissions, permission, write))
}

pub(crate) fn oauth_organization_path(permission: &str) -> String {
    let resource = match permission {
        "organization_administration" => "code-security",
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
        "pull_requests" => "pulls",
        "vulnerability_alerts" => "dependabot/alerts",
        "security_events" => "code-scanning/alerts",
        "environments" => "environments/production",
        other => other,
    };
    format!("/repos/owner/repo/{resource}")
}
