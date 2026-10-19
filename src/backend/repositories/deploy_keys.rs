//! Deploy keys: who may manage a repository's keys, what a key may do over Git, and the settings
//! section that lists them. A key acts only for its own repository and never signs in to the web
//! UI. Its pushes are attributed to the account that added it, and it gets no more than that
//! account's access, capped at write.
use crate::backend::db::models::{AccessLevel, DeployKey, Repository, User};
use crate::backend::db::users::ssh_keys::{self, SshKeyWriteRefusal};
use crate::backend::db::{self, deploy_keys::NewDeployKey};
use crate::backend::repositories::access::can_admin;
use crate::webui::components::deploy_keys::{DeployKeyPanel, DeployKeyRow};
use anyhow::{ensure, Context, Result};
use ssh_key::HashAlg;
use tracing::warn;

#[derive(Debug, thiserror::Error)]
pub enum DeployKeyError {
    #[error("Repository admin access is required")]
    Denied,
    #[error("Enter a title of up to 255 characters")]
    InvalidTitle,
    #[error("Deploy key not found")]
    NotFound,
    #[error(transparent)]
    Key(#[from] SshKeyWriteRefusal),
    #[error("Deploy key storage is unavailable")]
    Unavailable(anyhow::Error),
}

/// Keeps a typed key refusal from the database layer, and turns anything else into unavailable.
fn refusal(error: anyhow::Error) -> DeployKeyError {
    if error
        .downcast_ref::<DeployKeyError>()
        .is_some_and(|error| matches!(error, DeployKeyError::Denied))
    {
        return DeployKeyError::Denied;
    }
    match error.downcast::<SshKeyWriteRefusal>() {
        Ok(refusal) => DeployKeyError::Key(refusal),
        Err(error) => DeployKeyError::Unavailable(error),
    }
}

async fn require_admin(
    pool: &db::DbPool,
    actor: &User,
    repo: &Repository,
) -> Result<(), DeployKeyError> {
    match can_admin(pool, actor, repo).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(DeployKeyError::Denied),
        Err(error) => Err(DeployKeyError::Unavailable(error)),
    }
}

pub async fn list_keys(
    pool: &db::DbPool,
    actor: &User,
    repo: &Repository,
) -> Result<Vec<DeployKey>, DeployKeyError> {
    require_admin(pool, actor, repo).await?;
    db::deploy_keys::list_deploy_keys(pool, repo.id)
        .await
        .map_err(DeployKeyError::Unavailable)
}

/// Adds a key. The public key is validated by the same parser as account keys, and its
/// fingerprint must not already belong to an account key or a deploy key.
pub async fn add_key(
    pool: &db::DbPool,
    actor: &User,
    session: &str,
    repo: &Repository,
    title: &str,
    public_key: &str,
    can_write: bool,
) -> Result<DeployKey, DeployKeyError> {
    require_admin(pool, actor, repo).await?;
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 255 {
        return Err(DeployKeyError::InvalidTitle);
    }
    let parsed = ssh_keys::parse_public_key(public_key)?;
    let key = NewDeployKey {
        repo_id: repo.id,
        title: title.to_owned(),
        fingerprint: parsed.fingerprint(HashAlg::Sha256).to_string(),
        public_key: public_key.trim().to_owned(),
        can_write,
        created_by_id: actor.id,
    };
    db::deploy_keys::insert_deploy_key(pool, &key, session)
        .await
        .map_err(refusal)
}

pub async fn remove_key(
    pool: &db::DbPool,
    actor: &User,
    session: &str,
    repo: &Repository,
    key_id: i64,
) -> Result<(), DeployKeyError> {
    require_admin(pool, actor, repo).await?;
    let deleted = db::deploy_keys::delete_deploy_key(pool, repo.id, key_id, actor.id, session)
        .await
        .map_err(refusal)?;
    if deleted {
        Ok(())
    } else {
        Err(DeployKeyError::NotFound)
    }
}

/// Where the settings section's forms point, and the viewer's CSRF token.
pub struct Place<'a> {
    /// The section's URL with its query started, e.g. `/acme/app/settings?tab=deploy-keys`.
    pub base: &'a str,
    /// The repository form endpoint, `/owner/repo/_forms`.
    pub action: &'a str,
    pub csrf_token: &'a str,
}

/// The section for the settings page. `notice` is the query's `notice` value, which names a
/// finished action.
pub async fn settings_panel(
    pool: &db::DbPool,
    actor: &User,
    repo: &Repository,
    place: Place<'_>,
    notice: Option<&str>,
) -> Result<DeployKeyPanel, DeployKeyError> {
    let keys = list_keys(pool, actor, repo).await?;
    Ok(DeployKeyPanel {
        base: place.base.to_owned(),
        action_base: place.action.to_owned(),
        csrf_token: place.csrf_token.to_owned(),
        repository_id: repo.id,
        keys: keys.into_iter().map(row).collect(),
        notice: notice.and_then(notice_text).map(str::to_owned),
        ..DeployKeyPanel::default()
    })
}

fn notice_text(notice: &str) -> Option<&'static str> {
    Some(match notice {
        "added" => "Deploy key added.",
        "deleted" => "Deploy key deleted.",
        _ => return None,
    })
}

/// The key type is the first field of an OpenSSH public key, which the insert has validated.
pub(crate) fn key_type(public_key: &str) -> String {
    public_key
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn row(key: DeployKey) -> DeployKeyRow {
    DeployKeyRow {
        id: key.id,
        title: key.title,
        fingerprint: key.fingerprint,
        key_type: key_type(&key.public_key),
        can_write: key.can_write,
        added: key.created_at.to_rfc3339(),
        last_used: key.last_used_at.map(|used| used.to_rfc3339()),
    }
}

/// Checks one Git session of a deploy key against `repo`. The key must belong to `repo`, and a
/// write needs a write key. It returns the account the session is attributed to, and the access
/// that account grants, capped at write so a key is never an admin.
pub async fn authorize(
    pool: &db::DbPool,
    key_id: i64,
    repo: &Repository,
    write: bool,
) -> Result<(User, AccessLevel)> {
    let key = db::deploy_keys::get_deploy_key(pool, key_id)
        .await?
        .context("deploy key unavailable")?;
    ensure!(
        key.repo_id == repo.id,
        "deploy key belongs to another repository"
    );
    ensure!(!write || key.can_write, "deploy key is read-only");
    ensure!(!write || !repo.is_archived, "repository archived");
    let creator_id = key
        .created_by_id
        .context("deploy key creator unavailable")?;
    let creator = db::users::accounts::get_user_by_id(pool, creator_id)
        .await?
        .context("deploy key creator unavailable")?;
    ensure!(
        !creator.is_banned && !creator.must_change_password,
        "identity unavailable"
    );
    let access = if repo.owner_kind == "user" && creator.is_admin {
        AccessLevel::Write
    } else {
        db::repositories::access::get_user_repo_access(pool, creator.id, repo.id).await?
    };
    let required = if write {
        AccessLevel::Write
    } else {
        AccessLevel::Read
    };
    ensure!(access >= required, "repository access denied");
    Ok((creator, access.min(AccessLevel::Write)))
}

/// Records that a key authenticated. A failure is logged and does not refuse the session.
pub async fn record_use(pool: &db::DbPool, key_id: i64) {
    if let Err(error) = db::deploy_keys::touch_deploy_key(pool, key_id).await {
        warn!(key_id, error = %error, "could not record deploy key use");
    }
}
