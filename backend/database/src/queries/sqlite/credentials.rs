use crate::queries::credentials::Actor;
use anyhow::Result;
use sqlx::Sqlite;
use sqlx::Transaction;
pub async fn live(tx: &mut sqlx::SqliteConnection, actor: &Actor, write: bool) -> Result<bool> {
    match actor.credential() {
        None => Ok(true),
        Some(credential) => {
            crate::queries::sqlite::users::transitions::credential_live(
                tx,
                actor.user_id,
                &credential,
                write,
            )
            .await
        }
    }
}

/// The caller's own session or token is still live and, for a token, may still write
/// `permission` on the repository or organization. Call it in the transaction that writes.
pub async fn writes(
    tx: &mut sqlx::SqliteConnection,
    actor: &Actor,
    repo: Option<i64>,
    org: Option<i64>,
    permission: &str,
) -> Result<bool> {
    if !live(tx, actor, true).await? {
        return Ok(false);
    }
    if let Some(crate::queries::users::accounts::AccountCredential::ForeignToken(id, hash)) =
        actor.credential()
    {
        return crate::cells::authorization::permits_sqlite(
            tx,
            actor.user_id,
            &crate::cells::authorization::Query {
                kind: "token",
                id: Some(id),
                hash: Some(hash),
                write: true,
            },
            &crate::cells::authorization::Permission {
                repository: repo,
                organization: org,
                name: permission,
                write: true,
            },
        )
        .await;
    }
    match actor.token_id() {
        Some(token) => {
            crate::queries::sqlite::users::tokens::permits_write(
                tx,
                actor.user_id,
                token,
                repo,
                org,
                permission,
            )
            .await
        }
        None => Ok(true),
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

pub async fn repository(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &Actor,
    repo: i64,
    permission: &str,
    write: bool,
) -> Result<bool> {
    if !live(tx, actor, write).await? {
        return Ok(false);
    }
    if let Some(crate::queries::users::accounts::AccountCredential::ForeignToken(id, hash)) =
        actor.credential()
    {
        return crate::cells::authorization::permits_sqlite(
            tx,
            actor.user_id,
            &crate::cells::authorization::Query {
                kind: "token",
                id: Some(id),
                hash: Some(hash),
                write,
            },
            &crate::cells::authorization::Permission {
                repository: Some(repo),
                organization: None,
                name: permission,
                write,
            },
        )
        .await;
    }
    let Some(id) = actor.token_id() else {
        return Ok(true);
    };
    if !crate::queries::sqlite::fine_grained_tokens::repository_permission(
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
        return Ok(crate::queries::credentials::app_permission(
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
        let Some(job) = crate::queries::sqlite::ci_tokens::get_in_connection(tx, id).await? else {
            return Ok(false);
        };
        let permissions: codeyard_types::permissions::Permissions =
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
        if !active && client != codeyard_types::identity::CLI_CLIENT {
            return Ok(false);
        }
        let app: Option<(i64, String)> =
            sqlx::query_as("SELECT id,permissions FROM github_apps WHERE client_id=?1")
                .bind(&client)
                .fetch_optional(&mut **tx)
                .await?;
        if let Some((app, permissions)) = app {
            return Ok(crate::queries::credentials::app_permission(
                &permissions,
                "[]",
                repo,
                permission,
                write,
            )? && installation(tx, app, None, repo).await?);
        }
        return Ok(codeyard_types::oauth_apps::allows(
            &scopes,
            &crate::queries::credentials::oauth_repository_path(permission),
            write,
        ));
    }
    Ok(true)
}

pub async fn organization(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &Actor,
    org: i64,
    permission: &str,
    write: bool,
) -> Result<bool> {
    if !live(tx, actor, write).await? {
        return Ok(false);
    }
    if let Some(crate::queries::users::accounts::AccountCredential::ForeignToken(id, hash)) =
        actor.credential()
    {
        return crate::cells::authorization::permits_sqlite(
            tx,
            actor.user_id,
            &crate::cells::authorization::Query {
                kind: "token",
                id: Some(id),
                hash: Some(hash),
                write,
            },
            &crate::cells::authorization::Permission {
                repository: None,
                organization: Some(org),
                name: permission,
                write,
            },
        )
        .await;
    }
    let Some(id) = actor.token_id() else {
        return Ok(true);
    };
    let bounds: Option<crate::queries::fine_grained_tokens::Bounds> =
        sqlx::query_as(crate::queries::sqlite::fine_grained_tokens::BOUNDS)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(bounds) = bounds {
        let permissions = serde_json::from_str(&bounds.permissions)?;
        return Ok(bounds.active()
            && bounds.user_id == actor.user_id
            && bounds.owner_org_id == Some(org)
            && codeyard_types::fine_grained::allows(&permissions, permission, write));
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
        return Ok((active || client == codeyard_types::identity::CLI_CLIENT)
            && codeyard_types::oauth_apps::allows(
                &scopes,
                &crate::queries::credentials::oauth_organization_path(permission),
                write,
            ));
    }
    Ok(!sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM organization_token_policies WHERE org_id=?1 AND NOT classic_allowed)")
        .bind(org).fetch_one(&mut **tx).await?)
}
