use crate::backend::db::{
    github_apps::{App, Installation, Token},
    models::{PersonalAccessToken, Repository},
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn register(
    pool: &PgPool,
    actor: i64,
    session: &str,
    app: &App,
    secret_hash: &str,
) -> Result<Option<App>> {
    let mut tx = pool.begin().await?;
    let registered = register_in_connection(&mut tx, actor, session, app, secret_hash).await?;
    tx.commit().await?;
    Ok(registered)
}
pub(super) async fn register_in_connection(
    tx: &mut sqlx::PgConnection,
    actor: i64,
    session: &str,
    app: &App,
    secret_hash: &str,
) -> Result<Option<App>> {
    super::users::transitions::lock_membership_shared(tx).await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE")
        .bind(session)
        .fetch_optional(&mut *tx)
        .await?;
    let live: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users u JOIN sessions s ON s.user_id=u.id WHERE u.id=$1 AND s.id=$2 AND NOT u.is_banned AND NOT u.must_change_password AND s.expires_at>clock_timestamp())").bind(actor).bind(session).fetch_one(&mut *tx).await?;
    if !live {
        return Ok(None);
    }
    sqlx::query("INSERT INTO oauth_applications(owner_id,client_id,secret_hash,name,homepage,redirect_uri,is_github_app) VALUES($1,$2,$3,$4,$5,$5,TRUE)").bind(actor).bind(&app.client_id).bind(secret_hash).bind(&app.name).bind(&app.homepage).execute(&mut *tx).await?;
    let bot: i64 =
        sqlx::query_scalar("INSERT INTO users(username,display_name) VALUES($1,$2) RETURNING id")
            .bind(format!("{}[bot]", app.slug))
            .bind(&app.name)
            .fetch_one(&mut *tx)
            .await?;
    let app=sqlx::query_as("INSERT INTO github_apps(owner_id,bot_id,client_id,slug,name,description,homepage,public_key,permissions,events,access_level,is_public) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) RETURNING *")
        .bind(actor).bind(bot).bind(&app.client_id).bind(&app.slug).bind(&app.name).bind(&app.description).bind(&app.homepage).bind(&app.public_key).bind(&app.permissions).bind(&app.events).bind(app.access_level).bind(app.is_public).fetch_one(&mut *tx).await?;
    Ok(Some(app))
}
pub async fn client_active(pool: &PgPool, client: &str) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM oauth_applications c WHERE c.client_id=$1 AND c.is_github_app AND NOT EXISTS(SELECT 1 FROM github_apps a JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE a.client_id=c.client_id AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password))").bind(client).fetch_one(pool).await?)
}
pub async fn app(pool: &PgPool, key: &str) -> Result<Option<App>> {
    Ok(sqlx::query_as("SELECT a.* FROM github_apps a JOIN users u ON u.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE (a.slug=$1 OR a.client_id=$1 OR CAST(a.id AS TEXT)=$1) AND NOT u.is_banned AND NOT u.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password").bind(key).fetch_optional(pool).await?)
}
pub async fn apps(pool: &PgPool, actor: i64) -> Result<Vec<App>> {
    Ok(
        sqlx::query_as("SELECT * FROM github_apps WHERE owner_id=$1 ORDER BY id")
            .bind(actor)
            .fetch_all(pool)
            .await?,
    )
}
pub async fn install(
    pool: &PgPool,
    actor: i64,
    session: &str,
    app: i64,
    user: Option<i64>,
    org: Option<i64>,
    repos: Option<&[i64]>,
) -> Result<Option<Installation>> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE")
        .bind(session)
        .fetch_optional(&mut *tx)
        .await?;
    let allowed: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users u JOIN sessions s ON s.user_id=u.id WHERE u.id=$1 AND s.id=$2 AND s.expires_at>clock_timestamp() AND NOT u.is_banned AND NOT u.must_change_password AND (($3=u.id AND $4 IS NULL) OR ($3 IS NULL AND EXISTS(SELECT 1 FROM org_memberships WHERE org_id=$4 AND user_id=u.id AND role='owner'))))").bind(actor).bind(session).bind(user).bind(org).fetch_one(&mut *tx).await?;
    let visible:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND (a.is_public OR (a.owner_id=$2 AND a.owner_id=$3 AND CAST($4 AS BIGINT) IS NULL)))").bind(app).bind(actor).bind(user).bind(org).fetch_one(&mut *tx).await?;
    if !visible || !allowed || user.is_some() == org.is_some() {
        return Ok(None);
    }
    let installed: Installation=sqlx::query_as("INSERT INTO github_app_installations(app_id,user_id,org_id,all_repos) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING RETURNING *").bind(app).bind(user).bind(org).bind(repos.is_none()).fetch_one(&mut *tx).await?;
    for repo in repos.unwrap_or(&[]) {
        let valid: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND (owner_user_id=$2 OR owner_org_id=$3))").bind(repo).bind(user).bind(org).fetch_one(&mut *tx).await?;
        if !valid {
            return Ok(None);
        }
        sqlx::query("INSERT INTO github_app_repositories(installation_id,repo_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(installed.id).bind(repo).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM github_app_requests WHERE app_id=$1 AND org_id=$2")
        .bind(app)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(installed))
}
pub async fn installations(
    pool: &PgPool,
    app: Option<i64>,
    actor: Option<i64>,
) -> Result<Vec<Installation>> {
    Ok(sqlx::query_as("SELECT i.* FROM github_app_installations i WHERE ($1 IS NULL OR i.app_id=$1) AND ($2 IS NULL OR i.user_id=$2 OR EXISTS(SELECT 1 FROM org_memberships WHERE org_id=i.org_id AND user_id=$2) OR EXISTS(SELECT 1 FROM repo_access ra JOIN repositories r ON r.id=ra.repo_id WHERE ra.user_id=$2 AND ra.access_level BETWEEN 1 AND 5 AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))) AND ($2 IS NULL OR EXISTS(SELECT 1 FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.id=i.app_id AND NOT u.is_banned AND NOT u.must_change_password)) ORDER BY i.id").bind(app).bind(actor).fetch_all(pool).await?)
}
pub async fn installation(pool: &PgPool, id: i64) -> Result<Option<Installation>> {
    Ok(
        sqlx::query_as("SELECT * FROM github_app_installations WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn has_repository(pool: &PgPool, id: i64, repo: i64, active: bool) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN github_app_installations i ON i.id=$1 WHERE r.id=$2 AND (NOT $3 OR i.suspended_at IS NULL) AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))").bind(id).bind(repo).bind(active).fetch_one(pool).await?)
}
pub async fn repositories(pool: &PgPool, id: i64) -> Result<Vec<Repository>> {
    Ok(sqlx::query_as("SELECT r.* FROM repositories r JOIN github_app_installations i ON i.id=$1 WHERE (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) ORDER BY r.id").bind(id).fetch_all(pool).await?)
}
pub async fn managed_installations(
    pool: &PgPool,
    actor: i64,
) -> Result<Vec<crate::backend::db::github_apps::ManagedInstallation>> {
    Ok(sqlx::query_as("SELECT i.id,a.name AS app_name,COALESCE(account.username,o.name) AS account_name,i.all_repos FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=$1 LEFT JOIN users account ON account.id=i.user_id LEFT JOIN organizations o ON o.id=i.org_id WHERE NOT u.is_banned AND NOT u.must_change_password AND (i.user_id=u.id OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=i.org_id AND m.user_id=u.id AND m.role='owner')) ORDER BY i.id").bind(actor).fetch_all(pool).await?)
}
pub async fn uninstall(pool: &PgPool, actor: i64, session: &str, id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR SHARE")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE")
        .bind(session)
        .fetch_optional(&mut *tx)
        .await?;
    let app:Option<i64>=sqlx::query_scalar("SELECT i.app_id FROM github_app_installations i JOIN users u ON u.id=$1 JOIN sessions s ON s.user_id=u.id WHERE i.id=$3 AND s.id=$2 AND s.expires_at>clock_timestamp() AND NOT u.is_banned AND NOT u.must_change_password AND (i.user_id=u.id OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=i.org_id AND m.user_id=u.id AND m.role='owner')) FOR UPDATE OF i").bind(actor).bind(session).bind(id).fetch_optional(&mut *tx).await?;
    let Some(app) = app else {
        return Ok(false);
    };
    let changed = change_in_connection(&mut tx, app, id, "delete").await?;
    tx.commit().await?;
    Ok(changed)
}
pub async fn change_installation(pool: &PgPool, app: i64, id: i64, action: &str) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let changed = change_in_connection(&mut tx, app, id, action).await?;
    tx.commit().await?;
    Ok(changed)
}
async fn change_in_connection(
    tx: &mut sqlx::PgConnection,
    app: i64,
    id: i64,
    action: &str,
) -> Result<bool> {
    // Serialize revocation with issuance before selecting the rows to delete.
    let live: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM github_app_installations WHERE app_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(app)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if live.is_none() {
        return Ok(false);
    }
    if action == "delete" {
        sqlx::query("DELETE FROM personal_access_tokens WHERE id IN (SELECT t.token_id FROM github_app_tokens t JOIN github_app_installations i ON i.id=t.installation_id WHERE i.id=$1 AND i.app_id=$2)").bind(id).bind(app).execute(&mut *tx).await?;
    }
    let sql=match action {
        "delete" => "DELETE FROM github_app_installations WHERE app_id=$1 AND id=$2",
        "suspend" => "UPDATE github_app_installations SET suspended_at=clock_timestamp(),updated_at=clock_timestamp() WHERE app_id=$1 AND id=$2",
        "unsuspend" => "UPDATE github_app_installations SET suspended_at=NULL,suspended_by=NULL,updated_at=clock_timestamp() WHERE app_id=$1 AND id=$2",
        _ => anyhow::bail!("invalid installation action"),
    };
    let changed = sqlx::query(sql)
        .bind(app)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if changed && action == "suspend" {
        sqlx::query("DELETE FROM personal_access_tokens WHERE id IN (SELECT token_id FROM github_app_tokens WHERE installation_id=$1)").bind(id).execute(&mut *tx).await?;
    }
    Ok(changed)
}
pub async fn change_repository(
    pool: &PgPool,
    actor: i64,
    id: i64,
    repo: i64,
    add: bool,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR SHARE")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("SELECT id FROM github_app_installations WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let allowed: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_installations i JOIN users u ON u.id=$1 JOIN repositories r ON r.id=$3 WHERE i.id=$2 AND NOT i.all_repos AND NOT u.is_banned AND NOT u.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.user_id=u.id OR EXISTS(SELECT 1 FROM org_memberships WHERE org_id=i.org_id AND user_id=u.id AND role='owner')))").bind(actor).bind(id).bind(repo).fetch_one(&mut *tx).await?;
    if !allowed {
        return Ok(false);
    }
    if add {
        sqlx::query("INSERT INTO github_app_repositories(installation_id,repo_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(id).bind(repo).execute(&mut *tx).await?;
    } else {
        sqlx::query("DELETE FROM github_app_repositories WHERE installation_id=$1 AND repo_id=$2")
            .bind(id)
            .bind(repo)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}
#[expect(
    clippy::too_many_arguments,
    reason = "Credential issuance persists its explicit installation and permission bounds."
)]
pub async fn issue(
    pool: &PgPool,
    app: i64,
    installation: i64,
    actor: Option<i64>,
    source: Option<&str>,
    hash: &str,
    permissions: &str,
    repos: &str,
    write: bool,
) -> Result<Option<PersonalAccessToken>> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let owner: Option<i64>=sqlx::query_scalar("SELECT COALESCE($3,a.bot_id) FROM github_apps a JOIN github_app_installations i ON i.app_id=a.id JOIN users u ON u.id=a.owner_id WHERE a.id=$1 AND i.id=$2 AND i.suspended_at IS NULL AND NOT u.is_banned AND NOT u.must_change_password FOR SHARE OF a,i,u").bind(app).bind(installation).bind(actor).fetch_optional(&mut *tx).await?;
    let Some(owner) = owner else {
        return Ok(None);
    };
    let mut expiry = chrono::Utc::now() + chrono::Duration::hours(1);
    if let Some(actor) = actor {
        let source:Option<PersonalAccessToken>=sqlx::query_as("SELECT t.id,t.user_id,t.name,t.scope,t.expires_at,t.created_at FROM personal_access_tokens t JOIN oauth_app_tokens o ON o.token_id=t.id JOIN github_apps a ON a.client_id=o.client_id WHERE a.id=$1 AND t.token_hash=$2 AND t.user_id=$3 AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) FOR SHARE OF t,o").bind(app).bind(source).bind(actor).fetch_optional(&mut *tx).await?;
        let Some(source) = source else {
            return Ok(None);
        };
        if let Some(parent_expiry) = source.expires_at {
            expiry = expiry.min(parent_expiry);
        }
        let eligible:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_installations i WHERE i.id=$1 AND (i.user_id=$2 OR EXISTS(SELECT 1 FROM org_memberships WHERE org_id=i.org_id AND user_id=$2) OR EXISTS(SELECT 1 FROM repo_access ra JOIN repositories r ON r.id=ra.repo_id WHERE ra.user_id=$2 AND ra.access_level BETWEEN 1 AND 5 AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))))").bind(installation).bind(actor).fetch_one(&mut *tx).await?;
        if !eligible {
            return Ok(None);
        }
    }
    let requested: Vec<i64> = serde_json::from_str(repos)?;
    for repo in requested {
        let valid: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN github_app_installations i ON i.id=$1 WHERE r.id=$2 AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))").bind(installation).bind(repo).fetch_one(&mut *tx).await?;
        if !valid {
            return Ok(None);
        }
    }
    let token: Option<PersonalAccessToken>=sqlx::query_as("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope,expires_at) SELECT id,'GitHub App',$2,$3,$4 FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password RETURNING id,user_id,name,scope,expires_at,created_at").bind(owner).bind(hash).bind(if write {"write"}else{"read"}).bind(expiry).fetch_optional(&mut *tx).await?;
    let Some(token) = token else {
        return Ok(None);
    };
    sqlx::query("INSERT INTO github_app_tokens(token_id,installation_id,permissions,repositories,user_token) VALUES($1,$2,$3,$4,$5)").bind(token.id).bind(installation).bind(permissions).bind(repos).bind(actor.is_some()).execute(&mut *tx).await?;
    if actor.is_some() {
        sqlx::query("INSERT INTO oauth_app_tokens(token_id,client_id,scopes) SELECT $1,client_id,'' FROM github_apps WHERE id=$2").bind(token.id).bind(app).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Some(token))
}
pub async fn token(pool: &PgPool, id: i64) -> Result<Option<Token>> {
    Ok(sqlx::query_as("SELECT t.* FROM github_app_tokens t JOIN github_app_installations i ON i.id=t.installation_id JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=a.owner_id WHERE t.token_id=$1 AND i.suspended_at IS NULL AND NOT u.is_banned AND NOT u.must_change_password").bind(id).fetch_optional(pool).await?)
}
pub async fn request_installation(
    pool: &PgPool,
    actor: i64,
    session: &str,
    app: i64,
    org: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE")
        .bind(session)
        .fetch_optional(&mut *tx)
        .await?;
    let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users u JOIN sessions s ON s.user_id=u.id JOIN org_memberships m ON m.user_id=u.id JOIN github_apps a ON a.id=$4 AND a.is_public WHERE u.id=$1 AND s.id=$2 AND s.expires_at>clock_timestamp() AND NOT u.is_banned AND NOT u.must_change_password AND m.org_id=$3 AND m.role IN ('member','admin') AND NOT EXISTS(SELECT 1 FROM github_app_installations WHERE app_id=$4 AND org_id=$3))").bind(actor).bind(session).bind(org).bind(app).fetch_one(&mut *tx).await?;
    if !live {
        return Ok(false);
    }
    sqlx::query("INSERT INTO github_app_requests(app_id,org_id,user_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(app).bind(org).bind(actor).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
pub async fn requests(
    pool: &PgPool,
    app: i64,
) -> Result<Vec<crate::backend::db::github_apps::InstallationRequest>> {
    Ok(sqlx::query_as("SELECT q.* FROM github_app_requests q JOIN org_memberships m ON m.org_id=q.org_id AND m.user_id=q.user_id JOIN users u ON u.id=q.user_id WHERE q.app_id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND NOT EXISTS(SELECT 1 FROM github_app_installations i WHERE i.app_id=q.app_id AND i.org_id=q.org_id) ORDER BY q.id").bind(app).fetch_all(pool).await?)
}

pub async fn revoke(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM personal_access_tokens WHERE id=$1 AND EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=$1)").bind(id).execute(pool).await?;
    Ok(())
}
