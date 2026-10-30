use crate::backend::db::{
    github_apps::{App, Installation, InstallationFilter, RepositoryChange, Token},
    models::{PersonalAccessToken, Repository},
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn delete_registration(pool: &PgPool, actor: i64, session: &str, id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE").bind(actor).fetch_optional(&mut *tx).await?;
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR SHARE").bind(session).fetch_optional(&mut *tx).await?;
    let client: Option<String> = sqlx::query_scalar("SELECT a.client_id FROM github_apps a JOIN users u ON u.id=a.owner_id JOIN sessions s ON s.user_id=u.id WHERE a.id=$1 AND u.id=$2 AND s.id=$3 AND s.expires_at>clock_timestamp() AND NOT u.is_banned AND NOT u.must_change_password FOR UPDATE OF a")
        .bind(id).bind(actor).bind(session).fetch_optional(&mut *tx).await?;
    let Some(client) = client else { return Ok(false); };
    sqlx::query("DELETE FROM personal_access_tokens WHERE id IN (SELECT t.token_id FROM github_app_tokens t JOIN github_app_installations i ON i.id=t.installation_id WHERE i.app_id=$1) OR id IN (SELECT token_id FROM oauth_app_tokens WHERE client_id=$2)")
        .bind(id).bind(&client).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM oauth_app_grants WHERE client_id=$1").bind(&client).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM oauth_applications WHERE client_id=$1").bind(&client).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

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
    if !super::users::transitions::active_session(tx, actor, session).await? {
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
/// Whether a client's owner, and a GitHub App's bot, may still use it. A missing client is
/// not active.
pub async fn client_active(pool: &PgPool, client: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!("SELECT {}", crate::backend::db::oauth_apps::client_condition("$1"))).bind(client).fetch_one(pool).await?)
}
pub async fn app(pool: &PgPool, key: &str) -> Result<Option<App>> {
    Ok(sqlx::query_as("SELECT a.* FROM github_apps a JOIN users u ON u.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE (a.slug=$1 OR a.client_id=$1 OR CAST(a.id AS TEXT)=$1) AND NOT u.is_banned AND NOT u.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password").bind(key).fetch_optional(pool).await?)
}
/// Authentication accepts indexed numeric ids and client ids, never discovery slugs.
pub async fn app_issuer(pool: &PgPool, issuer: &str) -> Result<Option<App>> {
    let id = issuer.parse::<i64>().ok();
    let column = if id.is_some() { "a.id" } else { "a.client_id" };
    let query = format!("SELECT a.* FROM github_apps a JOIN users u ON u.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE {column}=$1 AND NOT u.is_banned AND NOT u.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password");
    Ok(if let Some(id) = id {
        sqlx::query_as(&query).bind(id).fetch_optional(pool).await?
    } else {
        sqlx::query_as(&query).bind(issuer).fetch_optional(pool).await?
    })
}

/// An app for documents already visible to the caller, regardless of owner state.
pub async fn app_by_id(pool: &PgPool, id: i64) -> Result<Option<App>> {
    Ok(sqlx::query_as("SELECT * FROM github_apps WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn apps(pool: &PgPool, actor: i64, session: &str) -> Result<Vec<App>> {
    Ok(
        sqlx::query_as("SELECT a.* FROM github_apps a JOIN users u ON u.id=a.owner_id JOIN sessions s ON s.user_id=u.id WHERE a.owner_id=$1 AND s.id=$2 AND s.expires_at>clock_timestamp() AND NOT u.is_banned AND NOT u.must_change_password ORDER BY a.id")
            .bind(actor).bind(session)
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
    if !super::users::transitions::active_session(&mut tx, actor, session).await? { return Ok(None); }
    let allowed: bool=sqlx::query_scalar("SELECT COALESCE(($2=$1 AND $3::bigint IS NULL) OR ($2::bigint IS NULL AND EXISTS(SELECT 1 FROM org_memberships WHERE org_id=$3 AND user_id=$1 AND role='owner')),FALSE)").bind(actor).bind(user).bind(org).fetch_one(&mut *tx).await?;
    let visible:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_apps a JOIN users u ON u.id=a.owner_id WHERE a.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND (a.is_public OR (a.owner_id=$2 AND a.owner_id=$3 AND CAST($4 AS BIGINT) IS NULL)))").bind(app).bind(actor).bind(user).bind(org).fetch_one(&mut *tx).await?;
    if !visible || !allowed || user.is_some() == org.is_some() {
        return Ok(None);
    }
    let installed: Installation=sqlx::query_as("INSERT INTO github_app_installations(app_id,user_id,org_id,all_repos) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING RETURNING *").bind(app).bind(user).bind(org).bind(repos.is_none()).fetch_one(&mut *tx).await?;
    for repo in repos.unwrap_or(&[]) {
        let valid: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND NOT is_gist AND (owner_user_id=$2 OR owner_org_id=$3))").bind(repo).bind(user).bind(org).fetch_one(&mut *tx).await?;
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
/// Whether user `actor` reaches installation `i`: as its account, as a member of its
/// organization or as a collaborator on one of its repositories.
fn actor_reaches(actor: &str) -> String {
    format!("(i.user_id={actor} OR EXISTS(SELECT 1 FROM org_memberships WHERE org_id=i.org_id AND user_id={actor}) OR EXISTS(SELECT 1 FROM repo_access ra JOIN repositories r ON r.id=ra.repo_id WHERE ra.user_id={actor} AND ra.access_level BETWEEN 1 AND 5 AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id))))")
}
/// An organization without a usable owner is hidden, as on its public pages.
const ACCOUNT_AVAILABLE: &str = "(i.org_id IS NULL OR EXISTS(SELECT 1 FROM org_memberships m JOIN users owner ON owner.id=m.user_id WHERE m.org_id=i.org_id AND m.role='owner' AND NOT owner.is_banned AND NOT owner.must_change_password))";
pub async fn installations(
    pool: &PgPool,
    filter: &InstallationFilter,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<Installation>)> {
    let filters = format!("($1 IS NULL OR i.app_id=$1) AND ($2 IS NULL OR ({reaches} AND EXISTS(SELECT 1 FROM github_apps a JOIN users u ON u.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE a.id=i.app_id AND NOT u.is_banned AND NOT u.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password))) AND ($3 IS NULL OR i.org_id=$3) AND ($4 IS NULL OR i.id=$4) AND ($2 IS NULL OR $3 IS NULL OR EXISTS(SELECT 1 FROM org_memberships m JOIN users actor ON actor.id=m.user_id WHERE m.org_id=$3 AND m.user_id=$2 AND m.role='owner' AND NOT actor.is_banned AND NOT actor.must_change_password)) AND {ACCOUNT_AVAILABLE}", reaches = actor_reaches("$2"));
    // One authorized snapshot for the count and the page.
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM github_app_installations i WHERE {filters}"))
        .bind(filter.app).bind(filter.actor).bind(filter.org).bind(filter.id)
        .fetch_one(&mut *tx).await?;
    let rows = sqlx::query_as(&format!("SELECT i.* FROM github_app_installations i WHERE {filters} ORDER BY i.id LIMIT $5 OFFSET $6"))
        .bind(filter.app).bind(filter.actor).bind(filter.org).bind(filter.id).bind(limit).bind(offset)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((total, rows))
}
/// The installation of `app` on an account, for discovery by name.
pub async fn account_installation(
    pool: &PgPool,
    app: i64,
    user: Option<i64>,
    org: Option<i64>,
) -> Result<Option<Installation>> {
    Ok(sqlx::query_as("SELECT * FROM github_app_installations WHERE app_id=$1 AND (user_id=$2 OR org_id=$3)")
        .bind(app).bind(user).bind(org).fetch_optional(pool).await?)
}
/// The installation of `app` that covers `repo`, if any; `active` leaves out a suspended one,
/// and `actor` one that user does not reach.
pub async fn repository_installation(
    pool: &PgPool,
    app: i64,
    repo: i64,
    active: bool,
    actor: Option<i64>,
) -> Result<Option<Installation>> {
    Ok(sqlx::query_as(&format!("SELECT i.* FROM github_app_installations i JOIN repositories r ON r.id=$2 WHERE i.app_id=$1 AND (NOT $3 OR i.suspended_at IS NULL) AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) AND ($4 IS NULL OR {reaches})", reaches = actor_reaches("$4")))
        .bind(app).bind(repo).bind(active).bind(actor).fetch_optional(pool).await?)
}
pub async fn installation(pool: &PgPool, id: i64) -> Result<Option<Installation>> {
    Ok(
        sqlx::query_as("SELECT * FROM github_app_installations WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
/// A page of the repositories installation `id` covers, among `selected` when that is not
/// empty, and the count of them all. With a `viewer`, only repositories that user reaches
/// explicitly: their own, as a collaborator or through an organization membership.
pub async fn installation_repositories(
    pool: &PgPool,
    id: i64,
    viewer: Option<i64>,
    selected: &[i64],
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<Repository>)> {
    let filters = format!("(r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND NOT r.is_gist AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)) AND (cardinality($3::BIGINT[]) = 0 OR r.id = ANY($3)) AND ($2 IS NULL OR ((r.owner_user_id=u.id OR EXISTS(SELECT 1 FROM repo_access d WHERE d.repo_id=r.id AND d.user_id=u.id AND d.access_level BETWEEN 1 AND 5) OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=u.id)) AND ({access}) >= 1))", access = crate::backend::db::postgres::repositories::access::USER_REPO_ACCESS_SQL);
    let from = "FROM repositories r JOIN github_app_installations i ON i.id=$1 LEFT JOIN users u ON u.id=$2";
    let mut tx = pool.begin().await?;
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {from} WHERE {filters}"))
        .bind(id).bind(viewer).bind(selected)
        .fetch_one(&mut *tx).await?;
    let rows = sqlx::query_as(&format!("SELECT r.* {from} WHERE {filters} ORDER BY r.id LIMIT $4 OFFSET $5"))
        .bind(id).bind(viewer).bind(selected).bind(limit).bind(offset)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((total, rows))
}
pub async fn managed_installations(
    pool: &PgPool,
    actor: i64,
    session: &str,
) -> Result<Vec<crate::backend::db::github_apps::ManagedInstallation>> {
    Ok(sqlx::query_as("SELECT i.id,a.name AS app_name,COALESCE(account.username,o.name) AS account_name,i.all_repos FROM github_app_installations i JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=$1 LEFT JOIN users account ON account.id=i.user_id LEFT JOIN organizations o ON o.id=i.org_id WHERE NOT u.is_banned AND NOT u.must_change_password AND EXISTS(SELECT 1 FROM sessions s WHERE s.user_id=u.id AND s.id=$2 AND s.expires_at>clock_timestamp()) AND (i.user_id=u.id OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=i.org_id AND m.user_id=u.id AND m.role='owner')) ORDER BY i.id").bind(actor).bind(session).fetch_all(pool).await?)
}
pub async fn rotate_credentials(pool: &PgPool, actor: i64, session: &str, slug: &str, public_key: &str, secret_hash: &str) -> Result<Option<String>> {
    let mut tx = pool.begin().await?;
    super::users::transitions::require_account(&mut tx, actor, &crate::backend::db::users::accounts::AccountCredential::Session(session)).await?;
    let app: Option<(i64, String)> = sqlx::query_as("SELECT id,client_id FROM github_apps WHERE owner_id=$1 AND slug=$2")
        .bind(actor).bind(slug).fetch_optional(&mut *tx).await?;
    let Some((id, client)) = app else { return Ok(None) };
    let client_changed = sqlx::query("UPDATE oauth_applications SET secret_hash=$2 WHERE client_id=$1 AND owner_id=$3")
        .bind(&client).bind(secret_hash).bind(actor).execute(&mut *tx).await?.rows_affected();
    let key_changed = sqlx::query("UPDATE github_apps SET public_key=$2 WHERE id=$1 AND owner_id=$3 AND client_id=$4")
        .bind(id).bind(public_key).bind(actor).bind(&client).execute(&mut *tx).await?.rows_affected();
    anyhow::ensure!(client_changed == 1 && key_changed == 1, "App credentials changed concurrently");
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,target_id) VALUES($1,'github_app.credentials.rotate','github_app',$2)")
        .bind(actor).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(client))
}

pub async fn uninstall(pool: &PgPool, actor: i64, session: &str, id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if !super::users::transitions::active_session_read(&mut tx, actor, session).await? { return Ok(false); }
    let app:Option<i64>=sqlx::query_scalar("SELECT i.app_id FROM github_app_installations i WHERE i.id=$2 AND (i.user_id=$1 OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=i.org_id AND m.user_id=$1 AND m.role='owner')) FOR UPDATE OF i").bind(actor).bind(id).fetch_optional(&mut *tx).await?;
    let Some(app) = app else {
        return Ok(false);
    };
    let changed = change_in_connection(&mut tx, app, id, "delete").await?;
    tx.commit().await?;
    Ok(changed)
}
/// An app changes its own installation only while its owner and bot are active; an
/// account owner uninstalls through `uninstall` whatever the app's state.
pub async fn change_installation(
    pool: &PgPool,
    app: i64,
    id: i64,
    action: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    super::users::transitions::lock_membership_shared(&mut tx).await?;
    let usable: Option<i64> = sqlx::query_scalar("SELECT a.id FROM github_apps a JOIN users owner ON owner.id=a.owner_id JOIN users bot ON bot.id=a.bot_id WHERE a.id=$1 AND NOT owner.is_banned AND NOT owner.must_change_password AND NOT bot.is_banned AND NOT bot.must_change_password FOR SHARE OF owner, bot")
        .bind(app)
        .fetch_optional(&mut *tx)
        .await?;
    if usable.is_none() {
        return Ok(false);
    }
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
    actor: impl Into<crate::backend::db::credentials::Actor>,
    id: i64,
    repo: i64,
    add: bool,
) -> Result<RepositoryChange> {
    let credential = actor.into();
    let actor = credential.user_id;
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
    let installation: Option<(bool, Option<i64>)> = sqlx::query_as(&format!("SELECT i.all_repos,i.org_id FROM github_app_installations i JOIN users u ON u.id=$1 JOIN repositories r ON r.id=$3 WHERE i.id=$2 AND NOT u.is_banned AND NOT u.must_change_password AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND ({})>=5", super::repositories::access::USER_REPO_ACCESS_SQL)).bind(actor).bind(id).bind(repo).fetch_optional(&mut *tx).await?;
    let Some((all_repos, org)) = installation else {
        return Ok(RepositoryChange::Missing);
    };
    if let Some(token) = credential.token_id() {
        if !super::users::tokens::permits_write(&mut tx, actor, token, Some(repo), None, "administration").await? {
            return Ok(RepositoryChange::Missing);
        }
    }
    if all_repos {
        return Ok(RepositoryChange::AllRepositories);
    }
    if !add && org.is_some() {
        // An organization installation keeps at least one repository, as on GitHub.
        let others: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_repositories WHERE installation_id=$1 AND repo_id<>$2)").bind(id).bind(repo).fetch_one(&mut *tx).await?;
        if !others {
            return Ok(RepositoryChange::LastRepository);
        }
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
    Ok(RepositoryChange::Changed)
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
        // A scoped source keeps its installation even after that installation is gone.
        let bound: Option<i64> =
            sqlx::query_scalar("SELECT installation_id FROM github_app_tokens WHERE token_id=$1 FOR SHARE")
                .bind(source.id)
                .fetch_optional(&mut *tx)
                .await?;
        if bound.is_some_and(|bound| bound != installation) {
            return Ok(None);
        }
        if let Some(parent_expiry) = source.expires_at {
            expiry = expiry.min(parent_expiry);
        }
        let eligible: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM github_app_installations i WHERE i.id=$1 AND {})", actor_reaches("$2"))).bind(installation).bind(actor).fetch_one(&mut *tx).await?;
        if !eligible {
            return Ok(None);
        }
    }
    let requested: Vec<i64> = serde_json::from_str(repos)?;
    for repo in requested {
        let valid: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN github_app_installations i ON i.id=$1 WHERE r.id=$2 AND NOT r.is_gist AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))").bind(installation).bind(repo).fetch_one(&mut *tx).await?;
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
/// A token's App bounds even when its installation or app is gone, so a scoped token never
/// reads as unscoped.
pub async fn token_bounds(pool: &PgPool, id: i64) -> Result<Option<Token>> {
    Ok(sqlx::query_as("SELECT * FROM github_app_tokens WHERE token_id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn request_installation(
    pool: &PgPool,
    actor: i64,
    session: &str,
    app: i64,
    org: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if !super::users::transitions::active_session(&mut tx, actor, session).await? { return Ok(false); }
    let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN github_apps a ON a.id=$3 AND a.is_public WHERE m.user_id=$1 AND m.org_id=$2 AND m.role IN ('member','admin') AND NOT EXISTS(SELECT 1 FROM github_app_installations WHERE app_id=$3 AND org_id=$2))").bind(actor).bind(org).bind(app).fetch_one(&mut *tx).await?;
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
    Ok(sqlx::query_as("SELECT q.* FROM github_app_requests q JOIN org_memberships m ON m.org_id=q.org_id AND m.user_id=q.user_id JOIN users u ON u.id=q.user_id WHERE q.app_id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND NOT EXISTS(SELECT 1 FROM github_app_installations i WHERE i.app_id=q.app_id AND i.org_id=q.org_id) AND EXISTS(SELECT 1 FROM org_memberships om JOIN users owner ON owner.id=om.user_id WHERE om.org_id=q.org_id AND om.role='owner' AND NOT owner.is_banned AND NOT owner.must_change_password) ORDER BY q.id").bind(app).fetch_all(pool).await?)
}

pub async fn revoke(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM personal_access_tokens WHERE id=$1 AND EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=$1)").bind(id).execute(pool).await?;
    Ok(())
}
