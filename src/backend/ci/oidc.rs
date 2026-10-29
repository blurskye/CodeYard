//! OIDC identities come from admitted workflows and live job credentials, never request claims.
use crate::backend::{
    db::{self, DbPool},
    identity::github_apps,
};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rsa::{
    pkcs8::DecodePrivateKey,
    signature::{SignatureEncoding, Signer},
    traits::PublicKeyParts,
    RsaPrivateKey,
};
use serde_json::{json, Value};

pub fn issuer() -> String {
    crate::backend::api::http::config::link_origin()
        .trim_end_matches('/')
        .to_owned()
}

pub struct SigningKeys {
    pool: DbPool,
    key: tokio::sync::OnceCell<std::sync::Arc<SigningKey>>,
}

struct SigningKey {
    signer: rsa::pkcs1v15::SigningKey<sha2::Sha256>,
    kid: String,
    jwks: Value,
}

impl SigningKeys {
    pub fn new(pool: DbPool) -> Self { Self { pool, key: tokio::sync::OnceCell::new() } }

    async fn key(&self) -> Result<std::sync::Arc<SigningKey>> {
        self.key.get_or_try_init(|| async {
            let pem = match db::oidc::key(&self.pool).await? {
                Some(pem) => pem,
                None => {
                    let (pem, _) = github_apps::generate_key().await?;
                    db::oidc::store_key(&self.pool, &pem).await?
                }
            };
            tokio::task::spawn_blocking(move || -> Result<_> {
                let key = RsaPrivateKey::from_pkcs8_pem(&pem)?;
                let kid = kid(&key);
                let jwks = json!({"keys":[{"kty":"RSA","use":"sig","alg":"RS256","kid":kid,
                    "n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())}]});
                Ok(std::sync::Arc::new(SigningKey { signer: rsa::pkcs1v15::SigningKey::new(key), kid, jwks }))
            }).await?
        }).await.cloned()
    }
}

fn kid(key: &RsaPrivateKey) -> String {
    super::runtime::token_hash(&URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()))
}

pub async fn jwks(keys: &SigningKeys) -> Result<Value> {
    Ok(keys.key().await?.jwks.clone())
}

pub fn escape(value: &str) -> String {
    value.replace(':', "%3A")
}

/// User and organization IDs overlap, so the owner kind is part of the owner identifier.
fn owner_identifier(owner_kind: &str, owner_id: i64) -> String {
    format!("{owner_kind}:{owner_id}")
}

pub async fn issue(
    keys: &SigningKeys,
    credential: &str,
    audience: Option<&str>,
) -> Result<Option<String>> {
    let pool = &keys.pool;
    let hash = super::runtime::token_hash(credential);
    let Some(job) = db::oidc::job(pool, &hash).await? else {
        return Ok(None);
    };
    let run = db::ci::get_ci_run_by_run_id(pool, job.run_id)
        .await?
        .context("OIDC run missing")?;
    let repo = db::repositories::records::get_repository_by_id(pool, run.repo_id)
        .await?
        .context("OIDC repository missing")?;
    let owner = db::repositories::records::repository_owner_name(pool, repo.id)
        .await?
        .context("OIDC owner missing")?;
    // A rerun keeps the original actor, as github.actor shows it to the workflow's jobs.
    let actor_id = match run.workflow_run_id.filter(|root| *root != run.id) {
        Some(root) => {
            db::ci::get_ci_run_by_id(pool, run.repo_id, root)
                .await?
                .context("OIDC root run missing")?
                .triggered_by_id
        }
        None => run.triggered_by_id,
    };
    let actor = match actor_id {
        Some(id) => db::users::accounts::get_user_by_id(pool, id).await?,
        None => None,
    }
    .context("OIDC actor missing")?;
    let repository = format!("{owner}/{}", repo.name);
    let reference = if run.event == "pull_request" {
        let pull = db::pull_requests::records::get_pull_request_by_id(
            pool,
            run.pull_request_id.context("OIDC pull request missing")?,
        )
        .await?
        .context("OIDC pull request missing")?;
        format!("refs/pull/{}/merge", pull.number)
    } else if run.branch.starts_with("refs/") && run.event != "pull_request_target" {
        run.branch.clone()
    } else {
        // pull_request_target stores the base branch's short name, which may itself start
        // with refs/.
        format!("refs/heads/{}", run.branch)
    };
    let context = if let Some(env) = &job.environment {
        format!("environment:{}", escape(env))
    } else if run.event == "pull_request" {
        "pull_request".into()
    } else {
        format!("ref:{}", escape(&reference))
    };
    let now = chrono::Utc::now().timestamp();
    let issuer = issuer();
    // The caller's workflow is a root file. Admission stores called files under synthetic paths, so
    // a called job's root comes from its credential's job identity. act runs one workflow.
    let root_path = job
        .job
        .split_once(':')
        .map(|(root, _)| root)
        .unwrap_or(job.workflow.as_str());
    let workflow_ref = format!("{repository}/{root_path}@{reference}");
    let workflow_sha = run.workflow_sha.as_ref().or(run.head_sha.as_ref());
    // A called job names the workflow it calls. Its repository, path, ref and commit come from the
    // snapshot admission stored for that file.
    let called_workflow = match run
        .workflow_blobs
        .get(job.workflow.as_str())
        .filter(|snapshot| snapshot.is_object() && job.workflow != root_path)
    {
        Some(snapshot) => {
            let source_id = snapshot["repository_id"]
                .as_i64()
                .context("OIDC called repository missing")?;
            let source = db::repositories::records::get_repository_by_id(pool, source_id)
                .await?
                .context("OIDC called repository missing")?;
            let source_owner = db::repositories::records::repository_owner_name(pool, source.id)
                .await?
                .context("OIDC called owner missing")?;
            let path = snapshot["source_path"]
                .as_str()
                .context("OIDC called workflow path missing")?;
            let called_ref = snapshot["full_ref"]
                .as_str()
                .or(snapshot["ref"].as_str())
                .unwrap_or(reference.as_str());
            Some((
                format!("{source_owner}/{}/{path}@{called_ref}", source.name),
                snapshot["sha"].as_str().map(str::to_owned),
            ))
        }
        None => None,
    };
    let mut claims = json!({
        "iss":issuer,"sub":format!("repo:{}:{context}",escape(&repository)),
        "aud":audience.map(str::to_owned).unwrap_or_else(||format!("{issuer}/{owner}")),
        "iat":now,"nbf":now,"exp":now+300,"jti":uuid::Uuid::new_v4().to_string(),
        "ref":reference,"sha":run.head_sha,"repository":repository,
        "repository_owner":owner,"repository_id":repo.id.to_string(),"repository_owner_id":owner_identifier(&repo.owner_kind, repo.owner_id()),
        "actor":actor.username,"actor_id":actor.id.to_string(),"workflow":job.workflow_name,
        "workflow_ref":workflow_ref,"workflow_sha":workflow_sha,
        "event_name":run.event,"ref_type":if reference.starts_with("refs/tags/") {"tag"} else {"branch"},
        "run_id":run.workflow_run_id.unwrap_or(run.id).to_string(),
        "run_number":run.workflow_run_number.unwrap_or(run.id).to_string(),
        "run_attempt":run.workflow_run_attempt.unwrap_or(run.attempt).to_string(),
        "runner_environment":"self-hosted","repository_visibility":if repo.is_private {"private"} else {"public"}
    });
    if let Some((reference, sha)) = called_workflow {
        claims["job_workflow_ref"] = json!(reference);
        claims["job_workflow_sha"] = json!(sha);
    }
    if let Some(environment) = job.environment {
        claims["environment"] = json!(environment);
    }
    let settings = db::oidc::token_settings(pool, repo.id).await?;
    for (name, value) in settings.properties {
        claims[&name] = json!(value);
    }
    if let Some(keys) = settings.include_claim_keys {
        let mut parts = Vec::new();
        for key in keys {
            if key == "context" {
                parts.push(context.clone());
                continue;
            }
            let claim = if key == "repo" { "repository" } else { &key };
            let Some(value) = claims[claim].as_str() else {
                // Required claims, including environment and properties, must not be silently dropped.
                return Ok(None);
            };
            parts.push(format!("{key}:{}", escape(value)));
        }
        claims["sub"] = json!(parts.join(":"));
    }
    let key = keys.key().await?;
    // Key generation can take time. Recheck without the REST credential's completion grace period.
    if db::oidc::job(pool, &hash).await?.is_none() {
        return Ok(None);
    }
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(
        &json!({"alg":"RS256","typ":"JWT","kid":key.kid}),
    )?);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let message = format!("{header}.{payload}");
    let signing_message = message.clone();
    let signature = tokio::task::spawn_blocking(move || key.signer.sign(signing_message.as_bytes())).await?;
    Ok(Some(format!(
        "{message}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )))
}
