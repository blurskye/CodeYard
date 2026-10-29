use crate::backend::db::credentials::{Actor, Credential};
use anyhow::Result;
use sqlx::{Postgres, Transaction};

/// The caller retains account/repository authority before retaining its credential.
pub(crate) async fn live(tx: &mut Transaction<'_, Postgres>, actor: &Actor, write: bool) -> Result<bool> {
    match &actor.credential {
        None => Ok(true),
        Some(Credential::Session(session)) => super::users::transitions::live_session(tx, actor.user_id, session).await,
        Some(Credential::Token { id, hash }) => {
            let expiry: Option<Option<chrono::DateTime<chrono::Utc>>> = sqlx::query_scalar("SELECT expires_at FROM personal_access_tokens WHERE id=$1 AND user_id=$2 AND token_hash=$3 AND (NOT $4 OR scope='write') FOR SHARE")
                .bind(id).bind(actor.user_id).bind(hash).bind(write).fetch_optional(&mut **tx).await?;
            let Some(expiry) = expiry else { return Ok(false); };
            Ok(sqlx::query_scalar("SELECT $1::timestamptz IS NULL OR $1>clock_timestamp()")
                .bind(expiry).fetch_one(&mut **tx).await?)
        }
    }
}

async fn installation(tx: &mut Transaction<'_, Postgres>, app: i64, installation: Option<i64>, repo: i64) -> Result<bool> {
    let live: Option<i64> = sqlx::query_scalar("SELECT i.id FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id JOIN repositories r ON r.id=$1 WHERE i.app_id=$2 AND ($3::bigint IS NULL OR i.id=$3) AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password AND ((r.owner_kind='user' AND r.owner_user_id=i.user_id) OR (r.owner_kind='org' AND r.owner_org_id=i.org_id)) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories selected WHERE selected.installation_id=i.id AND selected.repo_id=r.id)) ORDER BY i.id LIMIT 1 FOR SHARE OF i")
        .bind(repo).bind(app).bind(installation).fetch_optional(&mut **tx).await?;
    Ok(live.is_some())
}

pub(crate) async fn repository(tx: &mut Transaction<'_, Postgres>, actor: &Actor, repo: i64, permission: &str, write: bool) -> Result<bool> {
    if !live(tx, actor, write).await? { return Ok(false); }
    let Some(id) = actor.token_id() else { return Ok(true); };
    if !super::fine_grained_tokens::repository_permission(tx, id, actor.user_id, repo, permission, write).await? { return Ok(false); }
    let app: Option<(i64, i64, String, String)> = sqlx::query_as("SELECT i.app_id,i.id,g.permissions,g.repositories FROM github_app_tokens g JOIN github_app_installations i ON i.id=g.installation_id WHERE g.token_id=$1 FOR SHARE OF g,i")
        .bind(id).fetch_optional(&mut **tx).await?;
    if let Some((app, installed, permissions, repositories)) = app {
        return Ok(crate::backend::db::credentials::app_permission(&permissions, &repositories, repo, permission, write)?
            && installation(tx, app, Some(installed), repo).await?);
    }
    let job: Option<i64> = sqlx::query_scalar("SELECT g.token_id FROM actions_github_tokens g JOIN ci_runs r ON r.id=g.run_id WHERE g.token_id=$1 FOR SHARE OF g,r")
        .bind(id).fetch_optional(&mut **tx).await?;
    if job.is_some() {
        let Some(job) = super::ci_tokens::get_in_connection(&mut **tx, id).await? else { return Ok(false); };
        let permissions: crate::backend::ci::tokens::Permissions = serde_json::from_str(&job.permissions)?;
        return Ok(job.repo_id == repo && permissions.allows(&permission.replace('_', "-"), write));
    }
    let oauth: Option<(String, String)> = sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=$1 FOR SHARE")
        .bind(id).fetch_optional(&mut **tx).await?;
    if let Some((client, scopes)) = oauth {
        let active: Option<i64> = sqlx::query_scalar("SELECT a.id FROM oauth_applications a JOIN users u ON u.id=a.owner_id WHERE a.client_id=$1 AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF a")
            .bind(&client).fetch_optional(&mut **tx).await?;
        if active.is_none() && client != crate::backend::identity::oauth_apps::CLI_CLIENT { return Ok(false); }
        let app: Option<(i64, String)> = sqlx::query_as("SELECT id,permissions FROM github_apps WHERE client_id=$1 FOR SHARE")
            .bind(&client).fetch_optional(&mut **tx).await?;
        if let Some((app, permissions)) = app {
            return Ok(crate::backend::db::credentials::app_permission(&permissions, "[]", repo, permission, write)?
                && installation(tx, app, None, repo).await?);
        }
        return Ok(crate::backend::identity::oauth_apps::allows(&scopes, &crate::backend::db::credentials::oauth_repository_path(permission), write));
    }
    Ok(true)
}

pub(crate) async fn organization(tx: &mut Transaction<'_, Postgres>, actor: &Actor, org: i64, permission: &str, write: bool) -> Result<bool> {
    if !live(tx, actor, write).await? { return Ok(false); }
    let Some(id) = actor.token_id() else { return Ok(true); };
    let _: Option<i64> = sqlx::query_scalar("SELECT token_id FROM fine_grained_tokens WHERE token_id=$1 FOR SHARE")
        .bind(id).fetch_optional(&mut **tx).await?;
    let bounds: Option<crate::backend::db::fine_grained_tokens::Bounds> = sqlx::query_as(super::fine_grained_tokens::BOUNDS)
        .bind(id).fetch_optional(&mut **tx).await?;
    if let Some(bounds) = bounds {
        let permissions = serde_json::from_str(&bounds.permissions)?;
        return Ok(bounds.active() && bounds.user_id == actor.user_id && bounds.owner_org_id == Some(org)
            && crate::backend::identity::fine_grained::allows(&permissions, permission, write));
    }
    // App and workflow credentials have no supported organization administration grant.
    let derived: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=$1) OR EXISTS(SELECT 1 FROM actions_github_tokens WHERE token_id=$1) OR EXISTS(SELECT 1 FROM oauth_app_tokens t JOIN github_apps a ON a.client_id=t.client_id WHERE t.token_id=$1)")
        .bind(id).fetch_one(&mut **tx).await?;
    if derived { return Ok(false); }
    let oauth: Option<(String, String)> = sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=$1 FOR SHARE")
        .bind(id).fetch_optional(&mut **tx).await?;
    if let Some((client, scopes)) = oauth {
        let active: Option<i64> = sqlx::query_scalar("SELECT a.id FROM oauth_applications a JOIN users u ON u.id=a.owner_id WHERE a.client_id=$1 AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF a")
            .bind(&client).fetch_optional(&mut **tx).await?;
        return Ok((active.is_some() || client == crate::backend::identity::oauth_apps::CLI_CLIENT)
            && crate::backend::identity::oauth_apps::allows(&scopes, &crate::backend::db::credentials::oauth_organization_path(permission), write));
    }
    Ok(!sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM organization_token_policies WHERE org_id=$1 AND NOT classic_allowed)")
        .bind(org).fetch_one(&mut **tx).await?)
}
