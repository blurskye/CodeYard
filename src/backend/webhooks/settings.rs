//! Managing webhooks: who may, what a form may save, and the ping, test and redelivery
//! actions. Every action checks the actor before it reads or changes a hook.
use super::{enqueue, events, outbound, outgoing, payloads};
use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::models::{OrgRole, Repository, User, Webhook};
use crate::backend::db::organizations::OrganizationOutcome;
use crate::backend::db::webhooks::{self as hooks, HookOwner, SealedSecret, WebhookFields};
use crate::backend::db::{self, DbPool};
use crate::backend::repositories::access;
use serde_json::{json, Value};

pub const JSON_CONTENT_TYPE: &str = "application/json";
pub const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
const MAX_SECRET_BYTES: usize = 1024;
const ZEN: &str = "Anything added dilutes everything else.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookError {
    Denied,
    NotFound,
    /// The input is wrong. The text is safe to show.
    Invalid(&'static str),
    Unavailable,
}

/// What the webhook form posts.
#[derive(Debug, Clone)]
pub struct HookForm {
    pub username: Option<String>,
    pub password: Option<String>,
    pub clear_password: bool,
    pub payload_url: String,
    pub content_type: String,
    pub events: Vec<String>,
    pub active: bool,
    pub ssl_verify: bool,
    /// Empty keeps the stored secret, or stores none.
    pub secret: String,
    pub clear_secret: bool,
}

#[derive(Default)]
pub struct HookPatch {
    pub username: Option<String>,
    pub password: Option<String>,
    pub payload_url: Option<String>,
    pub content_type: Option<String>,
    pub events: Option<Vec<String>>,
    pub active: Option<bool>,
    pub ssl_verify: Option<bool>,
    pub secret: Option<String>,
}

fn unavailable<E>(_: E) -> HookError {
    HookError::Unavailable
}

/// Maps a refused hook write to what the form or API shows.
fn write_error(owner: HookOwner, error: anyhow::Error) -> HookError {
    if error.is::<hooks::HookLimit>() {
        HookError::Invalid(
            "This owner's webhook limit has been reached. Remove an existing hook first.",
        )
    } else if error.is::<hooks::HookExists>() {
        HookError::Invalid(match owner {
            HookOwner::Repository(_) => "Hook already exists on this repository",
            HookOwner::Organization(_) => "Hook already exists on this organization",
        })
    } else {
        HookError::Unavailable
    }
}

/// Repository admins manage a repository's hooks, and so do site administrators on user-owned
/// repositories. Only organization owners manage an organization's.
pub async fn authorize(pool: &DbPool, actor: &User, owner: HookOwner) -> Result<(), HookError> {
    let allowed = match owner {
        HookOwner::Repository(id) => {
            let repo = db::repositories::records::get_repository_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or(HookError::NotFound)?;
            access::can_admin(pool, actor, &repo)
                .await
                .map_err(unavailable)?
        }
        HookOwner::Organization(org) => matches!(
            db::organizations::membership::get_organization_role(pool, actor.id, org)
                .await
                .map_err(unavailable)?,
            OrganizationOutcome::Applied(OrgRole::Owner)
        ),
    };
    if allowed {
        Ok(())
    } else {
        Err(HookError::Denied)
    }
}

fn validate_url(text: &str) -> Result<String, HookError> {
    let payload_url = text.trim();
    outbound::validate_url(payload_url, outbound::allow_private()).map_err(
        |refusal| match refusal {
            outbound::Refusal::Invalid(text) => HookError::Invalid(text),
            _ => HookError::Invalid("Payload URL is not allowed"),
        },
    )?;
    Ok(payload_url.to_owned())
}

fn validate_content_type(text: &str) -> Result<String, HookError> {
    match text {
        JSON_CONTENT_TYPE | FORM_CONTENT_TYPE => Ok(text.to_owned()),
        _ => Err(HookError::Invalid("Choose a content type from the list")),
    }
}

fn validate_username(name: &str) -> Result<String, HookError> {
    if name.len() > 1024 || name.chars().any(|ch| ch == ':' || ch.is_control()) {
        return Err(HookError::Invalid("The HTTP username is invalid"));
    }
    Ok(name.to_owned())
}

pub(crate) fn validated(form: &HookForm) -> Result<WebhookFields, HookError> {
    Ok(WebhookFields {
        username: form
            .username
            .as_deref()
            .map(validate_username)
            .transpose()?,
        password: None,
        payload_url: validate_url(&form.payload_url)?,
        content_type: validate_content_type(&form.content_type)?,
        events: events::subscription(&form.events).map_err(HookError::Invalid)?,
        active: form.active,
        ssl_verify: form.ssl_verify,
    })
}

pub(crate) fn seal(
    encryptor: Option<&SecretEncryptor>,
    secret: &str,
) -> Result<Option<SealedSecret>, HookError> {
    if secret.is_empty() {
        return Ok(None);
    }
    sealed(encryptor, secret).map(Some)
}

fn sealed(encryptor: Option<&SecretEncryptor>, secret: &str) -> Result<SealedSecret, HookError> {
    if secret.len() > MAX_SECRET_BYTES || secret.contains('\0') {
        return Err(HookError::Invalid("The secret is too long or contains NUL"));
    }
    let encryptor = encryptor.ok_or(HookError::Unavailable)?;
    let (ciphertext, nonce) = encryptor.encrypt(secret.as_bytes()).map_err(unavailable)?;
    Ok(SealedSecret {
        ciphertext,
        nonce: nonce.to_vec(),
    })
}

/// Creates or updates a hook. A new hook gets a ping. Returns the hook's id.
pub async fn save(
    pool: &DbPool,
    encryptor: Option<&SecretEncryptor>,
    owner: HookOwner,
    actor: &User,
    id: Option<i64>,
    form: &HookForm,
) -> Result<i64, HookError> {
    authorize(pool, actor, owner).await?;
    let mut fields = validated(form)?;
    fields.password = form
        .password
        .as_deref()
        .map(|password| sealed(encryptor, password))
        .transpose()?;
    if id.is_none() && fields.password.is_some() && fields.username.is_none() {
        fields.username = Some(String::new());
    }
    if form.clear_password && form.password.is_some() {
        return Err(HookError::Invalid(
            "Choose a new HTTP password or remove the saved credentials",
        ));
    }
    let secret = seal(encryptor, &form.secret)?;
    match id {
        None => {
            let id = hooks::insert_webhook(pool, owner, actor.id, &fields, secret.as_ref())
                .await
                .map_err(|error| write_error(owner, error))?
                .ok_or(HookError::Denied)?;
            // A hook that was saved stays saved when its ping cannot be queued.
            if ping(pool, owner, actor, id).await.is_err() {
                tracing::warn!(webhook_id = id, "could not queue the webhook ping");
            }
            Ok(id)
        }
        Some(id) => {
            if form.clear_secret && !form.secret.is_empty() {
                return Err(HookError::Invalid(
                    "Choose a new secret or remove the saved secret",
                ));
            }
            let mut patch = hooks::WebhookPatch::from(&fields);
            patch.clear_secret = form.clear_secret;
            patch.clear_password = form.clear_password;
            let updated = hooks::patch_webhook(pool, owner, actor.id, id, &patch, secret.as_ref())
                .await
                .map_err(|error| write_error(owner, error))?
                .ok_or(HookError::Denied)?;
            if !updated {
                return Err(HookError::NotFound);
            }
            Ok(id)
        }
    }
}

pub async fn update(
    pool: &DbPool,
    encryptor: Option<&SecretEncryptor>,
    owner: HookOwner,
    actor: &User,
    id: i64,
    patch: &HookPatch,
) -> Result<(), HookError> {
    authorize(pool, actor, owner).await?;
    let fields = hooks::WebhookPatch {
        username: patch
            .username
            .as_deref()
            .map(validate_username)
            .transpose()?,
        password: patch
            .password
            .as_deref()
            .map(|password| sealed(encryptor, password))
            .transpose()?,
        clear_password: false,
        payload_url: patch.payload_url.as_deref().map(validate_url).transpose()?,
        content_type: patch
            .content_type
            .as_deref()
            .map(validate_content_type)
            .transpose()?,
        events: patch
            .events
            .as_ref()
            .map(|events| events::subscription(events).map_err(HookError::Invalid))
            .transpose()?,
        active: patch.active,
        ssl_verify: patch.ssl_verify,
        clear_secret: patch.secret.as_ref().is_some_and(String::is_empty),
    };
    let secret = patch
        .secret
        .as_deref()
        .map(|secret| seal(encryptor, secret))
        .transpose()?
        .flatten();
    match hooks::patch_webhook(pool, owner, actor.id, id, &fields, secret.as_ref())
        .await
        .map_err(|error| write_error(owner, error))?
    {
        None => Err(HookError::Denied),
        Some(false) => Err(HookError::NotFound),
        Some(true) => Ok(()),
    }
}

pub async fn delete(
    pool: &DbPool,
    owner: HookOwner,
    actor: &User,
    id: i64,
) -> Result<(), HookError> {
    authorize(pool, actor, owner).await?;
    let deleted = hooks::delete_webhook(pool, owner, actor.id, id)
        .await
        .map_err(unavailable)?
        .ok_or(HookError::Denied)?;
    if deleted {
        Ok(())
    } else {
        Err(HookError::NotFound)
    }
}

/// Queues a ping to one hook, as the Test button does.
pub async fn ping(pool: &DbPool, owner: HookOwner, actor: &User, id: i64) -> Result<(), HookError> {
    authorize(pool, actor, owner).await?;
    let hook = hooks::get_webhook(pool, owner, id)
        .await
        .map_err(unavailable)?
        .ok_or(HookError::NotFound)?;
    let payload = ping_payload(pool, owner, actor, &hook).await?;
    let json = payload.to_string();
    let mut outgoing = outgoing(events::PING, None, &hook, &json);
    outgoing.manual_actor = Some(actor.id);
    if enqueue(pool, hook, outgoing) {
        Ok(())
    } else {
        Err(HookError::Unavailable)
    }
}

/// Queues a new send of a recorded delivery to the hook's current URL.
pub async fn redeliver(
    pool: &DbPool,
    owner: HookOwner,
    actor: &User,
    hook_id: i64,
    delivery_id: i64,
) -> Result<(), HookError> {
    authorize(pool, actor, owner).await?;
    let hook = hooks::get_webhook(pool, owner, hook_id)
        .await
        .map_err(unavailable)?
        .ok_or(HookError::NotFound)?;
    let recorded = hooks::get_delivery(pool, hook_id, delivery_id)
        .await
        .map_err(unavailable)?
        .ok_or(HookError::NotFound)?;
    let mut outgoing = super::delivery::redelivery(&recorded);
    outgoing.manual_actor = Some(actor.id);
    if enqueue(pool, hook, outgoing) {
        Ok(())
    } else {
        Err(HookError::Unavailable)
    }
}

/// Replays the latest persisted push, without reading newer Git state or moving a reference.
pub async fn test_push(
    pool: &DbPool,
    owner: HookOwner,
    actor: &User,
    id: i64,
) -> Result<(), HookError> {
    authorize(pool, actor, owner).await?;
    let hook = hooks::get_webhook(pool, owner, id)
        .await
        .map_err(unavailable)?
        .ok_or(HookError::NotFound)?;
    if !events::subscribes(&hook.events, events::PUSH) { return Ok(()); }
    let HookOwner::Repository(repo) = owner else {
        return Err(HookError::Invalid("A repository webhook is required"));
    };
    let body = hooks::latest_push_for_hook(pool, actor.id, repo, id, super::MAX_PAYLOAD_BYTES as i64)
        .await.map_err(|error| if error.is::<hooks::HookAuthorityDenied>() { HookError::Denied } else { unavailable(error) })?;
    let Some(body) = body else { return Ok(()); };
    let payload: Value = serde_json::from_str(&body).map_err(unavailable)?;
    if payload["repository"]["id"].as_i64() != Some(repo) { return Err(HookError::Unavailable); }
    let mut send = outgoing(events::PUSH, None, &hook, &body);
    send.automatic = false;
    send.manual_actor = Some(actor.id);
    if enqueue(pool, hook, send) { Ok(()) } else { Err(HookError::Unavailable) }
}

async fn ping_payload(
    pool: &DbPool,
    owner: HookOwner,
    actor: &User,
    hook: &Webhook,
) -> Result<Value, HookError> {
    let mut payload = json!({"zen": ZEN, "hook_id": hook.id,
        "sender": payloads::person(&payloads::Person::of(actor))});
    let url = match owner {
        HookOwner::Repository(id) => {
            let repo: Repository = db::repositories::records::get_repository_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or(HookError::NotFound)?;
            let owner_name = db::repositories::records::repository_owner_names(pool, &[id])
                .await
                .map_err(unavailable)?
                .remove(&id)
                .unwrap_or_default();
            payload["repository"] = payloads::repository(&repo, &owner_name);
            super::set_open_issues(pool, id, &mut payload).await;
            format!(
                "{}/hooks/{}",
                crate::backend::api::github::repo_url(&owner_name, &repo.name),
                hook.id
            )
        }
        HookOwner::Organization(id) => {
            let org = db::organizations::get_public_organization_by_id(pool, id)
                .await
                .map_err(unavailable)?
                .ok_or(HookError::NotFound)?;
            payload["organization"] = payloads::organization(&org);
            crate::backend::api::github::api_url(&format!(
                "/orgs/{}/hooks/{}",
                crate::webui::encode_component(&org.name),
                hook.id
            ))
        }
    };
    payload["hook"] = payloads::webhook(hook, &url);
    Ok(payload)
}
