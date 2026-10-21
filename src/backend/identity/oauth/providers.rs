//! Supported provider requests and bounded response parsing. One configured HTTP client.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The providers that can sign a user in.
pub const PROVIDERS: [&str; 5] = ["github", "google", "gitlab", "discord", "bitbucket"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderMeta {
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub user_info_url: &'static str,
    /// Space-separated scopes, as the authorize URL needs them.
    pub scopes: &'static str,
}

/// Endpoints and scopes for a provider; `None` for an unknown one.
pub fn provider_meta(provider: &str) -> Option<ProviderMeta> {
    if !PROVIDERS.contains(&provider) {
        return None;
    }

    let meta = match provider {
        "github" => ProviderMeta {
            auth_url: "https://github.com/login/oauth/authorize",
            token_url: "https://github.com/login/oauth/access_token",
            user_info_url: "https://api.github.com/user",
            scopes: "read:user",
        },
        "google" => ProviderMeta {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
            token_url: "https://oauth2.googleapis.com/token",
            user_info_url: "https://www.googleapis.com/oauth2/v2/userinfo",
            scopes: "openid profile",
        },
        "gitlab" => ProviderMeta {
            auth_url: "https://gitlab.com/oauth/authorize",
            token_url: "https://gitlab.com/oauth/token",
            user_info_url: "https://gitlab.com/api/v4/user",
            scopes: "read_user",
        },
        "discord" => ProviderMeta {
            auth_url: "https://discord.com/api/oauth2/authorize",
            token_url: "https://discord.com/api/oauth2/token",
            user_info_url: "https://discord.com/api/users/@me",
            scopes: "identify",
        },
        "bitbucket" => ProviderMeta {
            auth_url: "https://bitbucket.org/site/oauth2/authorize",
            token_url: "https://bitbucket.org/site/oauth2/access_token",
            user_info_url: "https://api.bitbucket.org/2.0/user",
            scopes: "account",
        },
        _ => return None,
    };
    Some(meta)
}

/// `github` -> `("GITHUB_CLIENT_ID", "GITHUB_CLIENT_SECRET")`.
pub fn credential_env_keys(provider: &str) -> (String, String) {
    let prefix = provider.to_ascii_uppercase();
    (
        format!("{prefix}_CLIENT_ID"),
        format!("{prefix}_CLIENT_SECRET"),
    )
}

/// The client id and secret from the environment; both must be set and non-empty.
pub fn provider_credentials(provider: &str) -> Option<(String, String)> {
    let (id_key, secret_key) = credential_env_keys(provider);
    let read = |key: &str| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    Some((read(&id_key)?, read(&secret_key)?))
}

/// Percent encoding that leaves only RFC 3986 unreserved characters as they are.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The authorize URL the browser is redirected to.
pub fn authorize_url(
    meta: &ProviderMeta,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
) -> String {
    format!(
        "{}?client_id={}&redirect_uri={}&scope={}&state={}&response_type=code",
        meta.auth_url,
        encode(client_id),
        encode(redirect_uri),
        encode(meta.scopes),
        encode(state),
    )
}

/// A username from the provider's display name: lowercase, keep `[a-z0-9_-]`,
/// falling back to `user_<first 8 chars of the provider id>`.
pub fn username_from_profile(name: Option<&str>, provider_user_id: &str) -> String {
    let derived: String = name
        .unwrap_or_default()
        .to_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .take(200)
        .collect();

    if derived.is_empty() {
        let prefix: String = provider_user_id
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .take(8)
            .collect();
        format!("user_{prefix}")
    } else if crate::backend::db::users::is_reserved_name(&derived) {
        format!("{derived}_user")
    } else {
        derived
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProfile {
    pub provider_user_id: String,
    pub name: Option<String>,
}

fn as_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn as_optional_string(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(|text| text.to_string())
}

/// The account id and display name from a provider's user-info response. The email is never
/// read: accounts are linked by provider id only.
pub fn profile_from_user_info(provider: &str, data: &Value) -> Option<OAuthProfile> {
    let (id_field, name_fields): (&str, &[&str]) = match provider {
        "github" => ("id", &["name", "login"]),
        "google" => ("id", &["name"]),
        "gitlab" => ("id", &["name", "username"]),
        "discord" => ("id", &["global_name", "username"]),
        "bitbucket" => ("uuid", &["display_name"]),
        _ => return None,
    };
    Some(OAuthProfile {
        provider_user_id: as_id(data.get(id_field))?,
        name: name_fields
            .iter()
            .find_map(|field| as_optional_string(data.get(*field))),
    })
}

/// Only these providers take a PKCE code challenge.
pub(crate) fn supports_pkce(provider: &str) -> bool {
    matches!(provider, "github" | "google" | "gitlab")
}
pub(crate) fn browser_binding(nonce: &str) -> String {
    hex::encode(Sha256::digest(nonce.as_bytes()))
}
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(crate) fn authorization_with_pkce(
    meta: &ProviderMeta,
    client: &str,
    redirect: &str,
    state: &str,
    verifier: &str,
) -> String {
    let mut url = authorize_url(meta, client, redirect, state);
    if !verifier.is_empty() {
        url.push_str(&format!(
            "&code_challenge={}&code_challenge_method=S256",
            pkce_challenge(verifier)
        ));
    }
    url
}

pub(crate) fn token_body(
    provider: &str,
    client: &str,
    secret: &str,
    code: &str,
    redirect: &str,
    verifier: &str,
) -> String {
    // Bitbucket requires HTTP Basic confidential-client authentication. Never
    // send both Basic and client_secret_post authentication in the same request.
    let mut body = if provider == "bitbucket" {
        format!(
            "code={}&redirect_uri={}&grant_type=authorization_code",
            encode(code),
            encode(redirect)
        )
    } else {
        format!(
            "client_id={}&client_secret={}&code={}&redirect_uri={}&grant_type=authorization_code",
            encode(client),
            encode(secret),
            encode(code),
            encode(redirect)
        )
    };
    if !verifier.is_empty() {
        body.push_str(&format!("&code_verifier={}", encode(verifier)));
    }
    body
}

pub(crate) fn provider_client() -> Result<reqwest::Client, ()> {
    static CLIENT: std::sync::OnceLock<Result<reqwest::Client, ()>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .connect_timeout(std::time::Duration::from_secs(3))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("Codeyard OAuth")
                .build()
                .map_err(|_| ())
        })
        .clone()
}

async fn bounded_json(mut response: reqwest::Response) -> Result<Value, ()> {
    if !response.status().is_success()
        || response.content_length().is_some_and(|size| size > 65_536)
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len() + chunk.len() > 65_536 {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

// Concrete provider HTTP boundary also used by controlled fixture tests.
pub(crate) async fn exchange_profile(
    client: &reqwest::Client,
    provider: &str,
    token_url: &str,
    info_url: &str,
    body: String,
    basic: Option<(&str, &str)>,
) -> Result<OAuthProfile, ()> {
    let mut request = client
        .post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body);
    if let Some((id, secret)) = basic {
        request = request.basic_auth(id, Some(secret));
    }
    let response = request.send().await.map_err(|_| ())?;
    let token = bounded_json(response).await?;
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 8192)
        .ok_or(())?;
    if token
        .get("token_type")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
    {
        return Err(());
    }
    let response = client
        .get(info_url)
        .bearer_auth(access)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| ())?;
    let profile = profile_from_user_info(provider, &bounded_json(response).await?).ok_or(())?;
    if profile.provider_user_id.is_empty() || profile.provider_user_id.len() > 255 {
        return Err(());
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::auth::oauth::validated_public_origin;
    use axum::{
        http::{header, HeaderMap, StatusCode},
        Json,
    };

    #[test]
    fn provider_metadata_covers_every_provider() {
        for provider in PROVIDERS {
            let meta = provider_meta(provider).expect("known provider");
            assert!(meta.auth_url.starts_with("https://"));
            assert!(meta.token_url.starts_with("https://"));
            assert!(meta.user_info_url.starts_with("https://"));
            assert!(!meta.scopes.is_empty());
        }
        assert!(provider_meta("mastodon").is_none());
    }

    #[test]
    fn credential_env_keys_follow_the_provider_prefix() {
        assert_eq!(
            credential_env_keys("github"),
            (
                "GITHUB_CLIENT_ID".to_string(),
                "GITHUB_CLIENT_SECRET".to_string()
            )
        );
        assert_eq!(credential_env_keys("bitbucket").0, "BITBUCKET_CLIENT_ID");
    }

    #[test]
    fn authorize_url_encodes_every_parameter() {
        let meta = provider_meta("github").unwrap();
        let url = authorize_url(
            &meta,
            "client id",
            "https://git.example/api/auth/oauth/github/callback",
            "state-1",
        );
        assert!(url.starts_with("https://github.com/login/oauth/authorize?"));
        // A space becomes %20; colon and slash are encoded too.
        assert!(url.contains("client_id=client%20id"));
        assert!(url.contains("scope=read%3Auser"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fgit.example%2Fapi%2Fauth%2Foauth%2Fgithub%2Fcallback"
        ));
        assert!(url.contains("state=state-1"));
        assert!(url.ends_with("&response_type=code"));
    }

    #[test]
    fn usernames_keep_only_safe_characters() {
        assert_eq!(
            username_from_profile(Some("Ada Lovelace"), "42"),
            "adalovelace"
        );
        assert_eq!(username_from_profile(Some("ada_l-1"), "42"), "ada_l-1");
        assert_eq!(
            username_from_profile(Some("!!! "), "1234567890"),
            "user_12345678"
        );
        assert_eq!(username_from_profile(None, "abcdefghij"), "user_abcdefgh");
        // Non-ASCII letters are stripped.
        assert_eq!(username_from_profile(Some("Ünïcode"), "1"), "ncode");
        // A name that is a page of the site gets a suffix.
        assert_eq!(username_from_profile(Some("Admin"), "1"), "admin_user");
    }

    #[test]
    fn profiles_map_per_provider() {
        let github: Value = serde_json::json!({ "id": 7, "name": null, "login": "octocat" });
        let profile = profile_from_user_info("github", &github).unwrap();
        assert_eq!(profile.provider_user_id, "7");
        assert_eq!(profile.name.as_deref(), Some("octocat"));

        let discord: Value = serde_json::json!({
            "id": "99", "global_name": null, "username": "ada", "avatar": "hash"
        });
        let profile = profile_from_user_info("discord", &discord).unwrap();
        assert_eq!(profile.provider_user_id, "99");
        assert_eq!(profile.name.as_deref(), Some("ada"));

        let bitbucket: Value = serde_json::json!({ "uuid": "{abc}", "display_name": "Ada" });
        let profile = profile_from_user_info("bitbucket", &bitbucket).unwrap();
        assert_eq!(profile.provider_user_id, "{abc}");
        assert_eq!(profile.name.as_deref(), Some("Ada"));

        let google: Value = serde_json::json!({ "id": "g-1", "name": "G" });
        assert_eq!(
            profile_from_user_info("google", &google)
                .unwrap()
                .provider_user_id,
            "g-1"
        );

        let gitlab: Value = serde_json::json!({ "id": 5, "name": null, "username": "gl" });
        let profile = profile_from_user_info("gitlab", &gitlab).unwrap();
        assert_eq!(profile.provider_user_id, "5");
        assert_eq!(profile.name.as_deref(), Some("gl"));

        // A profile with no id cannot be linked.
        assert!(profile_from_user_info("github", &serde_json::json!({})).is_none());
        assert!(profile_from_user_info("mastodon", &github).is_none());
    }

    #[test]
    fn origin_and_pkce_are_pinned_and_version_matched() {
        assert_eq!(
            validated_public_origin("https://git.example/").as_deref(),
            Some("https://git.example")
        );
        assert!(validated_public_origin("https://attacker@git.example").is_none());
        assert!(validated_public_origin("http://git.example").is_none());
        assert!(validated_public_origin("https://git.example/path").is_none());
        assert!(validated_public_origin("https://git.example/?x=1").is_none());
        assert!(validated_public_origin("http://127.0.0.1:8080").is_some());
        // RFC 7636 Appendix B vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let url = authorization_with_pkce(
            &provider_meta("google").unwrap(),
            "id",
            "https://git.example",
            "state",
            verifier,
        );
        assert!(url.contains("code_challenge_method=S256"));
        assert!(
            token_body("google", "id", "secret", "code", "redirect", verifier)
                .contains(&format!("code_verifier={verifier}"))
        );
        assert!(!supports_pkce("bitbucket"));
        assert!(
            !token_body("bitbucket", "id", "secret", "code", "redirect", "")
                .contains("client_secret")
        );
        assert!(!supports_pkce("discord"));
    }

    #[tokio::test]
    async fn controlled_provider_exchange_rejects_failure_redirect_malformed_and_oversize() {
        use axum::{
            routing::{get, post},
            Router,
        };
        let router = Router::new()
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({"access_token":"fixture-token","token_type":"Bearer"}))
                }),
            )
            .route(
                "/profile",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Bearer fixture-token");
                    Json(serde_json::json!({"id":42,"login":"approved"}))
                }),
            )
            .route(
                "/bitbucket-token",
                post(|headers: HeaderMap, body: String| async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Basic aWQ6c2VjcmV0");
                    assert!(!body.contains("client_secret"));
                    Json(serde_json::json!({"access_token":"fixture-token","token_type":"Bearer"}))
                }),
            )
            .route(
                "/bitbucket-profile",
                get(|| async {
                    Json(serde_json::json!({"uuid":"{owned}","display_name":"approved"}))
                }),
            )
            .route("/failure", post(|| async { StatusCode::BAD_GATEWAY }))
            .route(
                "/redirect",
                post(|| async { (StatusCode::FOUND, [(header::LOCATION, "/token")]) }),
            )
            .route("/malformed", post(|| async { "not-json" }))
            .route("/missing", post(|| async { Json(serde_json::json!({})) }))
            .route("/huge", post(|| async { "x".repeat(65_537) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = provider_client().unwrap();
        let profile = exchange_profile(
            &client,
            "github",
            &format!("{origin}/token"),
            &format!("{origin}/profile"),
            "fixture".into(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(profile.provider_user_id, "42");
        let profile = exchange_profile(
            &client,
            "bitbucket",
            &format!("{origin}/bitbucket-token"),
            &format!("{origin}/bitbucket-profile"),
            token_body("bitbucket", "id", "secret", "code", "redirect", ""),
            Some(("id", "secret")),
        )
        .await
        .unwrap();
        assert_eq!(profile.provider_user_id, "{owned}");
        assert!(exchange_profile(
            &client,
            "github",
            &format!("{origin}/token"),
            &format!("{origin}/missing-profile"),
            "fixture".into(),
            None
        )
        .await
        .is_err());
        for path in ["failure", "redirect", "malformed", "missing", "huge"] {
            assert!(exchange_profile(
                &client,
                "github",
                &format!("{origin}/{path}"),
                &format!("{origin}/profile"),
                "fixture".into(),
                None
            )
            .await
            .is_err());
        }
        task.abort();
        let _ = task.await;
    }
}
