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
    pub payload_url: String,
    pub content_type: String,
    pub events: Vec<String>,
    pub active: bool,
    pub ssl_verify: bool,
    /// Empty keeps the stored secret, or stores none.
    pub secret: String,
}

fn unavailable<E>(_: E) -> HookError {
    HookError::Unavailable
}

/// Repository admins manage a repository's hooks. Organization owners manage an
/// organization's. Site administrators have no implicit access to either.
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

fn validated(form: &HookForm) -> Result<WebhookFields, HookError> {
    let payload_url = form.payload_url.trim();
    outbound::validate_url(payload_url, outbound::allow_private()).map_err(
        |refusal| match refusal {
            outbound::Refusal::Invalid(text) => HookError::Invalid(text),
            _ => HookError::Invalid("Payload URL is not allowed"),
        },
    )?;
    let content_type = match form.content_type.as_str() {
        JSON_CONTENT_TYPE | FORM_CONTENT_TYPE => form.content_type.clone(),
        _ => return Err(HookError::Invalid("Choose a content type from the list")),
    };
    if form.secret.len() > MAX_SECRET_BYTES || form.secret.contains('\0') {
        return Err(HookError::Invalid("The secret is too long or contains NUL"));
    }
    Ok(WebhookFields {
        payload_url: payload_url.to_owned(),
        content_type,
        events: events::subscription(&form.events).map_err(HookError::Invalid)?,
        active: form.active,
        ssl_verify: form.ssl_verify,
    })
}

fn seal(
    encryptor: Option<&SecretEncryptor>,
    secret: &str,
) -> Result<Option<SealedSecret>, HookError> {
    if secret.is_empty() {
        return Ok(None);
    }
    let encryptor = encryptor.ok_or(HookError::Unavailable)?;
    let (ciphertext, nonce) = encryptor.encrypt(secret.as_bytes()).map_err(unavailable)?;
    Ok(Some(SealedSecret {
        ciphertext,
        nonce: nonce.to_vec(),
    }))
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
    let fields = validated(form)?;
    let secret = seal(encryptor, &form.secret)?;
    match id {
        None => {
            let id = hooks::insert_webhook(pool, owner, actor.id, &fields, secret.as_ref())
                .await
                .map_err(unavailable)?
                .ok_or(HookError::Denied)?;
            // A hook that was saved stays saved when its ping cannot be queued.
            if ping(pool, owner, actor, id).await.is_err() {
                tracing::warn!(webhook_id = id, "could not queue the webhook ping");
            }
            Ok(id)
        }
        Some(id) => {
            let updated =
                hooks::update_webhook(pool, owner, actor.id, id, &fields, secret.as_ref())
                    .await
                    .map_err(unavailable)?
                    .ok_or(HookError::Denied)?;
            if !updated {
                return Err(HookError::NotFound);
            }
            Ok(id)
        }
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
    let outgoing = outgoing(events::PING, None, &hook, &json);
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
    if enqueue(pool, hook, super::delivery::redelivery(&recorded)) {
        Ok(())
    } else {
        Err(HookError::Unavailable)
    }
}

async fn ping_payload(
    pool: &DbPool,
    owner: HookOwner,
    actor: &User,
    hook: &Webhook,
) -> Result<Value, HookError> {
    let kind = match owner {
        HookOwner::Repository(_) => "Repository",
        HookOwner::Organization(_) => "Organization",
    };
    let subscribed: Vec<&str> = if hook.events == events::EVERYTHING {
        vec![events::EVERYTHING]
    } else {
        hook.events.split(',').collect()
    };
    let mut payload = json!({
        "zen": ZEN,
        "hook_id": hook.id,
        "hook": {
            "type": kind,
            "id": hook.id,
            "active": hook.active,
            "events": subscribed,
            "config": {
                "url": hook.payload_url,
                "content_type": hook.content_type,
                "insecure_ssl": if hook.ssl_verify { "0" } else { "1" },
            },
            "created_at": hook.created_at.to_rfc3339(),
            "updated_at": hook.updated_at.to_rfc3339(),
        },
        "sender": payloads::person(&payloads::Person::of(actor)),
    });
    match owner {
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
        }
        HookOwner::Organization(id) => {
            payload["organization"] = json!({ "id": id });
        }
    }
    Ok(payload)
}
