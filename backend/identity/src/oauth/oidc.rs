//! Organization-specific OIDC issuers configured by the operator; no email-based linking.
use super::providers::{bounded_json, provider_client, OAuthProfile};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rsa::{
    pkcs1v15::{Signature, VerifyingKey},
    signature::Verifier,
    traits::PublicKeyParts,
    BigUint, RsaPublicKey,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub struct Provider {
    pub organization: String,
    issuer: String,
    authorize: String,
    token: String,
    jwks: String,
    client: String,
    secret: String,
}

/// Provider names fit both engines' existing 50-character OAuth namespace.
pub fn provider_name(organization: &str) -> Option<String> {
    let provider = format!("oidc-{organization}");
    codeyard_types::oauth_apps::organization_from_oidc_provider(&provider)?;
    Some(provider)
}

fn endpoint(value: &str, issuer: bool) -> Result<String> {
    ensure!(value.len() <= 2048, "OIDC endpoint is too long");
    let url = url::Url::parse(value)?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "OIDC requires HTTPS"
    );
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && (!issuer || url.query().is_none()),
        "invalid OIDC endpoint"
    );
    Ok(value.to_owned())
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("missing OIDC configuration field")
}

fn valid_subject(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 255
            && value.is_ascii()
            && !value.chars().any(char::is_control),
        "invalid OIDC subject"
    );
    Ok(())
}

/// Local operators obtain the case-sensitive subject from their identity provider.
/// The database rechecks the named administrator and the target's membership at commit.
pub async fn enroll(
    pool: &codeyard_database::DbPool,
    administrator: &str,
    username: &str,
    organization: &str,
    subject: &str,
) -> Result<()> {
    for name in [administrator, username] {
        codeyard_types::account_input::validate_namespace(name, 3)
            .map_err(|_| anyhow::anyhow!("invalid account name"))?;
    }
    valid_subject(subject)?;
    let name = provider_name(organization).context("invalid OIDC organization name")?;
    let configured = provider(&name).context("organization OIDC provider is not configured")?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let actor =
            codeyard_db_identity::users::accounts::get_user_by_username(pool, administrator)
                .await?
                .context("administrator not found")?;
        let user = codeyard_db_identity::users::accounts::get_user_by_username(pool, username)
            .await?
            .context("account not found")?;
        ensure!(
            codeyard_db_identity::users::transitions::enroll_oauth_identity(
                pool,
                actor.id,
                user.id,
                &name,
                &configured.identity(subject)
            )
            .await?,
            "OIDC enrollment refused or identity is already enrolled"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

fn parse(raw: &str) -> Result<Vec<(String, Provider)>> {
    ensure!(raw.len() <= 65_536, "OIDC configuration is too large");
    let configuration: Value = serde_json::from_str(raw)?;
    let entries = configuration
        .as_array()
        .context("OIDC_PROVIDERS must be an array")?;
    ensure!(entries.len() <= 32, "too many OIDC providers");
    let mut result = Vec::new();
    for entry in entries {
        let object = entry.as_object().context("invalid OIDC provider")?;
        ensure!(
            object.keys().all(|key| matches!(
                key.as_str(),
                "organization"
                    | "issuer"
                    | "authorization_url"
                    | "token_url"
                    | "jwks_url"
                    | "client_id"
                    | "client_secret_env"
            )),
            "unknown OIDC configuration field"
        );
        let organization = field(entry, "organization")?.to_owned();
        let name = provider_name(&organization).context("invalid OIDC organization name")?;
        ensure!(
            !result.iter().any(|(existing, _)| existing == &name),
            "duplicate OIDC organization"
        );
        let client = field(entry, "client_id")?;
        ensure!(
            client.len() <= 255 && !client.chars().any(char::is_control),
            "invalid OIDC client id"
        );
        let secret_env = field(entry, "client_secret_env")?;
        ensure!(
            secret_env.len() <= 128
                && secret_env.starts_with(|ch: char| ch.is_ascii_uppercase() || ch == '_')
                && secret_env
                    .bytes()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == b'_'),
            "invalid OIDC secret environment name"
        );
        let secret = std::env::var(secret_env).context("OIDC client secret is unavailable")?;
        ensure!(
            !secret.is_empty() && secret.len() <= 16_384,
            "invalid OIDC client secret"
        );
        result.push((
            name,
            Provider {
                organization,
                issuer: endpoint(field(entry, "issuer")?, true)?,
                authorize: endpoint(field(entry, "authorization_url")?, false)?,
                token: endpoint(field(entry, "token_url")?, false)?,
                jwks: endpoint(field(entry, "jwks_url")?, false)?,
                client: client.to_owned(),
                secret,
            },
        ));
    }
    Ok(result)
}

/// Invalid configuration disables the entire configured set instead of partially accepting it.
pub fn provider(name: &str) -> Option<Provider> {
    if !name.starts_with("oidc-") {
        return None;
    }
    parse(&std::env::var("OIDC_PROVIDERS").ok()?)
        .ok()?
        .into_iter()
        .find_map(|(id, provider)| (id == name).then_some(provider))
}

impl Provider {
    fn identity(&self, subject: &str) -> String {
        let mut identity = Sha256::new();
        identity.update((self.issuer.len() as u64).to_be_bytes());
        identity.update(self.issuer.as_bytes());
        identity.update(subject.as_bytes());
        hex::encode(identity.finalize())
    }

    pub fn credentials(&self) -> (String, String) {
        (self.client.clone(), self.secret.clone())
    }

    // An issuer or client change invalidates unfinished flows. Accounts are also namespaced
    // by the issuer, so replacing an identity provider cannot take over existing accounts.
    fn binding(&self) -> String {
        let mut hash = Sha256::new();
        for value in [
            &self.issuer,
            &self.client,
            &self.authorize,
            &self.token,
            &self.jwks,
        ] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        format!("oidc:{}", &hex::encode(hash.finalize())[..40])
    }

    pub async fn begin(
        &self,
        pool: &codeyard_database::DbPool,
        redirect: &str,
        browser: &str,
        state: &str,
        verifier: &str,
        return_to: &str,
    ) -> Result<String> {
        ensure!(
            codeyard_security::crypto::is_hex_token(verifier, 64),
            "invalid OIDC verifier"
        );
        codeyard_db_identity::users::sessions::save_bound_oauth_state(
            pool,
            state,
            verifier,
            &self.binding(),
            redirect,
            &super::providers::browser_binding(browser),
            return_to,
        )
        .await?;
        let mut url = url::Url::parse(&self.authorize)?;
        let challenge = codeyard_types::oauth_apps::pkce_challenge(verifier);
        url.query_pairs_mut()
            .append_pair("client_id", &self.client)
            .append_pair("redirect_uri", redirect)
            .append_pair("scope", "openid profile")
            .append_pair("state", state)
            .append_pair("response_type", "code")
            .append_pair("nonce", &challenge)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    pub async fn complete(
        &self,
        pool: &codeyard_database::DbPool,
        code: &str,
        state: &str,
        browser: &str,
        redirect: &str,
    ) -> Result<(OAuthProfile, String), super::OAuthError> {
        let stored = codeyard_db_identity::users::sessions::consume_bound_oauth_state(
            pool,
            state,
            &self.binding(),
            &super::providers::browser_binding(browser),
            redirect,
        )
        .await
        .map_err(|_| super::OAuthError::StateUnavailable)?
        .ok_or(super::OAuthError::InvalidState)?;
        if !codeyard_security::crypto::is_hex_token(&stored.pkce_verifier, 64) {
            return Err(super::OAuthError::InvalidPkce);
        }
        let now = pool
            .unix_time()
            .await
            .map_err(|_| super::OAuthError::StateUnavailable)?;
        let profile = self
            .exchange(code, redirect, &stored.pkce_verifier, now)
            .await
            .map_err(|_| super::OAuthError::ProviderFailed)?;
        Ok((profile, stored.return_to))
    }

    async fn exchange(
        &self,
        code: &str,
        redirect: &str,
        verifier: &str,
        now: i64,
    ) -> Result<OAuthProfile> {
        let client = provider_client().map_err(|_| anyhow::anyhow!("OIDC client unavailable"))?;
        let body = super::providers::token_body(
            "oidc",
            &self.client,
            &self.secret,
            code,
            redirect,
            verifier,
        );
        let token = bounded_json(
            client
                .post(&self.token)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("accept", "application/json")
                .body(body)
                .send()
                .await?,
        )
        .await
        .map_err(|_| anyhow::anyhow!("OIDC token exchange failed"))?;
        let id = field(&token, "id_token")?;
        ensure!(id.len() <= 16_384, "OIDC token is too large");
        let access = field(&token, "access_token")?;
        ensure!(
            access.len() <= 8192
                && access.is_ascii()
                && !access.chars().any(char::is_control)
                && token["token_type"]
                    .as_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case("bearer")),
            "invalid OIDC token response"
        );
        let jwks = bounded_json(
            client
                .get(&self.jwks)
                .header("accept", "application/json")
                .send()
                .await?,
        )
        .await
        .map_err(|_| anyhow::anyhow!("OIDC keys unavailable"))?;
        // Public-key work is bounded and owned even if the HTTP request is cancelled.
        static VERIFY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
        let permit = VERIFY.try_acquire().context("OIDC verification busy")?;
        let value = id.to_owned();
        let issuer = self.issuer.clone();
        let audience = self.client.clone();
        let nonce = codeyard_types::oauth_apps::pkce_challenge(verifier);
        let claims = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            verify(&value, &jwks, &issuer, &audience, &nonce, now)
        })
        .await??;
        if let Some(expected) = claims.get("at_hash") {
            let digest = Sha256::digest(access.as_bytes());
            ensure!(
                expected.as_str() == Some(URL_SAFE_NO_PAD.encode(&digest[..16]).as_str()),
                "OIDC access token hash mismatch"
            );
        }
        Ok(OAuthProfile {
            provider_user_id: self.identity(field(&claims, "sub")?),
            name: claims["name"]
                .as_str()
                .filter(|v| v.len() <= 255)
                .map(str::to_owned),
        })
    }
}

fn verify(
    value: &str,
    jwks: &Value,
    issuer: &str,
    client: &str,
    nonce: &str,
    now: i64,
) -> Result<Value> {
    ensure!(value.len() <= 16_384, "OIDC token is too large");
    let pieces: Vec<_> = value.split('.').collect();
    ensure!(pieces.len() == 3, "invalid OIDC token");
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[0])?)?;
    ensure!(
        header["alg"] == "RS256"
            && header.get("crit").is_none()
            && header.get("jku").is_none()
            && header.get("jwk").is_none()
            && header.get("x5u").is_none()
            && header.get("b64").is_none(),
        "unsupported OIDC signature"
    );
    let kid = header
        .get("kid")
        .map(|v| {
            v.as_str()
                .filter(|v| !v.is_empty() && v.len() <= 255)
                .context("invalid OIDC key id")
        })
        .transpose()?;
    let keys = jwks["keys"].as_array().context("invalid OIDC keys")?;
    ensure!(keys.len() <= 64, "too many OIDC keys");
    let mut candidates = keys.iter().filter(|key| {
        key["kty"] == "RSA"
            && key.get("alg").is_none_or(|alg| alg == "RS256")
            && key.get("use").is_none_or(|usage| usage == "sig")
            && kid.is_none_or(|kid| key["kid"].as_str() == Some(kid))
    });
    let key = candidates.next().context("OIDC signing key not found")?;
    ensure!(candidates.next().is_none(), "ambiguous OIDC signing key");
    if let Some(operations) = key.get("key_ops") {
        ensure!(
            operations
                .as_array()
                .is_some_and(|ops| ops.len() <= 16 && ops.iter().any(|op| op == "verify")),
            "OIDC key cannot verify"
        );
    }
    let n = URL_SAFE_NO_PAD.decode(field(key, "n")?)?;
    let e = URL_SAFE_NO_PAD.decode(field(key, "e")?)?;
    ensure!(
        (256..=1024).contains(&n.len()) && (1..=4).contains(&e.len()),
        "unsupported OIDC key size"
    );
    let key = RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e))?;
    ensure!(
        (2048..=8192).contains(&key.n().bits()),
        "unsupported OIDC key size"
    );
    let signature = Signature::try_from(URL_SAFE_NO_PAD.decode(pieces[2])?.as_slice())?;
    VerifyingKey::<Sha256>::new(key).verify(
        format!("{}.{}", pieces[0], pieces[1]).as_bytes(),
        &signature,
    )?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[1])?)?;
    valid_subject(field(&claims, "sub")?)?;
    ensure!(
        claims["iss"].as_str() == Some(issuer) && claims["nonce"].as_str() == Some(nonce),
        "OIDC issuer, subject or nonce mismatch"
    );
    let audiences: Vec<&str> = match &claims["aud"] {
        Value::String(value) => vec![value],
        Value::Array(values) if !values.is_empty() && values.len() <= 16 => values
            .iter()
            .map(|v| v.as_str().context("invalid OIDC audience"))
            .collect::<Result<_>>()?,
        _ => anyhow::bail!("invalid OIDC audience"),
    };
    ensure!(
        audiences.contains(&client)
            && audiences.iter().all(|audience| *audience == client)
            && (audiences.len() == 1 || claims["azp"].as_str() == Some(client))
            && claims.get("azp").is_none_or(|v| v.as_str() == Some(client)),
        "OIDC audience mismatch"
    );
    let iat = claims["iat"].as_i64().context("missing OIDC issued time")?;
    let exp = claims["exp"].as_i64().context("missing OIDC expiry")?;
    ensure!(
        iat >= now - 660
            && iat <= now + 60
            && exp > now - 60
            && exp > iat
            && exp
                .checked_sub(iat)
                .is_some_and(|seconds| seconds <= 86_400)
            && claims
                .get("nbf")
                .is_none_or(|v| v.as_i64().is_some_and(|v| v <= now + 60)),
        "expired or future OIDC token"
    );
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::signature::{SignatureEncoding, Signer};

    fn key() -> &'static rsa::RsaPrivateKey {
        static KEY: std::sync::OnceLock<rsa::RsaPrivateKey> = std::sync::OnceLock::new();
        KEY.get_or_init(|| rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap())
    }
    fn token(claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"fixture"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        let data = format!("{header}.{body}");
        let signature =
            rsa::pkcs1v15::SigningKey::<Sha256>::new(key().clone()).sign(data.as_bytes());
        format!("{data}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }
    #[test]
    fn signed_tokens_require_the_pinned_issuer_audience_nonce_time_and_key() {
        let key = key();
        let jwks = serde_json::json!({"keys":[{"kty":"RSA","kid":"fixture","alg":"RS256","use":"sig",
            "n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())}]});
        let claims = serde_json::json!({"iss":"https://id.example/realm/acme","sub":"member","aud":"codeyard",
            "nonce":"browser nonce","iat":10_000,"exp":10_300});
        assert!(verify(
            &token(&claims),
            &jwks,
            "https://id.example/realm/acme",
            "codeyard",
            "browser nonce",
            10_001
        )
        .is_ok());
        for (field, value) in [
            ("iss", serde_json::json!("https://evil.example")),
            ("aud", serde_json::json!("another client")),
            ("nonce", serde_json::json!("another browser")),
            ("exp", serde_json::json!(9900)),
            ("iat", serde_json::json!(11_000)),
            ("sub", serde_json::json!("")),
            ("sub", serde_json::json!("non-ascii-é")),
            ("sub", serde_json::json!("control\u{0}")),
            ("aud", serde_json::json!(["codeyard", "other"])),
            ("azp", serde_json::json!("other")),
            ("nbf", serde_json::json!(11_000)),
        ] {
            let mut bad = claims.clone();
            bad[field] = value;
            assert!(
                verify(
                    &token(&bad),
                    &jwks,
                    "https://id.example/realm/acme",
                    "codeyard",
                    "browser nonce",
                    10_001
                )
                .is_err(),
                "accepted {field}"
            );
        }
        let mut multiple = claims.clone();
        multiple["aud"] = serde_json::json!(["codeyard", "other"]);
        multiple["azp"] = serde_json::json!("codeyard");
        assert!(verify(
            &token(&multiple),
            &jwks,
            "https://id.example/realm/acme",
            "codeyard",
            "browser nonce",
            10_001
        )
        .is_err());
        multiple["aud"] = serde_json::json!(["codeyard"]);
        assert!(verify(
            &token(&multiple),
            &jwks,
            "https://id.example/realm/acme",
            "codeyard",
            "browser nonce",
            10_001
        )
        .is_ok());
        let mut keys = jwks.clone();
        keys["keys"][0]["key_ops"] = serde_json::json!(["encrypt"]);
        assert!(verify(
            &token(&claims),
            &keys,
            "https://id.example/realm/acme",
            "codeyard",
            "browser nonce",
            10_001
        )
        .is_err());
        let mut keys = jwks.clone();
        keys["keys"]
            .as_array_mut()
            .unwrap()
            .push(jwks["keys"][0].clone());
        assert!(verify(
            &token(&claims),
            &keys,
            "https://id.example/realm/acme",
            "codeyard",
            "browser nonce",
            10_001
        )
        .is_err());
        assert!(verify(&"x".repeat(16_385), &jwks, "x", "x", "x", 0).is_err());
        let mut forged = token(&claims);
        forged.push('x');
        assert!(verify(
            &forged,
            &jwks,
            "https://id.example/realm/acme",
            "codeyard",
            "browser nonce",
            10_001
        )
        .is_err());
    }
    #[test]
    fn operator_endpoints_and_provider_names_are_bounded() {
        for bad in [
            "http://id.example",
            "https://user:password@id.example",
            "https://id.example/#fragment",
            "file:///tmp/key",
        ] {
            assert!(endpoint(bad, true).is_err());
        }
        assert!(endpoint("https://id.example/realms/acme", true).is_ok());
        assert!(endpoint("http://127.0.0.1:8080", true).is_ok());
        for bad in ["a", "../../acme", "acme?query", &"x".repeat(44)] {
            assert!(provider_name(bad).is_none());
        }
        assert_eq!(provider_name("acme").as_deref(), Some("oidc-acme"));
        assert!(parse(&"x".repeat(65_537)).is_err());
        assert!(parse("{}").is_err());
        assert!(parse("[{\"organization\":\"acme\",\"command\":\"no\"}]").is_err());
    }
}
