//! OAuth clients authorize through native forms; only hashes of codes, secrets and tokens persist.
use crate::backend::{
    db::{self, oauth_apps::Application, DbPool},
    identity::tokens::token_hash,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const CLI_CLIENT: &str = "178c6fc778ccc68e1d6a";
const CLI_SECRET: &str = "34ddeff2b558a23d38fba8a6de74f086ede1cc0b";

pub fn random() -> String {
    let mut bytes = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn user_code() -> String {
    let value = random();
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
    if !db::github_apps::client_active(pool, id).await? {
        return Ok(None);
    }
    db::oauth_apps::application(pool, id).await
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
        )
    }) {
        return None;
    }
    scopes.sort_unstable();
    scopes.dedup();
    Some(scopes.join(","))
}

pub fn has_scope(scopes: &str, scope: &str) -> bool {
    scopes.split(',').any(|granted| granted == scope
        || (scope == "read:org" && granted == "write:org")
        || (matches!(scope, "read:user" | "user:email" | "user:follow") && granted == "user"))
}

pub fn write_scope(scopes: &str) -> bool {
    has_scope(scopes, "repo")
        || has_scope(scopes, "gist")
        || has_scope(scopes, "delete_repo")
        || has_scope(scopes, "write:org")
        || has_scope(scopes, "security_events")
        || has_scope(scopes, "write:public_key")
        || has_scope(scopes, "admin:public_key")
        || has_scope(scopes, "write:ssh_signing_key")
        || has_scope(scopes, "admin:ssh_signing_key")
}

/// GitHub's key scopes: an admin scope includes write, and write includes read.
fn key_scope(scopes: &str, kind: &str, write: bool) -> bool {
    has_scope(scopes, &format!("admin:{kind}"))
        || has_scope(scopes, &format!("write:{kind}"))
        || (!write && has_scope(scopes, &format!("read:{kind}")))
}

pub fn verify_challenge(challenge: &str, verifier: &str) -> bool {
    if challenge.is_empty() {
        return true;
    }
    if !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
    {
        return false;
    }
    let actual = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    bool::from(actual.as_bytes().ct_eq(challenge.as_bytes()))
}

/// OAuth permissions complement, rather than replace, the account's live access rules.
pub fn allows(scopes: &str, path: &str, write: bool) -> bool {
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        [""] | ["user"] => !write,
        ["repos", _, _, "code-scanning", "default-setup", ..] => has_scope(scopes, "repo"),
        ["repos", _, _, "code-scanning", ..] | ["orgs", _, "code-scanning", ..] => {
            has_scope(scopes, "repo") || has_scope(scopes, "security_events")
        }
        ["orgs", _, "code-security", ..] => {
            has_scope(scopes, "write:org") || (!write && has_scope(scopes, "read:org"))
        }
        ["user", "docker", "conflicts"] | ["users" | "orgs", _, "docker", "conflicts"] => {
            !write && has_scope(scopes, "read:packages")
        }
        ["user", "repos" | "starred" | "subscriptions", ..]
        | ["repositories", ..]
        | ["repos", ..]
        | ["orgs", _, "repos"]
        | ["orgs", _, "attestations" | "artifacts", ..]
        | ["users", _, "attestations", ..]
        | ["notifications", ..] => has_scope(scopes, "repo"),
        ["user", "orgs"] | ["orgs", _] | ["orgs", _, "public_members", ..] => {
            !write && (has_scope(scopes, "read:org") || has_scope(scopes, "write:org"))
        }
        ["orgs", _, "members" | "memberships" | "teams", ..]
        | ["user", "memberships", "orgs", ..] => {
            has_scope(scopes, "write:org") || (!write && has_scope(scopes, "read:org"))
        }
        ["gists", ..] => has_scope(scopes, "gist"),
        ["users", _, "repos" | "starred" | "subscriptions", ..] | ["search", ..] => {
            !write && has_scope(scopes, "repo")
        }
        ["users", _]
        | ["users", _, "followers" | "following" | "keys" | "gpg_keys" | "ssh_signing_keys"]
        | ["rate_limit"]
        | ["meta"]
        | ["versions" | "zen" | "octocat" | "emojis" | "licenses" | "codes_of_conduct"]
        | ["emojis" | "licenses" | "codes_of_conduct", _]
        | ["gitignore", "templates", ..] => !write,
        // The only write to one key is its deletion, which GitHub keeps for the admin scope.
        ["user", "keys", _] if write => has_scope(scopes, "admin:public_key"),
        ["user", "ssh_signing_keys", _] if write => has_scope(scopes, "admin:ssh_signing_key"),
        ["user", "keys"] | ["user", "keys", _] => key_scope(scopes, "public_key", write),
        ["user", "ssh_signing_keys"] | ["user", "ssh_signing_keys", _] => {
            key_scope(scopes, "ssh_signing_key", write)
        }
        _ => false,
    }
}

/// Git access to a gist needs the `gist` scope; to any other repository, `repo`.
pub async fn allows_git(pool: &DbPool, token_id: i64, gist: bool) -> anyhow::Result<bool> {
    if let Some(bounds) = super::github_apps::authority(pool, token_id).await? {
        // A fine-grained token's per-repository check follows, including public reads.
        if bounds.personal().is_some() {
            return Ok(true);
        }
        return Ok(super::github_apps::allows(
            &bounds.permissions,
            "contents",
            false,
        ));
    }
    let scope = if gist { "gist" } else { "repo" };
    Ok(db::oauth_apps::token(pool, token_id)
        .await?
        .is_none_or(|token| has_scope(&token.scopes, scope)))
}

/// Whether a token may change workflow files. Apps and fine-grained tokens use their
/// `workflows` permission and OAuth tokens need the `workflow` scope. A job token never may,
/// as GitHub's GITHUB_TOKEN cannot be given the workflows permission.
pub async fn allows_workflows(pool: &DbPool, token_id: i64) -> anyhow::Result<bool> {
    if db::ci_tokens::get(pool, token_id).await?.is_some() {
        return Ok(false);
    }
    if let Some(bounds) = super::github_apps::authority(pool, token_id).await? {
        return Ok(super::github_apps::allows(
            &bounds.permissions,
            "workflows",
            true,
        ));
    }
    Ok(db::oauth_apps::token(pool, token_id)
        .await?
        .is_none_or(|token| has_scope(&token.scopes, "workflow")))
}

#[cfg(test)]
mod tests {
    use super::*;

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
