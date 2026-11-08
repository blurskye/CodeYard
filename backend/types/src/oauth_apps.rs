use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
/// Organization OIDC names share the existing bounded OAuth provider namespace.
pub fn organization_from_oidc_provider(provider: &str) -> Option<&str> {
    let organization = provider.strip_prefix("oidc-")?;
    (organization.is_ascii()
        && organization.len() <= 43
        && crate::account_input::validate_namespace(organization, 3).is_ok())
    .then_some(organization)
}

pub fn has_scope(scopes: &str, scope: &str) -> bool {
    scopes.split(',').any(|granted| {
        granted == scope
            || (scope == "read:org" && granted == "write:org")
            || (matches!(scope, "read:user" | "user:email" | "user:follow") && granted == "user")
    })
}

/// GitHub's key scopes: an admin scope includes write, and write includes read.
fn key_scope(scopes: &str, kind: &str, write: bool) -> bool {
    has_scope(scopes, &format!("admin:{kind}"))
        || has_scope(scopes, &format!("write:{kind}"))
        || (!write && has_scope(scopes, &format!("read:{kind}")))
}

/// OAuth permissions complement, rather than replace, the account's live access rules.
pub fn allows(scopes: &str, path: &str, write: bool) -> bool {
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        [""] | ["user"] => !write,
        ["repos", _, _, "code-scanning", "default-setup", ..] => has_scope(scopes, "repo"),
        ["repos", _, _, "code-scanning" | "secret-scanning", ..]
        | ["orgs", _, "code-scanning" | "secret-scanning", ..]
        | ["repos", _, _, "dependabot", "alerts", ..]
        | ["orgs", _, "dependabot", "alerts", ..] => {
            has_scope(scopes, "repo") || has_scope(scopes, "security_events")
        }
        ["orgs", _, "code-security", ..] => {
            has_scope(scopes, "write:org") || (!write && has_scope(scopes, "read:org"))
        }
        ["user", "docker", "conflicts"]
        | ["users" | "orgs", _, "docker", "conflicts"]
        | ["user", "packages", ..]
        | ["users" | "orgs", _, "packages", ..] => !write && has_scope(scopes, "read:packages"),
        ["user", "repos" | "starred" | "subscriptions", ..]
        | ["repositories", ..]
        | ["repos", ..]
        | ["orgs", _, "repos"]
        | ["orgs", _, "attestations" | "artifacts", ..]
        | ["users", _, "attestations", ..]
        | ["notifications", ..] => has_scope(scopes, "repo"),
        ["orgs", _] if write => has_scope(scopes, "repo"),
        ["user", "teams"] => {
            !write
                && (has_scope(scopes, "repo")
                    || has_scope(scopes, "read:org")
                    || has_scope(scopes, "write:org"))
        }
        ["orgs", _, "public_members", ..] => {
            has_scope(scopes, "write:org") || (!write && has_scope(scopes, "read:org"))
        }
        ["user", "orgs"] | ["orgs", _] | ["orgs", _, "issue-types", ..] => {
            !write && (has_scope(scopes, "read:org") || has_scope(scopes, "write:org"))
        }
        ["orgs", _, "rulesets", ..] => {
            has_scope(scopes, "write:org") || (!write && has_scope(scopes, "read:org"))
        }
        ["issues"] | ["user", "issues"] | ["orgs", _, "issues"] => {
            !write && has_scope(scopes, "repo")
        }
        ["users", _, "events" | "received_events", ..]
        | ["orgs", _, "events", ..]
        | ["networks", _, _, "events"] => !write && has_scope(scopes, "repo"),
        ["users"]
        | ["organizations", ..]
        | ["users", _, "orgs" | "gists" | "social_accounts"]
        | ["feeds"]
        | ["events"] => !write,
        ["orgs", _, "members" | "memberships" | "teams", ..]
        | ["user", "memberships", "orgs", ..] => {
            has_scope(scopes, "write:org") || (!write && has_scope(scopes, "read:org"))
        }
        ["gists", ..] => has_scope(scopes, "gist"),
        ["markdown"] | ["markdown", "raw"] => true,
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
        ["user", "gpg_keys", _] if write => has_scope(scopes, "admin:gpg_key"),
        ["user", "gpg_keys"] | ["user", "gpg_keys", _] => key_scope(scopes, "gpg_key", write),
        ["user", "keys"] | ["user", "keys", _] => key_scope(scopes, "public_key", write),
        ["user", "ssh_signing_keys"] | ["user", "ssh_signing_keys", _] => {
            key_scope(scopes, "ssh_signing_key", write)
        }
        _ => false,
    }
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
        || has_scope(scopes, "write:gpg_key")
        || has_scope(scopes, "admin:gpg_key")
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
    let actual = crate::oauth_apps::pkce_challenge(verifier);
    bool::from(actual.as_bytes().ct_eq(challenge.as_bytes()))
}
/// The RFC 7636 S256 challenge for a PKCE verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
pub fn validated_public_origin(value: &str) -> Option<String> {
    let url = crate::http_config::parse_public_url(value).ok()?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        return None;
    }
    Some(url.origin().ascii_serialization())
}
