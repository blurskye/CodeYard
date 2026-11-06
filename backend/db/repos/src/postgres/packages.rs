use crate::packages::Actor;
use crate::packages::Blob;
use crate::packages::Charge;
use crate::packages::Credential;
use crate::packages::Listing;
use crate::packages::Owner;
use crate::packages::Package;
use crate::packages::Permission;
use crate::packages::Publish;
use crate::packages::Refusal;
use crate::packages::Upload;
use crate::packages::Version;
use anyhow::ensure;
use anyhow::Result;
use codeyard_db_core::pool::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

type Tx<'a> = Transaction<'a, Postgres>;

async fn begin(pool: &PgPool) -> Result<Tx<'_>> {
    Ok(pool.begin().await?)
}

async fn read(pool: &PgPool) -> Result<Tx<'_>> {
    let mut tx = pool.begin().await?;
    // Authorization and metadata must come from the same snapshot without a package lock.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn lock_key(tx: &mut Tx<'_>, key: &str) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || ':package:' || $1, 723947120))")
        .bind(key).execute(&mut **tx).await?;
    Ok(())
}

async fn lock_package_key(
    tx: &mut Tx<'_>,
    owner_kind: &str,
    owner: i64,
    ecosystem: &str,
    name: &str,
) -> Result<()> {
    lock_key(
        tx,
        &serde_json::to_string(&(owner_kind, owner, ecosystem, name))?,
    )
    .await
}

async fn lock_package(tx: &mut Tx<'_>, id: i64) -> Result<()> {
    let key: Option<(String, i64, String, String)> = sqlx::query_as(
        "SELECT owner_kind,COALESCE(owner_user_id,owner_org_id),package_type,name FROM packages WHERE id=$1",
    ).bind(id).fetch_optional(&mut **tx).await?;
    let (kind, owner, ecosystem, name) = key.ok_or(Refusal::Missing)?;
    lock_package_key(tx, &kind, owner, &ecosystem, &name).await
}

async fn actor<'a>(
    tx: &mut Tx<'_>,
    token: Option<impl Into<Credential<'a>>>,
    permission: Permission,
) -> Result<Actor> {
    let Some(token) = token.map(Into::into) else {
        ensure!(
            matches!(permission, Permission::Read),
            Refusal::Unauthorized
        );
        return Ok(Actor::ANONYMOUS);
    };
    let token = match token {
        Credential::Token(id) => id,
        Credential::Session(session) => {
            let accounts: Vec<i64> =
                sqlx::query_scalar(&crate::packages::session_account_query("clock_timestamp()"))
                    .bind(session)
                    .bind(codeyard_support::token_hash::token_hash(session))
                    .fetch_all(&mut **tx)
                    .await?;
            ensure!(accounts.len() == 1, Refusal::Unauthorized);
            let account = accounts[0];
            codeyard_db_core::queries::postgres::users::transitions::lock_accounts_shared(
                tx,
                &[account],
            )
            .await?;
            ensure!(
                codeyard_db_core::queries::postgres::users::transitions::live_session(
                    tx, account, session
                )
                .await?,
                Refusal::Unauthorized
            );
            return Ok(Actor::session(account));
        }
    };
    let account: Option<i64> =
        sqlx::query_scalar("SELECT user_id FROM personal_access_tokens WHERE id=$1")
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    let account = account.ok_or(Refusal::Unauthorized)?;
    codeyard_db_core::queries::postgres::users::transitions::lock_accounts_shared(tx, &[account])
        .await?;
    let row: Option<(i64, String, String)> = sqlx::query_as("SELECT u.id,u.username,t.scope FROM users u JOIN personal_access_tokens t ON t.user_id=u.id WHERE t.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) FOR SHARE OF t")
        .bind(token).fetch_optional(&mut **tx).await?;
    let (id, name, scope) = row.ok_or(Refusal::Unauthorized)?;
    let write = !matches!(permission, Permission::Read);
    // App credentials have no package grant, and an OAuth token only reads packages with the
    // read:packages scope; neither collapses into account-wide identity otherwise.
    let app: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=$1) OR EXISTS(SELECT 1 FROM oauth_app_tokens o JOIN github_apps a ON a.client_id=o.client_id WHERE o.token_id=$1)").bind(token).fetch_one(&mut **tx).await?;
    ensure!(!app, Refusal::Forbidden);
    let oauth: Option<String> =
        sqlx::query_scalar("SELECT scopes FROM oauth_app_tokens WHERE token_id=$1")
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    ensure!(
        oauth.is_none_or(
            |scopes| !write && codeyard_model::oauth_apps::has_scope(&scopes, "read:packages")
        ),
        Refusal::Forbidden
    );
    ensure!(!write || scope == "write", Refusal::Forbidden);
    let mut owner = None;
    let mut selected = None;
    let mut classic = name != codeyard_db_core::queries::ci_tokens::BOT;
    let _: Option<i64> =
        sqlx::query_scalar("SELECT token_id FROM fine_grained_tokens WHERE token_id=$1 FOR SHARE")
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    let bounds: Option<codeyard_db_core::queries::fine_grained_tokens::Bounds> =
        sqlx::query_as(codeyard_db_core::queries::postgres::fine_grained_tokens::BOUNDS)
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(bounds) = bounds {
        classic = false;
        selected = (bounds.repository_access != "all").then_some(token);
        let permissions: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&bounds.permissions)?;
        let level = permissions
            .get("packages")
            .and_then(serde_json::Value::as_str);
        if !(bounds.active() && (level == Some("write") || (!write && level == Some("read")))) {
            // Without a packages grant the token reads public packages only.
            ensure!(!write, Refusal::Forbidden);
            return Ok(Actor::ANONYMOUS);
        }
        owner = Some(match bounds.owner_org_id {
            Some(org) => ("org", org),
            None => ("user", id),
        });
    }
    let mut repo = None;
    if name == codeyard_db_core::queries::ci_tokens::BOT {
        let job =
            codeyard_db_core::queries::postgres::ci_tokens::get_in_connection(&mut *tx, token)
                .await?
                .ok_or(Refusal::Unauthorized)?;
        let (repo_id, permissions) = (job.repo_id, job.permissions);
        let permissions: codeyard_model::permissions::Permissions =
            serde_json::from_str(&permissions)?;
        ensure!(permissions.allows("packages", write), Refusal::Forbidden);
        repo = Some(repo_id);
    }
    Ok(Actor {
        id: Some(id),
        repo,
        owner,
        selected,
        classic,
    })
}

fn read_condition(actor: &Actor) -> String {
    crate::packages::read_condition(
        actor,
        codeyard_db_core::queries::postgres::repositories::access::USER_REPO_ACCESS_SQL,
    )
}

async fn owner_level(tx: &mut Tx<'_>, package: &Package, user: i64) -> Result<i64> {
    if package.owner_user_id == Some(user) {
        return Ok(6);
    }
    if package.owner_user_id.is_some() {
        let admin: bool = sqlx::query_scalar("SELECT is_admin FROM users WHERE id=$1")
            .bind(user)
            .fetch_one(&mut **tx)
            .await?;
        return Ok(if admin { 5 } else { 0 });
    }
    let level: Option<i64> = sqlx::query_scalar("SELECT (CASE m.role WHEN 'owner' THEN 6 WHEN 'admin' THEN 5 WHEN 'member' THEN CASE WHEN $3 THEN 1 ELSE 0 END ELSE 0 END)::bigint FROM org_memberships m WHERE m.org_id=$1 AND m.user_id=$2 AND EXISTS(SELECT 1 FROM org_memberships own JOIN users u ON u.id=own.user_id WHERE own.org_id=m.org_id AND own.role='owner' AND NOT u.is_banned AND NOT u.must_change_password)")
        .bind(package.owner_org_id).bind(user).bind(package.repo_id.is_some()).fetch_optional(&mut **tx).await?;
    Ok(level.unwrap_or(0))
}

async fn authorize<'a>(
    tx: &mut Tx<'_>,
    id: i64,
    token: Option<impl Into<Credential<'a>>>,
    permission: Permission,
    deleted: bool,
) -> Result<Package> {
    let actor = actor(tx, token, permission).await?;
    let package: Package =
        sqlx::query_as("SELECT * FROM packages WHERE id=$1 AND (deleted_at IS NULL OR $2)")
            .bind(id)
            .bind(deleted)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Refusal::Missing)?;
    if let Some(org) = package.owner_org_id {
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR SHARE")
            .bind(org)
            .execute(&mut **tx)
            .await?;
    }
    let actor = actor.for_owner(
        &package.owner_kind,
        package.owner_user_id.or(package.owner_org_id),
    );
    ensure!(
        actor.id.is_some() || matches!(permission, Permission::Read),
        Refusal::Forbidden
    );
    ensure!(
        actor.repo.is_none() || actor.repo == package.repo_id,
        Refusal::Missing
    );
    if let Some(repo) = actor.repo {
        let same_owner: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND owner_kind=$2 AND COALESCE(owner_user_id,owner_org_id)=$3)")
            .bind(repo).bind(&package.owner_kind).bind(package.owner_user_id.or(package.owner_org_id)).fetch_one(&mut **tx).await?;
        ensure!(same_owner, Refusal::Missing);
    }
    let readable: bool = sqlx::query_scalar(&format!("SELECT COALESCE({},false) FROM packages p LEFT JOIN repositories r ON r.id=p.repo_id LEFT JOIN users u ON u.id=$2 WHERE p.id=$1", read_condition(&actor)))
        .bind(id).bind(actor.id).fetch_one(&mut **tx).await?;
    ensure!(readable, Refusal::Missing);
    if !matches!(permission, Permission::Read) {
        let scoped: bool = sqlx::query_scalar(&format!(
            "SELECT {} FROM packages p WHERE p.id=$1",
            actor.scope_condition()
        ))
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        ensure!(scoped, Refusal::Forbidden);
    }
    if !matches!(permission, Permission::Read) {
        let user = actor.id.ok_or(Refusal::Unauthorized)?;
        let mut level = owner_level(tx, &package, user).await?;
        if let Some(repo) = package.repo_id {
            let locked = codeyard_db_core::queries::postgres::repositories::access::lock_user_repo_access_in_transaction(
                &mut *tx,
                user,
                repo,
                codeyard_db_core::queries::postgres::repositories::access::RepositoryRowLock::Shared,
                matches!(permission, Permission::Admin),
            )
            .await?
            .ok_or(Refusal::Missing)?;
            let access = locked.access;
            level = level.max(access as i64);
            if actor.repo == Some(repo) && access.can_write() {
                level = level.max(5);
            }
        }
        ensure!(
            level
                >= if matches!(permission, Permission::Admin) {
                    5
                } else {
                    3
                },
            Refusal::Forbidden
        );
    }
    Ok(package)
}

pub async fn container_namespace(pool: &PgPool, owner: &Owner) -> Result<String> {
    let mut tx = begin(pool).await?;
    lock_key(&mut tx, &format!("namespace:{}:{}", owner.kind, owner.id)).await?;
    if let Some(namespace) = sqlx::query_scalar::<_, String>("SELECT namespace FROM registry_namespaces WHERE ($1='user' AND owner_user_id=$2) OR ($1='org' AND owner_org_id=$2)")
        .bind(&owner.kind).bind(owner.id).fetch_optional(&mut *tx).await? { return Ok(namespace); }
    let base = owner.name.to_ascii_lowercase();
    let base = if codeyard_model::package_input::valid_name(&base) {
        base
    } else {
        format!("cy-{}-{}", owner.kind, owner.id)
    };
    for attempt in 0..1024 {
        let candidate = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{}-{}-{attempt}", owner.kind, owner.id)
        };
        // Never steal the literal namespace of an existing lower-case account. Once
        // assigned, the mapping remains stable across case collisions and renames.
        let occupied: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM registry_namespaces WHERE namespace=$1) OR EXISTS(SELECT 1 FROM users WHERE username=$1 AND NOT ($2='user' AND id=$3)) OR EXISTS(SELECT 1 FROM organizations WHERE name=$1 AND NOT ($2='org' AND id=$3))")
            .bind(&candidate).bind(&owner.kind).bind(owner.id).fetch_one(&mut *tx).await?;
        if occupied {
            continue;
        }
        let inserted = sqlx::query("INSERT INTO registry_namespaces(namespace,owner_user_id,owner_org_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(&candidate).bind((owner.kind == "user").then_some(owner.id)).bind((owner.kind == "org").then_some(owner.id))
            .execute(&mut *tx).await?.rows_affected();
        if inserted == 0 {
            continue;
        }
        tx.commit().await?;
        return Ok(candidate);
    }
    anyhow::bail!("container namespace allocation exhausted")
}

pub async fn container_owner(pool: &PgPool, namespace: &str) -> Result<Option<Owner>> {
    if !codeyard_model::package_input::valid_name(namespace) || namespace.contains('/') {
        return Ok(None);
    }
    let mapped: Option<Owner> = sqlx::query_as("SELECT u.id,u.username AS name,'user' AS kind FROM registry_namespaces n JOIN users u ON u.id=n.owner_user_id WHERE n.namespace=$1 UNION ALL SELECT o.id,o.name,'org' AS kind FROM registry_namespaces n JOIN organizations o ON o.id=n.owner_org_id WHERE n.namespace=$1")
        .bind(namespace).fetch_optional(pool).await?;
    if mapped.is_some() {
        return Ok(mapped);
    }
    let owner: Option<Owner> = sqlx::query_as("SELECT * FROM (SELECT id,username AS name,'user' AS kind FROM users UNION ALL SELECT id,name,'org' AS kind FROM organizations) owners WHERE LOWER(name)=$1 ORDER BY CASE WHEN name=$1 THEN 0 ELSE 1 END,id LIMIT 1")
        .bind(namespace).fetch_optional(pool).await?;
    if let Some(owner) = owner {
        if container_namespace(pool, &owner).await? == namespace {
            return Ok(Some(owner));
        }
    }
    Ok(None)
}

pub async fn owner(pool: &PgPool, name: &str, kind: Option<&str>) -> Result<Option<Owner>> {
    Ok(sqlx::query_as("SELECT id,username AS name,'user' AS kind FROM users WHERE username=$1 AND ($2::text IS NULL OR $2='user') UNION ALL SELECT id,name,'org' AS kind FROM organizations WHERE name=$1 AND ($2::text IS NULL OR $2='org')")
        .bind(name).bind(kind).fetch_optional(pool).await?)
}

pub async fn find(
    pool: &PgPool,
    owner: &Owner,
    kind: &str,
    name: &str,
    token: Option<Credential<'_>>,
    permission: Permission,
    deleted: bool,
) -> Result<Package> {
    let mut tx = read(pool).await?;
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM packages WHERE owner_kind=$1 AND COALESCE(owner_user_id,owner_org_id)=$2 AND package_type=$3 AND name=$4")
        .bind(&owner.kind).bind(owner.id).bind(kind).bind(name).fetch_optional(&mut *tx).await?;
    let result = authorize(
        &mut tx,
        id.ok_or(Refusal::Missing)?,
        token,
        permission,
        deleted,
    )
    .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn list(
    pool: &PgPool,
    token: Option<Credential<'_>>,
    query: Listing<'_>,
) -> Result<Vec<Package>> {
    listing(pool, token, query, None).await
}

pub async fn owner_page(
    pool: &PgPool,
    token: Option<Credential<'_>>,
    owner: &Owner,
    kind: &str,
    before: i64,
) -> Result<Vec<Package>> {
    listing(
        pool,
        token,
        Listing {
            owner: Some(owner),
            repo: None,
            kind: Some(kind),
            visibility: None,
            offset: 0,
            limit: 100,
        },
        Some(before),
    )
    .await
}

async fn listing(
    pool: &PgPool,
    token: Option<Credential<'_>>,
    query: Listing<'_>,
    before: Option<i64>,
) -> Result<Vec<Package>> {
    let mut tx = read(pool).await?;
    let actor = actor(&mut tx, token, Permission::Read).await?;
    let actor = match query.owner {
        Some(owner) => actor.for_owner(&owner.kind, Some(owner.id)),
        None if actor.owner.is_some() => Actor::ANONYMOUS,
        None => actor,
    };
    let rows = sqlx::query_as(&format!("SELECT p.* FROM packages p LEFT JOIN repositories r ON r.id=p.repo_id LEFT JOIN users u ON u.id=$5 WHERE ($1::text IS NULL OR p.owner_kind=$1) AND ($2::bigint IS NULL OR COALESCE(p.owner_user_id,p.owner_org_id)=$2) AND ($3::text IS NULL OR p.package_type=$3) AND p.deleted_at IS NULL AND ($4::text IS NULL OR p.visibility=$4) AND ($6::bigint IS NULL OR (p.repo_id=$6 AND r.owner_kind=p.owner_kind AND COALESCE(r.owner_user_id,r.owner_org_id)=COALESCE(p.owner_user_id,p.owner_org_id))) AND ($9::bigint IS NULL OR p.repo_id=$9) AND ($10::bigint IS NULL OR p.id<$10) AND {} ORDER BY p.id DESC LIMIT $7 OFFSET $8",read_condition(&actor)))
        .bind(query.owner.map(|o| o.kind.as_str())).bind(query.owner.map(|o| o.id)).bind(query.kind).bind(query.visibility).bind(actor.id).bind(actor.repo).bind(query.limit).bind(query.offset).bind(query.repo).bind(before).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn count_linked(pool: &PgPool, token: Option<Credential<'_>>, repo: i64) -> Result<i64> {
    let mut tx = read(pool).await?;
    let actor = actor(&mut tx, token, Permission::Read).await?;
    // A fine-grained token counts public packages; its own owner's are listed on their page.
    let actor = if actor.owner.is_some() {
        Actor::ANONYMOUS
    } else {
        actor
    };
    let count = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM packages p LEFT JOIN repositories r ON r.id=p.repo_id LEFT JOIN users u ON u.id=$2 WHERE p.repo_id=$1 AND p.deleted_at IS NULL AND ($3::bigint IS NULL OR p.repo_id=$3) AND {}",read_condition(&actor)))
        .bind(repo).bind(actor.id).bind(actor.repo).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(count)
}

pub async fn create(
    pool: &PgPool,
    owner: &Owner,
    kind: &str,
    name: &str,
    token: i64,
    repo: Option<i64>,
) -> Result<Package> {
    let mut tx = begin(pool).await?;
    lock_package_key(&mut tx, &owner.kind, owner.id, kind, name).await?;
    let actor = actor(&mut tx, Some(token), Permission::Write).await?;
    let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM packages WHERE owner_kind=$1 AND COALESCE(owner_user_id,owner_org_id)=$2 AND name=$3 AND package_type=$4")
        .bind(&owner.kind).bind(owner.id).bind(name).bind(kind).fetch_optional(&mut *tx).await?;
    if let Some(id) = existing {
        let package = authorize(&mut tx, id, Some(token), Permission::Write, false).await?;
        tx.commit().await?;
        return Ok(package);
    }
    if actor.classic && owner.kind == "org" {
        let allowed: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM organization_token_policies WHERE org_id=$1 AND NOT classic_allowed)")
            .bind(owner.id).fetch_one(&mut *tx).await?;
        ensure!(allowed, Refusal::Forbidden);
    }
    let now = crate::packages::now();
    let candidate = Package {
        id: 0,
        owner_kind: owner.kind.clone(),
        owner_user_id: (owner.kind == "user").then_some(owner.id),
        owner_org_id: (owner.kind == "org").then_some(owner.id),
        name: name.into(),
        package_type: kind.into(),
        visibility: "private".into(),
        repo_id: repo,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    ensure!(actor.repo.is_none() || actor.repo == repo, Refusal::Missing);
    ensure!(
        actor
            .owner
            .is_none_or(|(kind, id)| kind == owner.kind && id == owner.id),
        Refusal::Forbidden
    );
    let user = actor.id.ok_or(Refusal::Unauthorized)?;
    let scoped: bool = sqlx::query_scalar(&format!(
        "SELECT {} FROM (SELECT $1::bigint AS owner_org_id,$2::bigint AS repo_id) p",
        actor.scope_condition()
    ))
    .bind(candidate.owner_org_id)
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(scoped, Refusal::Forbidden);
    let owner_access = owner_level(&mut tx, &candidate, user).await?;
    let mut allowed = owner_access >= 3;
    if let Some(repo) = repo {
        let locked = codeyard_db_core::queries::postgres::repositories::access::lock_user_repo_access_in_transaction(
            &mut tx,
            user,
            repo,
            codeyard_db_core::queries::postgres::repositories::access::RepositoryRowLock::Shared,
            false,
        )
        .await?
        .ok_or(Refusal::Missing)?;
        let repository: Option<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT owner_kind,owner_user_id,owner_org_id FROM repositories WHERE id=$1",
        )
        .bind(repo)
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(
            repository
                == Some((
                    candidate.owner_kind.clone(),
                    candidate.owner_user_id,
                    candidate.owner_org_id
                )),
            Refusal::Missing
        );
        let access = locked.access;
        ensure!(access.can_write(), Refusal::Forbidden);
        allowed |= access.can_write();
    }
    ensure!(allowed, Refusal::Forbidden);
    let id: Option<i64> = sqlx::query_scalar("INSERT INTO packages(owner_kind,owner_user_id,owner_org_id,name,package_type,repo_id,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$7) ON CONFLICT DO NOTHING RETURNING id")
        .bind(&candidate.owner_kind).bind(candidate.owner_user_id).bind(candidate.owner_org_id).bind(name).bind(kind).bind(repo).bind(now).fetch_optional(&mut *tx).await?;
    let id = match id { Some(id)=>id, None=>sqlx::query_scalar("SELECT id FROM packages WHERE owner_kind=$1 AND COALESCE(owner_user_id,owner_org_id)=$2 AND name=$3 AND package_type=$4")
        .bind(&owner.kind).bind(owner.id).bind(name).bind(kind).fetch_one(&mut *tx).await? };
    let result = authorize(&mut tx, id, Some(token), Permission::Write, false).await?;
    tx.commit().await?;
    Ok(result)
}

/// Count authorized package-download responses, not metadata probes or completed installs.
pub async fn nuget_downloaded(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
    version: i64,
) -> Result<()> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    let p = authorize(&mut tx, package, token, Permission::Read, false).await?;
    ensure!(p.package_type == "nuget", Refusal::Missing);
    let changed = sqlx::query("UPDATE package_versions SET downloads=downloads+1 WHERE id=$1 AND package_id=$2 AND deleted_at IS NULL").bind(version).bind(package).execute(&mut *tx).await?.rows_affected();
    ensure!(changed == 1, Refusal::Missing);
    tx.commit().await?;
    Ok(())
}

pub async fn versions(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
    deleted: bool,
    offset: i64,
    limit: i64,
) -> Result<Vec<Version>> {
    let mut tx = read(pool).await?;
    authorize(
        &mut tx,
        package,
        token,
        if deleted {
            Permission::Admin
        } else {
            Permission::Read
        },
        false,
    )
    .await?;
    let rows = sqlx::query_as("SELECT id,package_id,name,digest,media_type,'{}' AS manifest,downloads,created_at,updated_at,deleted_at FROM package_versions WHERE package_id=$1 AND ((deleted_at IS NOT NULL)=$2) ORDER BY id DESC LIMIT $3 OFFSET $4")
        .bind(package).bind(deleted).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn version_page(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
    before: i64,
    names_only: bool,
) -> Result<Vec<Version>> {
    let mut tx = read(pool).await?;
    let p = authorize(&mut tx, package, token, Permission::Read, false).await?;
    let metadata = if names_only {
        "'{}'"
    } else if p.package_type == "nuget" {
        "(manifest::jsonb-'nuspec')::text"
    } else {
        "manifest"
    };
    let rows = sqlx::query_as(&format!("SELECT id,package_id,name,digest,media_type,{metadata} AS manifest,downloads,created_at,updated_at,deleted_at FROM package_versions WHERE package_id=$1 AND deleted_at IS NULL AND id<$2 ORDER BY id DESC LIMIT 100"))
        .bind(package).bind(before).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn version(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
    reference: &str,
) -> Result<Version> {
    let mut tx = read(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let result = sqlx::query_as("SELECT v.* FROM package_versions v WHERE v.package_id=$1 AND v.deleted_at IS NULL AND (v.name=$2 OR EXISTS(SELECT 1 FROM package_tags t WHERE t.package_id=v.package_id AND t.version_id=v.id AND t.tag=$2)) ORDER BY (v.name=$2) DESC,v.id DESC LIMIT 1")
        .bind(package).bind(reference).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(result)
}

pub async fn version_by_id(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
    id: i64,
) -> Result<Version> {
    let mut tx = read(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let version = sqlx::query_as(
        "SELECT * FROM package_versions WHERE id=$1 AND package_id=$2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(package)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(version)
}

pub async fn version_counts(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, i64)>> {
    ensure!(ids.len() <= 100, "Package page exceeds its limit");
    Ok(sqlx::query_as("SELECT package_id,COUNT(*) FROM package_versions WHERE package_id=ANY($1) AND deleted_at IS NULL GROUP BY package_id").bind(ids).fetch_all(pool).await?)
}

pub async fn version_tags(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, String)>> {
    ensure!(ids.len() <= 100, "Version page exceeds its limit");
    Ok(sqlx::query_as(
        "SELECT version_id,tag FROM package_tags WHERE version_id=ANY($1) ORDER BY version_id,tag",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?)
}

pub async fn version_count(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
) -> Result<i64> {
    let mut tx = read(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let count = sqlx::query_scalar(
        "SELECT COUNT(*) FROM package_versions WHERE package_id=$1 AND deleted_at IS NULL",
    )
    .bind(package)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(count)
}

pub async fn tag_page(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
    after: &str,
    limit: i64,
) -> Result<Vec<(String, String)>> {
    let mut tx = read(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let tags = sqlx::query_as("SELECT t.tag,v.name FROM package_tags t JOIN package_versions v ON v.id=t.version_id AND v.package_id=t.package_id WHERE t.package_id=$1 AND v.deleted_at IS NULL AND t.tag>$2 ORDER BY t.tag LIMIT $3")
        .bind(package).bind(after).bind(limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(tags)
}

/// Points `tag` at the published version named `version`.
pub async fn set_tag(
    pool: &PgPool,
    package: i64,
    token: i64,
    tag: &str,
    version: &str,
) -> Result<()> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    authorize(&mut tx, package, Some(token), Permission::Write, false).await?;
    let changed = sqlx::query("INSERT INTO package_tags(package_id,tag,version_id) SELECT $1,$2,id FROM package_versions WHERE package_id=$1 AND name=$3 AND deleted_at IS NULL ON CONFLICT(package_id,tag) DO UPDATE SET version_id=excluded.version_id")
        .bind(package).bind(tag).bind(version).execute(&mut *tx).await?;
    ensure!(changed.rows_affected() == 1, Refusal::Missing);
    sqlx::query("UPDATE packages SET updated_at=$2 WHERE id=$1")
        .bind(package)
        .bind(crate::packages::now())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn delete_tag(pool: &PgPool, package: i64, token: i64, tag: &str) -> Result<()> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    authorize(&mut tx, package, Some(token), Permission::Write, false).await?;
    let changed = sqlx::query("DELETE FROM package_tags WHERE package_id=$1 AND tag=$2")
        .bind(package)
        .bind(tag)
        .execute(&mut *tx)
        .await?;
    ensure!(changed.rows_affected() == 1, Refusal::Missing);
    sqlx::query("UPDATE packages SET updated_at=$2 WHERE id=$1")
        .bind(package)
        .bind(crate::packages::now())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn rubygems_file(
    pool: &PgPool,
    owner: &Owner,
    token: Option<Credential<'_>>,
    filename: &str,
) -> Result<Blob> {
    ensure!(filename.len() <= 465, Refusal::Missing);
    let mut tx = read(pool).await?;
    let rows: Vec<(i64, String, i64)> = sqlx::query_as("SELECT p.id,b.digest,b.size FROM packages p JOIN package_versions v ON v.package_id=p.id JOIN package_blobs b ON b.digest=v.digest WHERE p.owner_kind=$1 AND COALESCE(p.owner_user_id,p.owner_org_id)=$2 AND p.package_type='rubygems' AND p.deleted_at IS NULL AND v.deleted_at IS NULL AND b.ready AND NOT b.deleting AND p.name||'-'||v.name=$3 LIMIT 2")
        .bind(&owner.kind).bind(owner.id).bind(filename).fetch_all(&mut *tx).await?;
    for (package, _, _) in &rows {
        authorize(&mut tx, *package, token, Permission::Read, false).await?;
    }
    ensure!(rows.len() <= 1, Refusal::Conflict);
    let (_, digest, size) = rows.into_iter().next().ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(Blob { digest, size })
}

pub async fn tags(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
    version: i64,
) -> Result<Vec<String>> {
    let mut tx = read(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let tags = sqlx::query_scalar(
        "SELECT tag FROM package_tags WHERE version_id=$1 AND package_id=$2 ORDER BY tag",
    )
    .bind(version)
    .bind(package)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(tags)
}

async fn check_blobs(tx: &mut Tx<'_>, package: i64, blobs: &[Blob], now: i64) -> Result<()> {
    let mut blobs: Vec<_> = blobs.iter().collect();
    blobs.sort_unstable_by(|a, b| a.digest.cmp(&b.digest));
    for blob in blobs {
        let _: Option<String> =
            sqlx::query_scalar("SELECT digest FROM package_blobs WHERE digest=$1 FOR UPDATE")
                .bind(&blob.digest)
                .fetch_optional(&mut **tx)
                .await?;
        let found: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM package_blobs b WHERE b.digest=$2 AND b.size=$3 AND b.ready AND NOT b.deleting AND (EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.package_id=$1 AND g.digest=b.digest AND g.expires_at>$4) OR EXISTS(SELECT 1 FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id WHERE v.package_id=$1 AND v.deleted_at IS NULL AND vb.digest=b.digest)))")
            .bind(package).bind(&blob.digest).bind(blob.size).bind(now).fetch_one(&mut **tx).await?;
        ensure!(found, Refusal::Blob);
    }
    Ok(())
}

pub async fn maven_file(
    pool: &PgPool,
    repo: i64,
    token: Option<Credential<'_>>,
    path: &str,
) -> Result<(Package, Blob)> {
    let mut tx = read(pool).await?;
    let package: i64 = sqlx::query_scalar("SELECT f.package_id FROM package_files f JOIN packages p ON p.id=f.package_id WHERE p.repo_id=$1 AND p.package_type='maven' AND f.path=$2").bind(repo).bind(path).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    let p = authorize(&mut tx, package, token, Permission::Read, false).await?;
    let blob = sqlx::query_as("SELECT b.digest,b.size FROM package_files f JOIN package_versions v ON v.id=f.version_id JOIN package_blobs b ON b.digest=f.digest WHERE f.package_id=$1 AND f.path=$2 AND v.deleted_at IS NULL AND b.ready AND NOT b.deleting")
        .bind(package).bind(path).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok((p, blob))
}

pub async fn put_maven_file(
    pool: &PgPool,
    token: i64,
    file: crate::packages::MavenFile<'_>,
) -> Result<()> {
    let mut tx = begin(pool).await?;
    // Maven index paths are shared by coordinates in the same repository.
    lock_key(&mut tx, &format!("maven:{}", file.repo_id)).await?;
    lock_package(&mut tx, file.package_id).await?;
    let p = authorize(
        &mut tx,
        file.package_id,
        Some(token),
        Permission::Write,
        false,
    )
    .await?;
    ensure!(
        p.package_type == "maven" && p.repo_id == Some(file.repo_id),
        Refusal::Missing
    );
    let now = crate::packages::now();
    // Index replacement can release older blobs shared with other repositories.
    sqlx::query("SELECT digest FROM package_blobs WHERE digest=$1 OR digest IN (SELECT vb.digest FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id JOIN packages p ON p.id=v.package_id WHERE p.repo_id=$2 AND p.package_type='maven') ORDER BY digest FOR UPDATE")
        .bind(&file.blob.digest).bind(p.repo_id).execute(&mut *tx).await?;
    check_blobs(&mut tx, p.id, std::slice::from_ref(file.blob), now).await?;
    let old: Option<String> =
        sqlx::query_scalar("SELECT digest FROM package_files WHERE package_id=$1 AND path=$2")
            .bind(p.id)
            .bind(file.path)
            .fetch_optional(&mut *tx)
            .await?;
    ensure!(
        file.mutable
            || old
                .as_ref()
                .is_none_or(|digest| digest == &file.blob.digest),
        Refusal::Conflict
    );
    let version: Version = if let Some(name) = file.version {
        let existing: Option<Version> =
            sqlx::query_as("SELECT * FROM package_versions WHERE package_id=$1 AND name=$2")
                .bind(p.id)
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(existing) = existing {
            ensure!(existing.deleted_at.is_none(), Refusal::Conflict);
            existing
        } else {
            sqlx::query_as("INSERT INTO package_versions(package_id,name,digest,media_type,manifest,created_at,updated_at) VALUES($1,$2,$3,'application/json',$4,$5,$5) RETURNING *")
                .bind(p.id).bind(name).bind(&file.blob.digest).bind(file.metadata).bind(now).fetch_one(&mut *tx).await?
        }
    } else {
        sqlx::query_as("SELECT * FROM package_versions WHERE package_id=$1 AND deleted_at IS NULL ORDER BY id DESC LIMIT 1").bind(p.id).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?
    };
    // Repository metadata paths are shared by plugin coordinates in the same group.
    sqlx::query("DELETE FROM package_files WHERE path=$1 AND package_id<>$2 AND package_id IN (SELECT id FROM packages WHERE repo_id=$3 AND package_type='maven')").bind(file.path).bind(p.id).bind(p.repo_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO package_files(package_id,path,version_id,digest) VALUES($1,$2,$3,$4) ON CONFLICT(package_id,path) DO UPDATE SET version_id=excluded.version_id,digest=excluded.digest")
        .bind(p.id).bind(file.path).bind(version.id).bind(&file.blob.digest).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO package_version_blobs(version_id,digest) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(version.id)
    .bind(&file.blob.digest)
    .execute(&mut *tx)
    .await?;
    // Keep snapshot-referenced bytes available to an online backup after their final reference goes.
    sqlx::query("UPDATE package_blobs SET expires_at=GREATEST(expires_at,$2) WHERE digest IN (SELECT vb.digest FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id JOIN packages p ON p.id=v.package_id WHERE p.repo_id=$1 AND p.package_type='maven' AND NOT EXISTS(SELECT 1 FROM package_files f WHERE f.version_id=vb.version_id AND f.digest=vb.digest))")
        .bind(p.repo_id).bind(now + codeyard_db_core::queries::files::RESTORE_WINDOW_SECONDS).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM package_version_blobs WHERE version_id IN (SELECT v.id FROM package_versions v JOIN packages p ON p.id=v.package_id WHERE p.repo_id=$1 AND p.package_type='maven') AND NOT EXISTS(SELECT 1 FROM package_files f WHERE f.version_id=package_version_blobs.version_id AND f.digest=package_version_blobs.digest)").bind(p.repo_id).execute(&mut *tx).await?;
    // Repository/group index updates do not replace the version's artifact digest.
    sqlx::query("UPDATE package_versions SET digest=CASE WHEN $4 THEN digest ELSE $2 END,updated_at=$3 WHERE id=$1")
        .bind(version.id)
        .bind(&file.blob.digest)
        .bind(now)
        .bind(file.path.ends_with("/maven-metadata.xml"))
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE packages SET updated_at=$2 WHERE id=$1")
        .bind(p.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn publish(pool: &PgPool, token: i64, new: Publish<'_>) -> Result<Version> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, new.package_id).await?;
    let package = authorize(
        &mut tx,
        new.package_id,
        Some(token),
        Permission::Write,
        false,
    )
    .await?;
    ensure!(
        crate::packages::listed_metadata_fits(&package.package_type, new.manifest),
        Refusal::Conflict
    );
    if package.package_type == "rubygems" {
        lock_key(
            &mut tx,
            &serde_json::to_string(&(
                "rubygems-file",
                &package.owner_kind,
                package.owner_user_id.or(package.owner_org_id),
                format!("{}-{}", package.name, new.name),
            ))?,
        )
        .await?;
        let collision: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM packages p JOIN package_versions v ON v.package_id=p.id WHERE p.owner_kind=$1 AND COALESCE(p.owner_user_id,p.owner_org_id)=$2 AND p.package_type='rubygems' AND p.id<>$3 AND p.name||'-'||v.name=$4)")
            .bind(&package.owner_kind).bind(package.owner_user_id.or(package.owner_org_id)).bind(package.id).bind(format!("{}-{}", package.name, new.name)).fetch_one(&mut *tx).await?;
        ensure!(!collision, Refusal::Conflict);
    }
    let now = crate::packages::now();
    check_blobs(&mut tx, package.id, new.blobs, now).await?;
    let existing: Option<Version> =
        sqlx::query_as("SELECT * FROM package_versions WHERE package_id=$1 AND name=$2")
            .bind(package.id)
            .bind(new.name)
            .fetch_optional(&mut *tx)
            .await?;
    let result = if let Some(version) = existing {
        ensure!(
            package.package_type == "container"
                && version.digest == new.digest
                && version.deleted_at.is_none(),
            Refusal::Conflict
        );
        version
    } else {
        sqlx::query_as("INSERT INTO package_versions(package_id,name,digest,media_type,manifest,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,$6) RETURNING *")
            .bind(package.id).bind(new.name).bind(new.digest).bind(new.media_type).bind(new.manifest).bind(now).fetch_one(&mut *tx).await?
    };
    for blob in new.blobs {
        sqlx::query("INSERT INTO package_version_blobs(version_id,digest) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(result.id).bind(&blob.digest).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE package_blob_grants SET expires_at=$3 WHERE package_id=$1 AND digest=$2",
        )
        .bind(package.id)
        .bind(&blob.digest)
        .bind(now + 86400)
        .execute(&mut *tx)
        .await?;
    }
    // An index retains the child manifests' layers even if those versions are later deleted.
    if package.package_type == "container" {
        for blob in new.blobs {
            sqlx::query("INSERT INTO package_version_blobs(version_id,digest) SELECT $1,vb.digest FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id WHERE v.package_id=$2 AND v.digest=$3 AND v.id<>$1 AND v.deleted_at IS NULL ON CONFLICT DO NOTHING")
                .bind(result.id).bind(package.id).bind(&blob.digest).execute(&mut *tx).await?;
        }
    }
    for tag in new.tags {
        sqlx::query("INSERT INTO package_tags(package_id,tag,version_id) VALUES($1,$2,$3) ON CONFLICT(package_id,tag) DO UPDATE SET version_id=excluded.version_id")
            .bind(package.id).bind(tag).bind(result.id).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE packages SET updated_at=$2 WHERE id=$1")
        .bind(package.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn set_deleted(
    pool: &PgPool,
    package: i64,
    version: Option<i64>,
    token: Credential<'_>,
    restore: bool,
) -> Result<()> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    let p = authorize(&mut tx, package, Some(token), Permission::Admin, true).await?;
    let now = crate::packages::now();
    if let Some(version) = version {
        ensure!(p.deleted_at.is_none(), Refusal::Missing);
        let deleted: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT deleted_at FROM package_versions WHERE id=$1 AND package_id=$2",
        )
        .bind(version)
        .bind(package)
        .fetch_optional(&mut *tx)
        .await?;
        let deleted = deleted.ok_or(Refusal::Missing)?;
        ensure!(
            if restore {
                deleted.is_some_and(|at| at > now - 30 * 86400)
            } else {
                deleted.is_none()
            },
            Refusal::Missing
        );
        sqlx::query(
            "UPDATE package_versions SET deleted_at=$3,updated_at=$4 WHERE id=$1 AND package_id=$2",
        )
        .bind(version)
        .bind(package)
        .bind(if restore { None } else { Some(now) })
        .bind(now)
        .execute(&mut *tx)
        .await?;
    } else {
        ensure!(
            if restore {
                p.deleted_at.is_some_and(|at| at > now - 30 * 86400)
            } else {
                p.deleted_at.is_none()
            },
            Refusal::Missing
        );
        sqlx::query("UPDATE packages SET deleted_at=$2,updated_at=$3 WHERE id=$1")
            .bind(package)
            .bind(if restore { None } else { Some(now) })
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn settings(
    pool: &PgPool,
    package: i64,
    token: Credential<'_>,
    visibility: &str,
    repo: Option<i64>,
) -> Result<()> {
    ensure!(
        matches!(visibility, "private" | "public"),
        Refusal::Conflict
    );
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    let current = authorize(&mut tx, package, Some(token), Permission::Admin, false).await?;
    if repo != current.repo_id {
        if let Some(repo) = repo {
            let actor = actor(&mut tx, Some(token), Permission::Admin).await?;
            let user = actor.id.ok_or(Refusal::Unauthorized)?;
            let locked = codeyard_db_core::queries::postgres::repositories::access::lock_user_repo_access_in_transaction(
                &mut tx,
                user,
                repo,
                codeyard_db_core::queries::postgres::repositories::access::RepositoryRowLock::Shared,
                false,
            )
            .await?
            .ok_or(Refusal::Missing)?;
            let target: Option<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
                "SELECT owner_kind,owner_user_id,owner_org_id FROM repositories WHERE id=$1",
            )
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
            ensure!(
                target
                    == Some((
                        current.owner_kind,
                        current.owner_user_id,
                        current.owner_org_id
                    )),
                Refusal::Missing
            );
            ensure!(locked.access.can_admin(), Refusal::Forbidden);
        }
    }
    sqlx::query("UPDATE packages SET visibility=$2,updated_at=$3,repo_id=$4 WHERE id=$1")
        .bind(package)
        .bind(visibility)
        .bind(crate::packages::now())
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn reserve_blob(
    pool: &PgPool,
    package: i64,
    token: i64,
    blob: &Blob,
    charge: Charge<'_>,
) -> Result<bool> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    authorize(&mut tx, package, Some(token), Permission::Write, false).await?;
    let actor = actor(&mut tx, Some(token), Permission::Write).await?;
    let user = actor.id.ok_or(Refusal::Unauthorized)?;
    let now = crate::packages::now();
    match charge {
        Charge::Upload => {
            crate::postgres::files::charge_upload_in_transaction(
                &mut tx,
                user,
                "package",
                blob.size.max(1),
                None,
            )
            .await?;
        }
        Charge::Chunk(upload) => {
            let earlier: Option<uuid::Uuid> = sqlx::query_scalar("SELECT charge_id FROM package_uploads WHERE id=$1 AND package_id=$2 AND token_id=$3 AND expires_at>$4 FOR UPDATE")
                .bind(upload).bind(package).bind(token).bind(now).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
            let charge = crate::postgres::files::charge_upload_in_transaction(
                &mut tx,
                user,
                "package",
                blob.size.max(1),
                earlier,
            )
            .await?;
            sqlx::query("UPDATE package_uploads SET charge_id=$2 WHERE id=$1")
                .bind(upload)
                .bind(charge)
                .execute(&mut *tx)
                .await?;
        }
        Charge::Assembled => {}
    }
    sqlx::query("INSERT INTO package_blobs(digest,size,expires_at) VALUES($1,$2,$3) ON CONFLICT(digest) DO NOTHING")
        .bind(&blob.digest).bind(blob.size).bind(now + codeyard_db_core::queries::files::RESTORE_WINDOW_SECONDS).execute(&mut *tx).await?;
    let (size, ready, deleting): (i64, bool, bool) =
        sqlx::query_as("SELECT size,ready,deleting FROM package_blobs WHERE digest=$1 FOR UPDATE")
            .bind(&blob.digest)
            .fetch_one(&mut *tx)
            .await?;
    ensure!(size == blob.size && !deleting, Refusal::Blob);
    sqlx::query("UPDATE package_blobs SET expires_at=GREATEST(expires_at,$2) WHERE digest=$1")
        .bind(&blob.digest)
        .bind(now + codeyard_db_core::queries::files::RESTORE_WINDOW_SECONDS)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO package_blob_grants(package_id,digest,expires_at) VALUES($1,$2,$3) ON CONFLICT(package_id,digest) DO UPDATE SET expires_at=excluded.expires_at")
        .bind(package).bind(&blob.digest).bind(now+86400).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(ready)
}

pub async fn ready_blob(pool: &PgPool, package: i64, token: i64, blob: &Blob) -> Result<()> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    authorize(&mut tx, package, Some(token), Permission::Write, false).await?;
    let changed = sqlx::query("UPDATE package_blobs SET ready=true WHERE digest=$1 AND size=$2 AND NOT deleting AND expires_at>$3 AND EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.package_id=$4 AND g.digest=package_blobs.digest)")
        .bind(&blob.digest).bind(blob.size).bind(crate::packages::now()).bind(package).execute(&mut *tx).await?;
    ensure!(changed.rows_affected() == 1, Refusal::Blob);
    tx.commit().await?;
    Ok(())
}

pub async fn blob(
    pool: &PgPool,
    package: i64,
    token: Option<Credential<'_>>,
    digest: &str,
) -> Result<Blob> {
    let mut tx = read(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let result = sqlx::query_as("SELECT b.digest,b.size FROM package_blobs b WHERE b.digest=$2 AND b.ready AND NOT b.deleting AND (EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.package_id=$1 AND g.digest=b.digest AND g.expires_at>$3) OR EXISTS(SELECT 1 FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id WHERE v.package_id=$1 AND v.deleted_at IS NULL AND vb.digest=b.digest))")
        .bind(package).bind(digest).bind(crate::packages::now()).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(result)
}

pub async fn start_upload(pool: &PgPool, package: i64, token: i64, id: &str) -> Result<Upload> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    authorize(&mut tx, package, Some(token), Permission::Write, false).await?;
    let result = sqlx::query_as("INSERT INTO package_uploads(id,package_id,token_id,expires_at) VALUES($1,$2,$3,$4) RETURNING *")
        .bind(id).bind(package).bind(token).bind(crate::packages::now()+3600).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}

async fn get_upload(tx: &mut Tx<'_>, package: i64, token: i64, id: &str) -> Result<Upload> {
    authorize(tx, package, Some(token), Permission::Write, false).await?;
    Ok(sqlx::query_as("SELECT * FROM package_uploads WHERE id=$1 AND package_id=$2 AND token_id=$3 AND expires_at>$4")
        .bind(id).bind(package).bind(token).bind(crate::packages::now()).fetch_optional(&mut **tx).await?.ok_or(Refusal::Missing)?)
}

pub async fn upload(pool: &PgPool, package: i64, token: i64, id: &str) -> Result<Upload> {
    let mut tx = read(pool).await?;
    let result = get_upload(&mut tx, package, token, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn append_upload(
    pool: &PgPool,
    package: i64,
    token: i64,
    id: &str,
    offset: i64,
    chunk: &Blob,
) -> Result<Upload> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    let mut upload = get_upload(&mut tx, package, token, id).await?;
    ensure!(upload.offset == offset, Refusal::Offset);
    let mut chunks: Vec<Blob> = serde_json::from_str(&upload.chunks)?;
    ensure!(chunks.len() < 101, Refusal::Offset);
    chunks.push(chunk.clone());
    upload.offset += chunk.size;
    upload.chunks = serde_json::to_string(&chunks)?;
    sqlx::query("UPDATE package_uploads SET uploaded_bytes=$2,chunks=$3 WHERE id=$1")
        .bind(id)
        .bind(upload.offset)
        .bind(&upload.chunks)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(upload)
}

pub async fn cancel_upload(pool: &PgPool, package: i64, token: i64, id: &str) -> Result<()> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    get_upload(&mut tx, package, token, id).await?;
    sqlx::query("DELETE FROM package_uploads WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn finish_upload(
    pool: &PgPool,
    package: i64,
    token: i64,
    id: &str,
    offset: i64,
) -> Result<()> {
    let mut tx = begin(pool).await?;
    lock_package(&mut tx, package).await?;
    let upload = get_upload(&mut tx, package, token, id).await?;
    ensure!(upload.offset == offset, Refusal::Offset);
    sqlx::query("DELETE FROM package_uploads WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn garbage(pool: &PgPool) -> Result<Vec<String>> {
    let mut tx = begin(pool).await?;
    let now = crate::packages::now();
    let expired: Vec<i64> = sqlx::query_scalar("SELECT id FROM packages WHERE deleted_at<=$1 OR id IN (SELECT package_id FROM package_versions WHERE deleted_at<=$1 ORDER BY id LIMIT 100) ORDER BY id LIMIT 200")
        .bind(now - 30 * 86400).fetch_all(&mut *tx).await?;
    for id in &expired {
        lock_package(&mut tx, *id).await?;
    }
    sqlx::query("DELETE FROM package_registry_tokens WHERE expires_at<=$1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM package_uploads WHERE expires_at<=$1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT digest FROM package_blobs WHERE digest IN (SELECT vb.digest FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id JOIN packages p ON p.id=v.package_id WHERE p.id=ANY($2) AND (v.deleted_at<=$1 OR p.deleted_at<=$1)) ORDER BY digest FOR UPDATE")
        .bind(now - 30 * 86400).bind(&expired).execute(&mut *tx).await?;
    sqlx::query("UPDATE package_blobs SET expires_at=GREATEST(expires_at,$2) WHERE digest IN (SELECT vb.digest FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id JOIN packages p ON p.id=v.package_id WHERE p.id=ANY($3) AND (v.deleted_at<=$1 OR p.deleted_at<=$1))")
        .bind(now - 30 * 86400).bind(now + codeyard_db_core::queries::files::RESTORE_WINDOW_SECONDS).bind(&expired).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM packages WHERE id=ANY($2) AND deleted_at<=$1")
        .bind(now - 30 * 86400)
        .bind(&expired)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM package_versions WHERE id IN (SELECT id FROM package_versions WHERE package_id=ANY($2) AND deleted_at<=$1 ORDER BY id LIMIT 100)").bind(now-30*86400).bind(&expired).execute(&mut *tx).await?;
    // Lock bytes before checking references again, after any publisher has committed.
    let blobs: Vec<String> = sqlx::query_scalar("SELECT b.digest FROM package_blobs b WHERE b.expires_at<=$1 AND NOT b.deleting AND NOT EXISTS(SELECT 1 FROM package_version_blobs vb WHERE vb.digest=b.digest) AND NOT EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.digest=b.digest AND g.expires_at>$1) ORDER BY b.digest LIMIT 100 FOR UPDATE OF b SKIP LOCKED")
        .bind(now).fetch_all(&mut *tx).await?;
    sqlx::query("UPDATE package_blobs b SET deleting=true WHERE digest=ANY($2) AND expires_at<=$1 AND NOT EXISTS(SELECT 1 FROM package_version_blobs vb WHERE vb.digest=b.digest) AND NOT EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.digest=b.digest AND g.expires_at>$1)")
        .bind(now).bind(&blobs).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM package_blob_grants WHERE expires_at<=$1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query_scalar("SELECT digest FROM package_blobs WHERE deleting LIMIT 100")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn issue_registry_token(
    pool: &PgPool,
    hash: &str,
    token: Option<i64>,
    resources: &str,
    write: bool,
) -> Result<()> {
    let mut tx = begin(pool).await?;
    ensure!(token.is_some() || !write, Refusal::Unauthorized);
    actor(
        &mut tx,
        token,
        if write {
            Permission::Write
        } else {
            Permission::Read
        },
    )
    .await?;
    sqlx::query("INSERT INTO package_registry_tokens(hash,token_id,resources,write,expires_at) VALUES($1,$2,$3,$4,$5)")
        .bind(hash).bind(token).bind(resources).bind(write).bind(crate::packages::now()+300).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn registry_token(
    pool: &PgPool,
    hash: &str,
) -> Result<Option<crate::packages::RegistryToken>> {
    Ok(sqlx::query_as("SELECT token_id,resources,write,expires_at FROM package_registry_tokens WHERE hash=$1 AND expires_at>$2")
        .bind(hash).bind(crate::packages::now()).fetch_optional(pool).await?)
}

pub async fn registry_source(
    pool: &PgPool,
    id: i64,
) -> Result<Option<codeyard_db_core::models::PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at FROM personal_access_tokens WHERE id=$1").bind(id).fetch_optional(pool).await?)
}

/// A connection that holds the session lock of package collection, or `None` when another node
/// holds it. The lock is keyed by the schema, so installations sharing a database collect on
/// their own, and it ends with the connection.
pub async fn collector_lock(pool: &PgPool) -> Result<Option<sqlx::PgConnection>> {
    let mut connection = codeyard_db_core::connection::session_connection(pool).await?;
    let taken: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_lock(hashtext(COALESCE(current_schema(),'')), 723947123)",
    )
    .fetch_one(&mut connection)
    .await?;
    Ok(taken.then_some(connection))
}

pub async fn removed_blob(pool: &PgPool, digest: &str) -> Result<()> {
    sqlx::query("DELETE FROM package_blobs WHERE digest=$1 AND deleting")
        .bind(digest)
        .execute(pool)
        .await?;
    Ok(())
}
