//! GitHub App public-key credentials and explicit repository permission bounds.
use crate::backend::db::{self, github_apps::App, DbPool};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use rsa::{
    pkcs1v15::{Signature, VerifyingKey},
    pkcs8::DecodePublicKey,
    signature::Verifier,
    RsaPublicKey,
};
use serde_json::{Map, Value};
use sha2::Sha256;

/// The stored form of an App name: trimmed, not empty and at most 100 bytes.
pub fn valid_name(name: &str) -> Option<&str> {
    let name = name.trim();
    (!name.is_empty() && name.len() <= 100).then_some(name)
}

pub fn valid_slug(slug: &str) -> bool {
    (1..=80).contains(&slug.len())
        && slug.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && format!("{slug}[bot]") != db::ci_tokens::BOT
}

/// Bound expensive key generation, including concurrent replays of a conversion code.
pub async fn generate_key() -> anyhow::Result<(String, String)> {
    static SLOTS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(4)));
    let permit = SLOTS.clone().try_acquire_owned()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048)?;
        Ok((
            key.to_pkcs8_pem(LineEnding::LF)?.to_string(),
            key.to_public_key().to_public_key_pem(LineEnding::LF)?,
        ))
    })
    .await?
}

pub enum RegistrationKeyError {
    Limited,
    Unavailable,
}

/// Native registration and credential recovery share one account key-generation budget.
pub async fn registration_key(pool: &DbPool, actor: i64) -> Result<(String, String), RegistrationKeyError> {
    super::admission::admit_operation(pool, None, "app-register", &actor.to_string()).await
        .map_err(|error| match error {
            super::admission::AdmissionError::Limited => RegistrationKeyError::Limited,
            super::admission::AdmissionError::Unavailable => RegistrationKeyError::Unavailable,
        })?;
    generate_key().await.map_err(|_| RegistrationKeyError::Unavailable)
}

#[derive(Clone)]
pub struct Manifest {
    pub name: String,
    pub slug: String,
    pub description: String,
    pub homepage: String,
    pub permissions: Value,
    pub events: String,
    pub is_public: bool,
    pub redirect: Option<String>,
    pub callbacks: Vec<String>,
    pub hook_url: Option<String>,
    pub hook_active: bool,
}
pub fn manifest(value: &Value) -> Option<Manifest> {
    let object = value.as_object()?;
    // These optional setup automations have no native UI counterpart yet; do not silently
    // promise a redirect or an OAuth approval step that installation will not perform.
    if object.contains_key("setup_url")
        || ["request_oauth_on_install", "setup_on_update"]
            .iter()
            .any(|key| object.get(*key).is_some_and(|value| value != false))
    {
        return None;
    }
    let name = valid_name(object.get("name")?.as_str()?)?;
    let homepage = object.get("url")?.as_str()?;
    if homepage.len() > 2048
        || !super::oauth_apps::valid_url(homepage)
    {
        return None;
    }
    let description = object
        .get("description")
        .map(Value::as_str)
        .unwrap_or(Some(""))?;
    if description.len() > 1000 {
        return None;
    }
    let permissions = Value::Object(permissions(
        object
            .get("default_permissions")
            .unwrap_or(&serde_json::json!({})),
    )?);
    let chosen = object
        .get("default_events")
        .map(|value| serde_json::from_value::<Vec<String>>(value.clone()))
        .transpose()
        .ok()?
        .unwrap_or_default();
    let events = if chosen.is_empty() {
        String::new()
    } else {
        crate::backend::webhooks::events::subscription(&chosen).ok()?
    };
    // An app cannot subscribe to a repository category it was not granted.
    if chosen
        .iter()
        .any(|event| !event_permission(&permissions, event))
    {
        return None;
    }
    let is_public = object
        .get("public")
        .map(Value::as_bool)
        .unwrap_or(Some(false))?;
    let redirect = url_field(object, "redirect_url")?;
    let mut callbacks = object
        .get("callback_urls")
        .map(|value| serde_json::from_value::<Vec<String>>(value.clone()))
        .transpose()
        .ok()?
        .unwrap_or_default();
    if callbacks.len() > 10
        || callbacks
            .iter()
            .any(|url| url.len() > 2048 || !super::oauth_apps::valid_url(url))
    {
        return None;
    }
    if callbacks.is_empty() {
        callbacks.push(homepage.to_owned());
    }
    let hook = match object.get("hook_attributes") {
        Some(value) => Some(value.as_object()?),
        None => None,
    };
    let hook_url = match hook {
        Some(hook) => url_field(hook, "url")?,
        None => None,
    };
    let hook_active = hook
        .and_then(|hook| hook.get("active"))
        .map(Value::as_bool)
        .unwrap_or(Some(true))?;
    let slug = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let mut slug = slug.trim_matches('-').chars().take(70).collect::<String>();
    if !slug.as_bytes().first().is_some_and(u8::is_ascii_lowercase) {
        slug = format!("app-{slug}");
    }
    if !valid_slug(&slug) {
        return None;
    }
    Some(Manifest {
        name: name.into(),
        slug,
        description: description.into(),
        homepage: homepage.into(),
        permissions,
        events,
        is_public,
        redirect,
        callbacks,
        hook_url,
        hook_active,
    })
}
fn url_field(object: &Map<String, Value>, key: &str) -> Option<Option<String>> {
    match object.get(key) {
        None => Some(None),
        Some(Value::String(url)) if url.len() <= 2048 && super::oauth_apps::valid_url(url) => {
            Some(Some(url.clone()))
        }
        _ => None,
    }
}

pub fn event_permission(permissions: &Value, event: &str) -> bool {
    let key = match event {
        "push" | "create" | "delete" | "release" | "commit_comment" => "contents",
        "issues" | "issue_comment" => "issues",
        "pull_request" | "pull_request_review" | "pull_request_review_comment" => "pull_requests",
        "fork" | "star" | "repository" => "metadata",
        "workflow_run" | "workflow_job" => "actions",
        "repository_dispatch" => "contents",
        "check_run" | "check_suite" => "checks",
        "deployment" | "deployment_status" => "deployments",
        "deployment_protection_rule" => return allows(permissions, "deployments", true),
        _ => return false,
    };
    allows(permissions, key, false)
}

pub fn permissions(value: &Value) -> Option<Map<String, Value>> {
    let mut permissions = value.as_object()?.clone();
    if permissions.len() > 20
        || permissions.iter().any(|(key, value)| {
            !matches!(
                key.as_str(),
                "metadata"
                    | "contents"
                    | "issues"
                    | "pull_requests"
                    | "checks"
                    | "statuses"
                    | "actions"
                    | "deployments"
                    | "pages"
                    | "administration"
                    | "workflows"
            ) || !matches!(value.as_str(), Some("read" | "write"))
                || (key == "metadata" && value != "read")
                || (key == "workflows" && value != "write")
        })
    {
        return None;
    }
    permissions.insert("metadata".into(), Value::String("read".into()));
    Some(permissions)
}
pub fn subset(requested: &Value, granted: &Value) -> Option<Value> {
    let requested = permissions(requested)?;
    let granted = granted.as_object()?;
    requested
        .iter()
        .all(|(key, value)| {
            granted
                .get(key)
                .is_some_and(|grant| grant == "write" || grant == value)
        })
        .then_some(Value::Object(requested))
}
pub fn allows(permissions: &Value, key: &str, write: bool) -> bool {
    permissions
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|level| level == "write" || (!write && level == "read"))
}
pub fn write(permissions: &Value) -> bool {
    permissions
        .as_object()
        .is_some_and(|map| map.values().any(|value| value == "write"))
}

/// Only RS256, with a maximum ten-minute lifetime. Private signing keys never persist.
pub async fn authenticate(pool: &DbPool, value: &str) -> anyhow::Result<Option<App>> {
    let invalid = || anyhow::anyhow!("invalid app JWT");
    let mut pieces = value.split('.');
    let (Some(header), Some(claims), Some(signature), None) =
        (pieces.next(), pieces.next(), pieces.next(), pieces.next())
    else {
        return Ok(None);
    };
    let checked = || -> anyhow::Result<(Value, Value, Signature)> {
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header)?)?;
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims)?)?;
        let signature = Signature::try_from(URL_SAFE_NO_PAD.decode(signature)?.as_slice())?;
        if header["alg"] != "RS256" || header.get("crit").is_some() {
            return Err(invalid());
        }
        Ok((header, claims, signature))
    };
    let Ok((_, claims, signature)) = checked() else {
        return Ok(None);
    };
    let now = Utc::now().timestamp();
    let (Some(iat), Some(exp)) = (claims["iat"].as_i64(), claims["exp"].as_i64()) else {
        return Ok(None);
    };
    if iat > now + 60
        || iat < now - 660
        || exp <= now
        || exp > now + 600
        || exp <= iat
        || exp - iat > 660
        || claims
            .get("nbf")
            .is_some_and(|nbf| nbf.as_i64().is_none_or(|nbf| nbf > now + 60))
    {
        return Ok(None);
    }
    let issuer = match &claims["iss"] {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => return Ok(None),
    };
    let Some(app) = db::github_apps::app_issuer(pool, &issuer).await? else {
        return Ok(None);
    };
    // Slugs are discovery identifiers, not JWT issuers.
    if issuer != app.client_id && issuer != app.id.to_string() {
        return Ok(None);
    }
    let key = RsaPublicKey::from_public_key_pem(&app.public_key)?;
    if VerifyingKey::<Sha256>::new(key)
        .verify(
            format!(
                "{header}.{claims}",
                claims = value.split('.').nth(1).unwrap()
            )
            .as_bytes(),
            &signature,
        )
        .is_err()
    {
        return Ok(None);
    }
    Ok(Some(app))
}

/// Effective bounds shared by REST, Git and receive-pack revalidation: an App's grant or a
/// fine-grained personal token's.
#[derive(Clone)]
pub struct Authority {
    /// An App's granted permissions, or a fine-grained token's repository permissions.
    pub permissions: Value,
    pub source: Source,
}
#[derive(Clone)]
pub enum Source {
    App {
        app_id: i64,
        installation: Option<i64>,
        repositories: Vec<i64>,
    },
    Personal(super::fine_grained::Grant),
}
impl Authority {
    pub fn installation(&self) -> Option<i64> {
        match &self.source {
            Source::App { installation, .. } => *installation,
            Source::Personal(_) => None,
        }
    }
    pub fn personal(&self) -> Option<&super::fine_grained::Grant> {
        match &self.source {
            Source::Personal(grant) => Some(grant),
            Source::App { .. } => None,
        }
    }
    /// The permissions this credential holds on `repo`, or `None` when it cannot reach it.
    /// The user's own rights still apply on top.
    pub async fn repository(
        &self,
        pool: &DbPool,
        user: i64,
        repo: i64,
    ) -> anyhow::Result<Option<Value>> {
        let (app_id, installation, repositories) = match &self.source {
            Source::Personal(grant) => return grant.repository(pool, repo).await,
            Source::App {
                app_id,
                installation,
                repositories,
            } => (*app_id, *installation, repositories),
        };
        if !repositories.is_empty() && !repositories.contains(&repo) {
            return Ok(None);
        }
        // An installation token names its installation; a user token reaches any installation
        // of its app that the user reaches.
        let actor = installation.is_none().then_some(user);
        let covering =
            db::github_apps::repository_installation(pool, app_id, repo, true, actor).await?;
        Ok(covering
            .is_some_and(|found| installation.is_none_or(|id| id == found.id))
            .then(|| self.permissions.clone()))
    }
    /// Whether this credential reaches `repo` with permission `key`.
    pub async fn allows(
        &self,
        pool: &DbPool,
        user: i64,
        repo: i64,
        key: &str,
        write: bool,
    ) -> anyhow::Result<bool> {
        Ok(self
            .repository(pool, user, repo)
            .await?
            .is_some_and(|permissions| allows(&permissions, key, write)))
    }
}
pub async fn authority(pool: &DbPool, id: i64) -> anyhow::Result<Option<Authority>> {
    if let Some(token) = db::github_apps::token(pool, id).await? {
        let Some(installation) = db::github_apps::installation(pool, token.installation_id).await?
        else {
            return Ok(None);
        };
        return Ok(Some(Authority {
            permissions: serde_json::from_str(&token.permissions)?,
            source: Source::App {
                app_id: installation.app_id,
                installation: Some(installation.id),
                repositories: serde_json::from_str(&token.repositories)?,
            },
        }));
    }
    if let Some(token) = db::oauth_apps::token(pool, id).await? {
        if let Some(app) = db::github_apps::app(pool, &token.client_id).await? {
            return Ok(Some(Authority {
                permissions: serde_json::from_str(&app.permissions)?,
                source: Source::App {
                    app_id: app.id,
                    installation: None,
                    repositories: vec![],
                },
            }));
        }
    }
    if let Some(grant) = super::fine_grained::grant(pool, id).await? {
        return Ok(Some(Authority {
            permissions: Value::Object(grant.group(super::fine_grained::Group::Repository)),
            source: Source::Personal(grant),
        }));
    }
    Ok(None)
}

pub fn repository_permission(path: &str, write: bool) -> Option<&'static str> {
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    if !matches!(parts.as_slice(), ["repos", _, _, ..]) {
        return None;
    }
    match parts.get(3).copied() {
        None => Some(if write { "administration" } else { "metadata" }),
        Some("statuses") => Some("statuses"),
        // GitHub files these read-only summaries under repository metadata.
        Some("languages" | "contributors" | "stats") if !write => Some("metadata"),
        // Only structural subresources determine authority, never an owner/repo/ref name.
        Some("commits") => match parts.get(5).copied() {
            Some("status" | "statuses") => Some("statuses"),
            Some("check-runs" | "check-suites") => Some("checks"),
            Some("pulls") => Some("pull_requests"),
            _ => Some("contents"),
        },
        Some("branches") if parts.get(5) == Some(&"protection") => Some("administration"),
        Some("tags") if parts.get(4) == Some(&"protection") => Some("administration"),
        Some(
            "contents" | "git" | "compare" | "releases" | "tags" | "branches" | "zipball"
            | "tarball" | "readme",
        ) => Some("contents"),
        Some("issues" | "labels" | "milestones" | "assignees") => Some("issues"),
        // These publishers need credential/tree/head-repository fencing, not just an actor ID.
        Some("pulls")
            if write
                && matches!(
                    parts.get(5).copied(),
                    Some("merge" | "merge-async" | "update-branch")
                ) =>
        {
            None
        }
        Some("pulls") => Some("pull_requests"),
        // Environment secrets and variables have their own unsupported permissions.
        Some("environments") if matches!(parts.get(5).copied(), Some("secrets" | "variables")) => {
            None
        }
        Some("environments") => Some(if write {
            "administration"
        } else {
            "deployments"
        }),
        // Reviewing a run's pending deployments approves a deployment, not an Actions change.
        Some("actions")
            if parts.get(6) == Some(&"deployment_protection_rule")
                || (write && parts.get(6) == Some(&"pending_deployments")) =>
        {
            Some("deployments")
        }
        Some("actions") => match parts.get(4).copied() {
            Some("permissions") => Some("administration"),
            Some("cache") if matches!(parts.get(5).copied(), Some("storage-limit" | "retention-limit")) => Some("administration"),
            Some("runs" | "workflows" | "jobs" | "artifacts" | "caches" | "cache") => {
                Some("actions")
            }
            // Secrets, variables, runners and OIDC have separate unsupported permissions.
            _ => None,
        },
        Some("check-runs" | "check-suites") => Some("checks"),
        Some("deployments") => Some("deployments"),
        Some("pages") => Some("pages"),
        Some("collaborators") if !write => Some("metadata"),
        Some("collaborators" | "teams" | "invitations") => Some("administration"),
        Some("comments" | "codeowners" | "license" | "dispatches") => Some("contents"),
        Some("traffic") if !write => Some("administration"),
        // Repository hooks need repository_hooks, not administration.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn github_apps_permission_bounds_are_monotonic() {
        assert!(permissions(&json!({"organization_administration":"write"})).is_none());
        assert!(permissions(&json!({"metadata":"write"})).is_none());
        assert!(permissions(&json!({"workflows":"read"})).is_none());
        let granted = json!({"metadata":"read","contents":"read","issues":"write"});
        assert!(subset(&json!({"contents":"write"}), &granted).is_none());
        assert!(subset(&json!({"issues":"read"}), &granted).is_some());
        assert!(!event_permission(&json!({"metadata":"read"}), "issues"));
        assert!(event_permission(&json!({"issues":"read"}), "issues"));
    }
    #[test]
    fn github_apps_route_permissions_do_not_infer_authority_from_resource_names() {
        for path in [
            "/repos/status/proj/commits/main",
            "/repos/alice/statuses/commits/main",
            "/repos/alice/proj/commits/status",
        ] {
            assert_eq!(repository_permission(path, false), Some("contents"));
        }
        assert_eq!(
            repository_permission("/repos/a/b/commits/main/status", false),
            Some("statuses")
        );
        assert_eq!(
            repository_permission("/repos/a/b/commits/main/check-runs", false),
            Some("checks")
        );
        assert_eq!(
            repository_permission("/repos/a/b/branches/main/protection", true),
            Some("administration")
        );
        assert_eq!(
            repository_permission("/repos/a/b/actions/runs", false),
            Some("actions")
        );
        assert_eq!(
            repository_permission("/repos/a/b/traffic/clones", false),
            Some("administration")
        );
        let pending = "/repos/a/b/actions/runs/1/pending_deployments";
        assert_eq!(repository_permission(pending, false), Some("actions"));
        assert_eq!(repository_permission(pending, true), Some("deployments"));
        for path in [
            "/repos/a/b/actions/secrets",
            "/repos/a/b/actions/variables",
            "/repos/a/b/hooks",
            "/repos/a/b/pulls/1/merge",
            "/repos/a/b/pulls/1/update-branch",
        ] {
            assert_eq!(repository_permission(path, true), None);
        }
    }
    #[test]
    fn github_apps_manifests_reject_unimplemented_authority_and_setup_steps() {
        assert!(manifest(
            &json!({"name":"Bot","url":"https://example.test","default_events":["issues"]})
        )
        .is_none());
        assert!(manifest(&json!({"name":"Bot","url":"https://example.test","setup_url":"https://example.test/setup"})).is_none());
        let parsed = manifest(&json!({"name":"Bot","url":"https://example.test"})).unwrap();
        assert!(!parsed.is_public);
        assert_eq!(parsed.permissions, json!({"metadata":"read"}));
    }
}
