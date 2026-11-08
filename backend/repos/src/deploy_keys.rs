//! Deploy keys: who may manage a repository's keys, what a key may do over Git, and the settings
//! section that lists them. A key acts only for its own repository and never signs in to the web
//! UI. Its pushes are attributed to the account that added it, and it gets no more than that
//! account's access, capped at write.
use anyhow::Context;

use anyhow::ensure;

use anyhow::Result;
use codeyard_database::models::AccessLevel;
use codeyard_database::models::DeployKey;
use codeyard_database::models::Repository;
use codeyard_database::models::User;
use codeyard_db_identity::deploy_keys::NewDeployKey;
use codeyard_identity::repository_access::can_admin;
use codeyard_types::identity::SshKeyWriteRefusal;
use codeyard_types::repositories::DeployKeyError;
use codeyard_webui::components::deploy_keys::DeployKeyPanel;
use codeyard_webui::components::deploy_keys::DeployKeyRow;
use ssh_key::HashAlg;
use tracing::warn;

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
    pool: &codeyard_database::DbPool,
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
    pool: &codeyard_database::DbPool,
    actor: &User,
    repo: &Repository,
    credential: impl Into<codeyard_database::queries::users::accounts::OwnedAccountCredential>,
) -> Result<Vec<DeployKey>, DeployKeyError> {
    require_admin(pool, actor, repo).await?;
    codeyard_db_identity::deploy_keys::list_deploy_keys(
        pool,
        actor.id,
        repo.id,
        credential,
        0,
        i64::MAX,
    )
    .await
    .map_err(refusal)
}

/// Adds a key. The public key is validated by the same parser as account keys, and its
/// fingerprint must not already belong to an account key or a deploy key.
pub async fn add_key(
    pool: &codeyard_database::DbPool,
    actor: &User,
    credential: impl Into<codeyard_database::queries::users::accounts::OwnedAccountCredential>,
    repo: &Repository,
    title: &str,
    public_key: &str,
    can_write: bool,
) -> Result<DeployKey, DeployKeyError> {
    require_admin(pool, actor, repo).await?;
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 255 || title.contains('\0') {
        return Err(DeployKeyError::InvalidTitle);
    }
    let parsed = codeyard_db_identity::users::ssh_keys::parse_public_key(public_key)?;
    let key = NewDeployKey {
        repo_id: repo.id,
        title: title.to_owned(),
        fingerprint: parsed.fingerprint(HashAlg::Sha256).to_string(),
        public_key: public_key.trim().to_owned(),
        can_write,
        created_by_id: actor.id,
    };
    codeyard_db_identity::deploy_keys::insert_deploy_key(pool, &key, credential)
        .await
        .map_err(refusal)
}

pub async fn remove_key(
    pool: &codeyard_database::DbPool,
    actor: &User,
    credential: impl Into<codeyard_database::queries::users::accounts::OwnedAccountCredential>,
    repo: &Repository,
    key_id: i64,
) -> Result<(), DeployKeyError> {
    require_admin(pool, actor, repo).await?;
    let deleted = codeyard_db_identity::deploy_keys::delete_deploy_key(
        pool, repo.id, key_id, actor.id, credential,
    )
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
    pub credential: codeyard_database::queries::users::accounts::OwnedAccountCredential,
}

/// The section for the settings page. `notice` is the query's `notice` value, which names a
/// finished action.
pub async fn settings_panel(
    pool: &codeyard_database::DbPool,
    actor: &User,
    repo: &Repository,
    place: Place<'_>,
    notice: Option<&str>,
    after: i64,
) -> Result<DeployKeyPanel, DeployKeyError> {
    require_admin(pool, actor, repo).await?;
    let mut keys = codeyard_db_identity::deploy_keys::list_deploy_keys(
        pool,
        actor.id,
        repo.id,
        place.credential.clone(),
        after,
        51,
    )
    .await
    .map_err(refusal)?;
    let more = keys.len() > 50;
    keys.truncate(50);
    let next = more.then(|| format!("{}&keys_after={}", place.base, keys.last().unwrap().id));
    Ok(DeployKeyPanel {
        base: place.base.to_owned(),
        action_base: place.action.to_owned(),
        csrf_token: place.csrf_token.to_owned(),
        repository_id: repo.id,
        next,
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
pub fn key_type(public_key: &str) -> String {
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
    pool: &codeyard_database::DbPool,
    key_id: i64,
    repo: &Repository,
    write: bool,
) -> Result<(User, AccessLevel)> {
    let key = codeyard_db_identity::deploy_keys::usable_deploy_key(pool, key_id)
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
    let creator = codeyard_db_identity::users::accounts::get_user_by_id(pool, creator_id)
        .await?
        .context("deploy key creator unavailable")?;
    ensure!(
        !creator.is_banned && !creator.must_change_password,
        "identity unavailable"
    );
    let access = if repo.owner_kind == "user" && creator.is_admin {
        AccessLevel::Write
    } else {
        codeyard_db_repos::repositories::access::get_user_repo_access(pool, creator.id, repo.id)
            .await?
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
pub async fn record_use(pool: &codeyard_database::DbPool, key_id: i64) {
    if let Err(error) = codeyard_db_identity::deploy_keys::touch_deploy_key(pool, key_id).await {
        warn!(key_id, error = %error, "could not record deploy key use");
    }
}
