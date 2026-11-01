use crate::backend::db::ci::CiCredential;
use crate::backend::db::credentials::Actor;
use crate::backend::db::users::accounts::{AccountRestricted, OwnedAccountCredential};
use anyhow::Result;
use sqlx::{Sqlite, Transaction};

pub(crate) async fn live(
    tx: &mut sqlx::SqliteConnection,
    actor: &Actor,
    write: bool,
) -> Result<bool> {
    match actor.credential() {
        None => Ok(true),
        Some(credential) => {
            super::users::transitions::credential_live(tx, actor.user_id, &credential, write).await
        }
    }
}

/// The caller's own session or token is still live and, for a token, may still write
/// `permission` on the repository or organization. Call it in the transaction that writes.
pub(crate) async fn writes(
    tx: &mut sqlx::SqliteConnection,
    actor: &Actor,
    repo: Option<i64>,
    org: Option<i64>,
    permission: &str,
) -> Result<bool> {
    if !live(tx, actor, true).await? {
        return Ok(false);
    }
    match actor.token_id() {
        Some(token) => {
            super::users::tokens::permits_write(tx, actor.user_id, token, repo, org, permission)
                .await
        }
        None => Ok(true),
    }
}

/// A live, unrestricted account and, unless this is a trusted in-process call, its live credential.
pub(crate) async fn require_account(tx: &mut Transaction<'_, Sqlite>, actor: &Actor) -> Result<()> {
    match actor.credential() {
        Some(credential) => {
            super::users::transitions::require_account(tx, actor.user_id, &credential).await
        }
        None => {
            anyhow::ensure!(
                super::users::transitions::active_account(tx, actor.user_id).await?,
                AccountRestricted
            );
            Ok(())
        }
    }
}

async fn installation(
    tx: &mut Transaction<'_, Sqlite>,
    app: i64,
    installation: Option<i64>,
    repo: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN repositories r ON r.id=?1 WHERE i.app_id=?2 AND (?3 IS NULL OR i.id=?3) AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND ((r.owner_kind='user' AND r.owner_user_id=i.user_id) OR (r.owner_kind='org' AND r.owner_org_id=i.org_id)) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories selected WHERE selected.installation_id=i.id AND selected.repo_id=r.id)))")
        .bind(repo).bind(app).bind(installation).fetch_one(&mut **tx).await?)
}

pub(crate) async fn repository(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &Actor,
    repo: i64,
    permission: &str,
    write: bool,
) -> Result<bool> {
    if !live(tx, actor, write).await? {
        return Ok(false);
    }
    let Some(id) = actor.token_id() else {
        return Ok(true);
    };
    if !super::fine_grained_tokens::repository_permission(
        tx,
        id,
        actor.user_id,
        repo,
        permission,
        write,
    )
    .await?
    {
        return Ok(false);
    }
    let app: Option<(i64, i64, String, String)> = sqlx::query_as("SELECT i.app_id,i.id,g.permissions,g.repositories FROM github_app_tokens g JOIN github_app_installations i ON i.id=g.installation_id WHERE g.token_id=?1")
        .bind(id).fetch_optional(&mut **tx).await?;
    if let Some((app, installed, permissions, repositories)) = app {
        return Ok(crate::backend::db::credentials::app_permission(
            &permissions,
            &repositories,
            repo,
            permission,
            write,
        )? && installation(tx, app, Some(installed), repo).await?);
    }
    let job: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_github_tokens WHERE token_id=?1)")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    if job {
        let Some(job) = super::ci_tokens::get_in_connection(tx, id).await? else {
            return Ok(false);
        };
        let permissions: crate::backend::ci::tokens::Permissions =
            serde_json::from_str(&job.permissions)?;
        return Ok(job.repo_id == repo && permissions.allows(&permission.replace('_', "-"), write));
    }
    let oauth: Option<(String, String)> =
        sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=?1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some((client, scopes)) = oauth {
        let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_applications a JOIN users u ON u.id=a.owner_id WHERE a.client_id=?1 AND NOT u.is_banned AND NOT u.must_change_password)")
            .bind(&client).fetch_one(&mut **tx).await?;
        if !active && client != crate::backend::identity::oauth_apps::CLI_CLIENT {
            return Ok(false);
        }
        let app: Option<(i64, String)> =
            sqlx::query_as("SELECT id,permissions FROM github_apps WHERE client_id=?1")
                .bind(&client)
                .fetch_optional(&mut **tx)
                .await?;
        if let Some((app, permissions)) = app {
            return Ok(crate::backend::db::credentials::app_permission(
                &permissions,
                "[]",
                repo,
                permission,
                write,
            )? && installation(tx, app, None, repo).await?);
        }
        return Ok(crate::backend::identity::oauth_apps::allows(
            &scopes,
            &crate::backend::db::credentials::oauth_repository_path(permission),
            write,
        ));
    }
    Ok(true)
}

/// The credential behind a CI request, checked in the transaction that makes the effect.
/// A job token acts for the user who triggered its running run in this repository; any
/// other credential must belong to `actor`. Repository access is the caller's check.
pub(crate) async fn ci(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    repo: i64,
    proof: &CiCredential,
) -> Result<bool> {
    let (owner, credential, permission) = match proof {
        CiCredential::Session(session) => (
            actor,
            OwnedAccountCredential::Session(session.clone()),
            "actions",
        ),
        CiCredential::Token {
            id,
            hash,
            permission,
        } => {
            let token: Option<(i64, bool)> = sqlx::query_as("SELECT t.user_id,u.username=$2 FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1")
                .bind(id).bind(crate::backend::db::ci_tokens::BOT).fetch_optional(&mut **tx).await?;
            let Some((owner, bot)) = token else {
                return Ok(false);
            };
            let job: Option<bool> = sqlx::query_scalar("SELECT r.repo_id=$2 AND r.triggered_by_id=$3 AND r.status='running' AND NOT r.cancel_requested AND g.revoked_at IS NULL FROM actions_github_tokens g JOIN ci_runs r ON r.id=g.run_id WHERE g.token_id=$1")
                .bind(id).bind(repo).bind(actor).fetch_optional(&mut **tx).await?;
            // Only the workflow bot holds job tokens, and it holds no other kind.
            if bot != job.is_some() || job == Some(false) {
                return Ok(false);
            }
            (
                if bot { owner } else { actor },
                OwnedAccountCredential::Token(*id, hash.clone()),
                *permission,
            )
        }
    };
    let actor = Actor {
        user_id: owner,
        credential: Some(credential),
    };
    match require_account(tx, &actor).await {
        Ok(()) => {}
        Err(error) if error.is::<AccountRestricted>() => return Ok(false),
        Err(error) => return Err(error),
    }
    repository(tx, &actor, repo, permission, true).await
}

pub(crate) async fn organization(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &Actor,
    org: i64,
    permission: &str,
    write: bool,
) -> Result<bool> {
    if !live(tx, actor, write).await? {
        return Ok(false);
    }
    let Some(id) = actor.token_id() else {
        return Ok(true);
    };
    let bounds: Option<crate::backend::db::fine_grained_tokens::Bounds> =
        sqlx::query_as(super::fine_grained_tokens::BOUNDS)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(bounds) = bounds {
        let permissions = serde_json::from_str(&bounds.permissions)?;
        return Ok(bounds.active()
            && bounds.user_id == actor.user_id
            && bounds.owner_org_id == Some(org)
            && crate::backend::identity::fine_grained::allows(&permissions, permission, write));
    }
    let derived: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=?1) OR EXISTS(SELECT 1 FROM actions_github_tokens WHERE token_id=?1) OR EXISTS(SELECT 1 FROM oauth_app_tokens t JOIN github_apps a ON a.client_id=t.client_id WHERE t.token_id=?1)")
        .bind(id).fetch_one(&mut **tx).await?;
    if derived {
        return Ok(false);
    }
    let oauth: Option<(String, String)> =
        sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=?1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some((client, scopes)) = oauth {
        let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_applications a JOIN users u ON u.id=a.owner_id WHERE a.client_id=?1 AND NOT u.is_banned AND NOT u.must_change_password)")
            .bind(&client).fetch_one(&mut **tx).await?;
        return Ok(
            (active || client == crate::backend::identity::oauth_apps::CLI_CLIENT)
                && crate::backend::identity::oauth_apps::allows(
                    &scopes,
                    &crate::backend::db::credentials::oauth_organization_path(permission),
                    write,
                ),
        );
    }
    Ok(!sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM organization_token_policies WHERE org_id=?1 AND NOT classic_allowed)")
        .bind(org).fetch_one(&mut **tx).await?)
}
