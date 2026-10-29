//! Request credentials are checked after repository authority, inside the effect transaction.
use crate::backend::db::ci::CiCredential;
use anyhow::Result;
use sqlx::{Postgres, Transaction};

pub(super) async fn live(tx: &mut Transaction<'_, Postgres>, actor: i64, repo: i64, proof: &CiCredential) -> Result<bool> {
    let (id, hash, permission) = match proof {
        CiCredential::Session(session) => return super::users::transitions::live_session(tx, actor, session).await,
        CiCredential::Token { id, hash, permission } => (*id, hash, *permission),
    };
    // A job's run precedes its credential rows, as in completion and revocation.
    let source: Option<i64> = sqlx::query_scalar("SELECT run_id FROM actions_github_tokens WHERE token_id=$1")
        .bind(id).fetch_optional(&mut **tx).await?;
    if let Some(source) = source {
        let active: Option<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE id=$1 AND repo_id=$2 AND triggered_by_id=$3 AND status='running' AND NOT cancel_requested FOR SHARE")
            .bind(source).bind(repo).bind(actor).fetch_optional(&mut **tx).await?;
        if active.is_none() { return Ok(false); }
    }
    let owner: Option<(i64, String)> = sqlx::query_as("SELECT t.user_id,u.username FROM personal_access_tokens t JOIN users u ON u.id=t.user_id WHERE t.id=$1 AND t.token_hash=$2 AND t.scope='write' AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF t")
        .bind(id).bind(hash).fetch_optional(&mut **tx).await?;
    let Some((owner, username)) = owner else { return Ok(false); };
    if let Some(source) = source {
        if username != crate::backend::db::ci_tokens::BOT { return Ok(false); }
        let grants: Option<String> = sqlx::query_scalar("SELECT permissions FROM actions_github_tokens WHERE token_id=$1 AND run_id=$2 AND revoked_at IS NULL FOR SHARE")
            .bind(id).bind(source).fetch_optional(&mut **tx).await?;
        let Some(grants) = grants else { return Ok(false); };
        return Ok(serde_json::from_str::<crate::backend::ci::tokens::Permissions>(&grants)?.allows(permission, true));
    }
    if owner != actor || username == crate::backend::db::ci_tokens::BOT { return Ok(false); }
    let fine: Option<i64> = sqlx::query_scalar("SELECT token_id FROM fine_grained_tokens WHERE token_id=$1 FOR SHARE")
        .bind(id).fetch_optional(&mut **tx).await?;
    if fine.is_some() {
        let bounds: crate::backend::db::fine_grained_tokens::Bounds = sqlx::query_as(super::fine_grained_tokens::BOUNDS)
            .bind(id).fetch_one(&mut **tx).await?;
        let permissions = serde_json::from_str(&bounds.permissions)?;
        let selected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$1 AND ((r.owner_kind='user' AND r.owner_user_id=$2 AND $3::bigint IS NULL) OR (r.owner_kind='org' AND r.owner_org_id=$3)) AND ($4='all' OR EXISTS(SELECT 1 FROM fine_grained_token_repositories f WHERE f.token_id=$5 AND f.repo_id=r.id)))")
            .bind(repo).bind(owner).bind(bounds.owner_org_id).bind(&bounds.repository_access).bind(id).fetch_one(&mut **tx).await?;
        return Ok(bounds.active() && selected && crate::backend::identity::fine_grained::allows(&permissions, permission, true));
    }
    let app: Option<crate::backend::db::github_apps::Token> = sqlx::query_as("SELECT g.* FROM github_app_tokens g JOIN github_app_installations i ON i.id=g.installation_id JOIN github_apps a ON a.id=i.app_id JOIN users owner ON owner.id=a.owner_id JOIN repositories r ON r.id=$2 WHERE g.token_id=$1 AND i.suspended_at IS NULL AND NOT owner.is_banned AND NOT owner.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) FOR SHARE OF g,i,a,owner")
        .bind(id).bind(repo).fetch_optional(&mut **tx).await?;
    if let Some(app) = app {
        let repositories: Vec<i64> = serde_json::from_str(&app.repositories)?;
        let grants = serde_json::from_str(&app.permissions)?;
        return Ok((repositories.is_empty() || repositories.contains(&repo)) && crate::backend::identity::github_apps::allows(&grants, permission, true));
    }
    // An orphaned integration token must never turn into an unscoped personal token.
    let integration: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=$1)")
        .bind(id).fetch_one(&mut **tx).await?;
    if integration { return Ok(false); }
    let oauth: Option<(String, String)> = sqlx::query_as("SELECT client_id,scopes FROM oauth_app_tokens WHERE token_id=$1 FOR SHARE")
        .bind(id).fetch_optional(&mut **tx).await?;
    if let Some((client, scopes)) = oauth {
        let app_grants: Option<String> = sqlx::query_scalar("SELECT a.permissions FROM github_apps a JOIN users owner ON owner.id=a.owner_id WHERE a.client_id=$1 AND NOT owner.is_banned AND NOT owner.must_change_password AND EXISTS(SELECT 1 FROM github_app_installations i JOIN repositories r ON r.id=$2 WHERE i.app_id=a.id AND i.suspended_at IS NULL AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id))) FOR SHARE OF a,owner")
            .bind(&client).bind(repo).fetch_optional(&mut **tx).await?;
        if let Some(grants) = app_grants {
            return Ok(crate::backend::identity::github_apps::allows(&serde_json::from_str(&grants)?, permission, true));
        }
        let application: Option<bool> = sqlx::query_scalar("SELECT is_github_app FROM oauth_applications WHERE client_id=$1 FOR SHARE")
            .bind(&client).fetch_optional(&mut **tx).await?;
        return Ok((application == Some(false) || client == crate::backend::identity::oauth_apps::CLI_CLIENT)
            && crate::backend::identity::oauth_apps::allows(&scopes, "/repos/owner/repo/actions/workflows/workflow/dispatches", true));
    }
    Ok(sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM repositories r JOIN organization_token_policies p ON p.org_id=r.owner_org_id WHERE r.id=$1 AND NOT p.classic_allowed)")
        .bind(repo).fetch_one(&mut **tx).await?)
}
