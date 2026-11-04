//! OAuth clients authorize through native forms; only hashes of codes, secrets and tokens persist.

use codeyard_db_core::DbPool;
use codeyard_db_identity::oauth_apps::Application;
use codeyard_model::identity::CLI_CLIENT;
use codeyard_support::token_hash::token_hash;
use subtle::ConstantTimeEq;

const CLI_SECRET: &str = "34ddeff2b558a23d38fba8a6de74f086ede1cc0b";

pub fn user_code() -> String {
    let value = codeyard_support::crypto::random_hex();
    format!("{}-{}", &value[..4], &value[4..8]).to_uppercase()
}

pub fn normalize_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

pub async fn client(pool: &DbPool, id: &str) -> anyhow::Result<Option<Application>> {
    if id == CLI_CLIENT {
        return Ok(Some(Application {
            id: 0,
            owner_id: 0,
            client_id: id.to_owned(),
            secret_hash: token_hash(CLI_SECRET),
            name: "GitHub CLI".into(),
            homepage: "https://cli.github.com".into(),
            redirect_uri: "http://localhost/".into(),
            created_at: chrono::Utc::now(),
        }));
    }
    if !codeyard_db_identity::github_apps::client_active(pool, id).await? {
        return Ok(None);
    }
    codeyard_db_identity::oauth_apps::application(pool, id).await
}

pub fn valid_secret(app: &Application, secret: &str) -> bool {
    bool::from(
        app.secret_hash
            .as_bytes()
            .ct_eq(token_hash(secret).as_bytes()),
    )
}

pub fn redirect(app: &Application, supplied: Option<&str>) -> Option<String> {
    let uri = supplied.unwrap_or(&app.redirect_uri);
    let url = url::Url::parse(uri).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return None;
    }
    if app.client_id == CLI_CLIENT {
        return (url.scheme() == "http"
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
            && matches!(url.path(), "/" | "/callback")
            && url.query().is_none())
        .then(|| uri.to_owned());
    }
    (uri == app.redirect_uri).then(|| uri.to_owned())
}

pub fn valid_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

/// Unsupported permissions are rejected, never silently granted broader account access.
pub fn scopes(value: &str) -> Option<String> {
    if value.len() > 1024 {
        return None;
    }
    // The CLI may request refreshable credentials, but also accepts a non-expiring token.
    // Do not advertise offline access when no refresh token was issued.
    let mut scopes: Vec<_> = value
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|s| !s.is_empty() && *s != "offline_access")
        .collect();
    if scopes.iter().any(|s| {
        !matches!(
            *s,
            "repo"
                | "read:org"
                | "write:org"
                | "security_events"
                | "gist"
                | "workflow"
                | "delete_repo"
                | "read:packages"
                | "read:public_key"
                | "write:public_key"
                | "admin:public_key"
                | "read:ssh_signing_key"
                | "write:ssh_signing_key"
                | "admin:ssh_signing_key"
                | "read:gpg_key"
                | "write:gpg_key"
                | "admin:gpg_key"
        )
    }) {
        return None;
    }
    scopes.sort_unstable();
    scopes.dedup();
    Some(scopes.join(","))
}

/// OAuth credentials retain anonymous-equivalent public reads; private reads and writes need scopes.
/// `bounds` is the token's `github_apps::authority`, which the caller loads once and reuses for
/// its repository check.
pub async fn allows_git(
    pool: &DbPool,
    token_id: i64,
    bounds: Option<&crate::github_apps::Authority>,
    gist: bool,
    public_read: bool,
) -> anyhow::Result<bool> {
    if let Some(bounds) = bounds {
        // A fine-grained token's per-repository check follows, including public reads.
        if bounds.personal().is_some() {
            return Ok(true);
        }
        return Ok(
            !gist && codeyard_model::github_apps::allows(&bounds.permissions, "contents", false)
        );
    }
    let scope = if gist { "gist" } else { "repo" };
    Ok(codeyard_db_identity::oauth_apps::token(pool, token_id)
        .await?
        .is_none_or(|token| public_read || has_scope(&token.scopes, scope)))
}

/// Whether a token may change workflow files. Apps and fine-grained tokens use their
/// `workflows` permission and OAuth tokens need the `workflow` scope. A job token never may,
/// as GitHub's GITHUB_TOKEN cannot be given the workflows permission.
pub async fn allows_workflows(pool: &DbPool, token_id: i64) -> anyhow::Result<bool> {
    if codeyard_db_actions::ci_tokens::get(pool, token_id)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    if let Some(grant) = crate::fine_grained::grant(pool, token_id).await? {
        return Ok(grant.active
            && codeyard_model::fine_grained::allows(&grant.permissions, "workflows", true));
    }
    if let Some(bounds) = crate::github_apps::authority(pool, token_id).await? {
        return Ok(codeyard_model::github_apps::allows(
            &bounds.permissions,
            "workflows",
            true,
        ));
    }
    Ok(codeyard_db_identity::oauth_apps::token(pool, token_id)
        .await?
        .is_none_or(|token| has_scope(&token.scopes, "workflow")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeyard_model::oauth_apps::allows;
    use codeyard_model::oauth_apps::write_scope;

    #[test]
    fn code_scanning_and_security_configuration_scopes_are_narrow() {
        assert_eq!(
            scopes("security_events write:org"),
            Some("security_events,write:org".into())
        );
        assert!(write_scope("security_events"));
        assert!(write_scope("write:org"));
        assert!(allows(
            "security_events",
            "/repos/alice/scan/code-scanning/sarifs",
            true
        ));
        assert!(allows(
            "security_events",
            "/orgs/acme/code-scanning/alerts",
            false
        ));
        assert!(!allows(
            "security_events",
            "/repos/alice/scan/code-scanning/default-setup",
            false
        ));
        assert!(!allows(
            "security_events",
            "/repos/alice/scan/issues",
            false
        ));
        assert!(allows(
            "read:org",
            "/orgs/acme/code-security/configurations",
            false
        ));
        assert!(!allows(
            "read:org",
            "/orgs/acme/code-security/configurations",
            true
        ));
        assert!(allows(
            "write:org",
            "/orgs/acme/code-security/configurations",
            true
        ));
        assert!(!allows(
            "write:org",
            "/repos/alice/scan/code-scanning/sarifs",
            true
        ));
    }

    #[test]
    fn deleting_a_key_needs_the_admin_scope() {
        assert!(allows("write:public_key", "/user/keys", true));
        assert!(allows("write:public_key", "/user/keys/7", false));
        assert!(!allows("write:public_key", "/user/keys/7", true));
        assert!(allows("admin:public_key", "/user/keys/7", true));
        assert!(!allows(
            "write:ssh_signing_key",
            "/user/ssh_signing_keys/7",
            true
        ));
        assert!(allows(
            "admin:ssh_signing_key",
            "/user/ssh_signing_keys/7",
            true
        ));
    }

    #[test]
    fn package_read_scope_does_not_grant_repository_or_write_access() {
        assert_eq!(scopes("read:packages").as_deref(), Some("read:packages"));
        assert!(!write_scope("read:packages"));
        assert!(allows("read:packages", "/user/docker/conflicts", false));
        assert!(allows("read:packages", "/user/packages", false));
        assert!(allows(
            "read:packages",
            "/orgs/acme/packages/container/app/versions",
            false
        ));
        assert!(!allows(
            "read:packages",
            "/user/packages/container/app",
            true
        ));
        assert!(!allows("repo", "/users/alice/packages", false));
        assert!(!allows("read:packages", "/repos/alice/project", false));
        assert!(!allows(
            "read:packages",
            "/orgs/acme/docker/conflicts",
            true
        ));
        assert!(allows("repo", "/users/alice/attestations/bulk-list", true));
        assert!(!allows(
            "read:org",
            "/users/alice/attestations/bulk-list",
            true
        ));
    }

    #[test]
    fn cli_refresh_request_may_receive_a_non_expiring_token() {
        assert_eq!(
            scopes("repo read:org gist offline_access"),
            Some("gist,read:org,repo".to_owned())
        );
        assert_eq!(scopes("offline_access"), Some(String::new()));
        assert_eq!(scopes("admin:enterprise"), None);
        // The workflow scope is checked against the published trees, not the route.
        assert!(allows("repo", "/repos/a/b/pulls/1/merge", true));
        assert!(allows("repo", "/repos/a/b/pulls/1/update-branch", true));
        assert!(allows("repo", "/repos/a/b/merges", true));
        assert!(allows("repo", "/repos/a/b/merge-upstream", true));
        assert!(allows("repo,workflow", "/repos/a/b/merge-upstream", true));
        assert!(allows("repo,workflow", "/repos/a/b/pulls/1/merge", true));
        assert!(!allows("read:org", "/orgs/private/repos", false));
        assert!(!allows("read:org", "/users/alice/repos", false));
        assert!(allows("read:org", "/orgs/acme/members", false));
        assert!(!allows("read:org", "/orgs/acme/members", true));
        assert!(allows(
            "repo",
            "/orgs/acme/attestations/sha256:digest",
            false
        ));
        assert!(allows(
            "repo",
            "/orgs/acme/artifacts/metadata/storage-record",
            true
        ));
        assert!(!allows(
            "read:org",
            "/orgs/acme/artifacts/metadata/storage-record",
            true
        ));
    }
}

use codeyard_model::oauth_apps::has_scope;
