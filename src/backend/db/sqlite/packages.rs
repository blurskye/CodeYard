use crate::backend::db::packages::{
    self, Blob, Credential, Listing, Owner, Package, Permission, Publish, Refusal, Upload, Version,
};
use anyhow::{ensure, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};

type Tx<'a> = Transaction<'a, Sqlite>;

async fn begin(pool: &SqlitePool) -> Result<Tx<'_>> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

struct Actor {
    id: Option<i64>,
    repo: Option<i64>,
    /// A fine-grained token acts only on its resource owner's packages, as `(kind, id)`.
    owner: Option<(&'static str, i64)>,
}

impl Actor {
    const ANONYMOUS: Self = Self {
        id: None,
        repo: None,
        owner: None,
    };
    /// A fine-grained token reads another owner's packages as an anonymous request would.
    fn for_owner(self, kind: &str, id: Option<i64>) -> Self {
        match self.owner {
            Some((owner_kind, owner)) if owner_kind != kind || Some(owner) != id => Self::ANONYMOUS,
            _ => self,
        }
    }
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
            let id: Option<i64> = sqlx::query_scalar("SELECT u.id FROM users u JOIN sessions s ON s.user_id=u.id WHERE s.id=$1 AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT u.is_banned AND NOT u.must_change_password")
                .bind(session).fetch_optional(&mut **tx).await?;
            return Ok(Actor {
                id: Some(id.ok_or(Refusal::Unauthorized)?),
                repo: None,
                owner: None,
            });
        }
    };
    let row: Option<(i64, String, String)> = sqlx::query_as("SELECT u.id,u.username,t.scope FROM users u JOIN personal_access_tokens t ON t.user_id=u.id WHERE t.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))")
        .bind(token).fetch_optional(&mut **tx).await?;
    let (id, name, scope) = row.ok_or(Refusal::Unauthorized)?;
    // These credentials have no package grant; do not collapse them into account-wide identity.
    let integration:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_app_tokens WHERE token_id=$1) OR EXISTS(SELECT 1 FROM github_app_tokens WHERE token_id=$1)").bind(token).fetch_one(&mut **tx).await?;
    ensure!(!integration, Refusal::Forbidden);
    let write = !matches!(permission, Permission::Read);
    ensure!(!write || scope == "write", Refusal::Forbidden);
    let mut owner = None;
    let bounds: Option<crate::backend::db::fine_grained_tokens::Bounds> =
        sqlx::query_as(super::fine_grained_tokens::BOUNDS)
            .bind(token)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(bounds) = bounds {
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
    if name == crate::backend::db::ci_tokens::BOT {
        let job: Option<(i64, String)> = sqlx::query_as("SELECT r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=$1 AND (t.revoked_at IS NULL OR t.revoked_at>CAST(strftime('%s','now') AS INTEGER)-60) AND (r.status='running' OR r.completed_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 minute'))")
            .bind(token).fetch_optional(&mut **tx).await?;
        let (repo_id, permissions) = job.ok_or(Refusal::Unauthorized)?;
        let permissions: crate::backend::ci::tokens::Permissions =
            serde_json::from_str(&permissions)?;
        ensure!(permissions.allows("packages", write), Refusal::Forbidden);
        repo = Some(repo_id);
    }
    Ok(Actor {
        id: Some(id),
        repo,
        owner,
    })
}

fn read_condition() -> String {
    // Public repository access alone does not make a private package public.
    let access = super::repositories::access::USER_REPO_ACCESS_SQL
        .replace("CASE WHEN NOT r.is_private THEN 1 ELSE 0 END", "0");
    format!("((p.visibility='public' AND (p.repo_id IS NULL OR NOT r.is_private)) OR p.owner_user_id=u.id OR (p.owner_kind='user' AND u.is_admin) OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=p.owner_org_id AND m.user_id=u.id AND m.role IN ('owner','admin')) OR (p.repo_id IS NOT NULL AND r.owner_kind=p.owner_kind AND COALESCE(r.owner_user_id,r.owner_org_id)=COALESCE(p.owner_user_id,p.owner_org_id) AND ({access})>=1))")
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
    let level: Option<i64> = sqlx::query_scalar("SELECT CASE m.role WHEN 'owner' THEN 6 WHEN 'admin' THEN 5 WHEN 'member' THEN CASE WHEN $3 THEN 1 WHEN $4 THEN 3 ELSE 0 END ELSE 0 END FROM org_memberships m WHERE m.org_id=$1 AND m.user_id=$2 AND EXISTS(SELECT 1 FROM org_memberships own JOIN users u ON u.id=own.user_id WHERE own.org_id=m.org_id AND own.role='owner' AND NOT u.is_banned AND NOT u.must_change_password)")
        .bind(package.owner_org_id).bind(user).bind(package.repo_id.is_some()).bind(package.visibility == "public").fetch_optional(&mut **tx).await?;
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
    let readable: bool = sqlx::query_scalar(&format!("SELECT COALESCE({},false) FROM packages p LEFT JOIN repositories r ON r.id=p.repo_id LEFT JOIN users u ON u.id=$2 WHERE p.id=$1", read_condition()))
        .bind(id).bind(actor.id).fetch_one(&mut **tx).await?;
    ensure!(readable, Refusal::Missing);
    if !matches!(permission, Permission::Read) {
        let user = actor.id.ok_or(Refusal::Unauthorized)?;
        let mut level = owner_level(tx, &package, user).await?;
        if let Some(repo) = package.repo_id {
            let archived: bool =
                sqlx::query_scalar("SELECT is_archived FROM repositories WHERE id=$1")
                    .bind(repo)
                    .fetch_one(&mut **tx)
                    .await?;
            ensure!(
                !archived || matches!(permission, Permission::Admin),
                Refusal::Missing
            );
            let access =
                super::repositories::access::user_repo_access_in_transaction(&mut *tx, user, repo)
                    .await?;
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

pub async fn owner(pool: &SqlitePool, name: &str, kind: Option<&str>) -> Result<Option<Owner>> {
    Ok(sqlx::query_as("SELECT id,username AS name,'user' AS kind FROM users WHERE username=$1 AND ($2 IS NULL OR $2='user') UNION ALL SELECT id,name,'org' AS kind FROM organizations WHERE name=$1 AND ($2 IS NULL OR $2='org')")
        .bind(name).bind(kind).fetch_optional(pool).await?)
}

pub async fn find(
    pool: &SqlitePool,
    owner: &Owner,
    kind: &str,
    name: &str,
    token: Option<Credential<'_>>,
    permission: Permission,
    deleted: bool,
) -> Result<Package> {
    let mut tx = begin(pool).await?;
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
    pool: &SqlitePool,
    token: Option<Credential<'_>>,
    query: Listing<'_>,
) -> Result<Vec<Package>> {
    let mut tx = begin(pool).await?;
    let actor = actor(&mut tx, token, Permission::Read).await?;
    let actor = match query.owner {
        Some(owner) => actor.for_owner(&owner.kind, Some(owner.id)),
        None if actor.owner.is_some() => Actor::ANONYMOUS,
        None => actor,
    };
    let rows = sqlx::query_as(&format!("SELECT p.* FROM packages p LEFT JOIN repositories r ON r.id=p.repo_id LEFT JOIN users u ON u.id=$5 WHERE ($1 IS NULL OR p.owner_kind=$1) AND ($2 IS NULL OR COALESCE(p.owner_user_id,p.owner_org_id)=$2) AND ($3 IS NULL OR p.package_type=$3) AND p.deleted_at IS NULL AND ($4 IS NULL OR p.visibility=$4) AND ($6 IS NULL OR (p.repo_id=$6 AND r.owner_kind=p.owner_kind AND COALESCE(r.owner_user_id,r.owner_org_id)=COALESCE(p.owner_user_id,p.owner_org_id))) AND ($9 IS NULL OR p.repo_id=$9) AND {} ORDER BY p.id DESC LIMIT $7 OFFSET $8",read_condition()))
        .bind(query.owner.map(|o| o.kind.as_str())).bind(query.owner.map(|o| o.id)).bind(query.kind).bind(query.visibility).bind(actor.id).bind(actor.repo).bind(query.limit).bind(query.offset).bind(query.repo).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn count_linked(
    pool: &SqlitePool,
    token: Option<Credential<'_>>,
    repo: i64,
) -> Result<i64> {
    let mut tx = begin(pool).await?;
    let actor = actor(&mut tx, token, Permission::Read).await?;
    // A fine-grained token counts public packages; its own owner's are listed on their page.
    let actor = if actor.owner.is_some() {
        Actor::ANONYMOUS
    } else {
        actor
    };
    let count = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM packages p LEFT JOIN repositories r ON r.id=p.repo_id LEFT JOIN users u ON u.id=$2 WHERE p.repo_id=$1 AND p.deleted_at IS NULL AND ($3 IS NULL OR p.repo_id=$3) AND {}",read_condition()))
        .bind(repo).bind(actor.id).bind(actor.repo).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(count)
}

pub async fn create(
    pool: &SqlitePool,
    owner: &Owner,
    kind: &str,
    name: &str,
    token: i64,
    repo: Option<i64>,
) -> Result<Package> {
    let mut tx = begin(pool).await?;
    let actor = actor(&mut tx, Some(token), Permission::Write).await?;
    let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM packages WHERE owner_kind=$1 AND COALESCE(owner_user_id,owner_org_id)=$2 AND name=$3 AND package_type=$4")
        .bind(&owner.kind).bind(owner.id).bind(name).bind(kind).fetch_optional(&mut *tx).await?;
    if let Some(id) = existing {
        let package = authorize(&mut tx, id, Some(token), Permission::Write, false).await?;
        tx.commit().await?;
        return Ok(package);
    }
    let now = packages::now();
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
    let owner_access = owner_level(&mut tx, &candidate, user).await?;
    let mut allowed = owner_access >= 3;
    if let Some(repo) = repo {
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
        let access =
            super::repositories::access::user_repo_access_in_transaction(&mut tx, user, repo)
                .await?;
        ensure!(access.can_write(), Refusal::Forbidden);
        let archived: bool = sqlx::query_scalar("SELECT is_archived FROM repositories WHERE id=$1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
        ensure!(!archived, Refusal::Missing);
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
    pool: &SqlitePool,
    package: i64,
    token: Option<Credential<'_>>,
    version: i64,
) -> Result<()> {
    let mut tx = begin(pool).await?;
    let p = authorize(&mut tx, package, token, Permission::Read, false).await?;
    ensure!(p.package_type == "nuget", Refusal::Missing);
    let changed = sqlx::query("UPDATE package_versions SET downloads=downloads+1 WHERE id=$1 AND package_id=$2 AND deleted_at IS NULL").bind(version).bind(package).execute(&mut *tx).await?.rows_affected();
    ensure!(changed == 1, Refusal::Missing);
    tx.commit().await?;
    Ok(())
}

pub async fn versions(
    pool: &SqlitePool,
    package: i64,
    token: Option<Credential<'_>>,
    deleted: bool,
    offset: i64,
    limit: i64,
) -> Result<Vec<Version>> {
    let mut tx = begin(pool).await?;
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
    let rows = sqlx::query_as("SELECT * FROM package_versions WHERE package_id=$1 AND ((deleted_at IS NOT NULL)=$2) ORDER BY id DESC LIMIT $3 OFFSET $4")
        .bind(package).bind(deleted).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn version(
    pool: &SqlitePool,
    package: i64,
    token: Option<Credential<'_>>,
    reference: &str,
) -> Result<Version> {
    let mut tx = begin(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let result = sqlx::query_as("SELECT v.* FROM package_versions v WHERE v.package_id=$1 AND v.deleted_at IS NULL AND (v.name=$2 OR EXISTS(SELECT 1 FROM package_tags t WHERE t.package_id=v.package_id AND t.version_id=v.id AND t.tag=$2)) ORDER BY v.id DESC LIMIT 1")
        .bind(package).bind(reference).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(result)
}

pub async fn version_by_id(
    pool: &SqlitePool,
    package: i64,
    token: Option<Credential<'_>>,
    id: i64,
) -> Result<Version> {
    let mut tx = begin(pool).await?;
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

pub async fn version_count(pool: &SqlitePool, package: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM package_versions WHERE package_id=$1 AND deleted_at IS NULL",
    )
    .bind(package)
    .fetch_one(pool)
    .await?)
}

pub async fn tag_page(
    pool: &SqlitePool,
    package: i64,
    token: Option<Credential<'_>>,
    after: &str,
    limit: i64,
) -> Result<Vec<String>> {
    let mut tx = begin(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let tags = sqlx::query_scalar("SELECT t.tag FROM package_tags t JOIN package_versions v ON v.id=t.version_id AND v.package_id=t.package_id WHERE t.package_id=$1 AND v.deleted_at IS NULL AND t.tag>$2 ORDER BY t.tag LIMIT $3")
        .bind(package).bind(after).bind(limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(tags)
}

pub async fn tags(pool: &SqlitePool, version: i64) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT tag FROM package_tags WHERE version_id=$1 ORDER BY tag")
            .bind(version)
            .fetch_all(pool)
            .await?,
    )
}

async fn check_blobs(tx: &mut Tx<'_>, package: i64, blobs: &[Blob], now: i64) -> Result<()> {
    for blob in blobs {
        let found: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM package_blobs b WHERE b.digest=$2 AND b.size=$3 AND b.ready AND NOT b.deleting AND (EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.package_id=$1 AND g.digest=b.digest AND g.expires_at>$4) OR EXISTS(SELECT 1 FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id WHERE v.package_id=$1 AND v.deleted_at IS NULL AND vb.digest=b.digest)))")
            .bind(package).bind(&blob.digest).bind(blob.size).bind(now).fetch_one(&mut **tx).await?;
        ensure!(found, Refusal::Blob);
    }
    Ok(())
}

pub async fn maven_file(
    pool: &SqlitePool,
    repo: i64,
    token: Option<Credential<'_>>,
    path: &str,
) -> Result<(Package, Blob)> {
    let mut tx = begin(pool).await?;
    let package: i64 = sqlx::query_scalar("SELECT f.package_id FROM package_files f JOIN packages p ON p.id=f.package_id WHERE p.repo_id=$1 AND p.package_type='maven' AND f.path=$2").bind(repo).bind(path).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    let p = authorize(&mut tx, package, token, Permission::Read, false).await?;
    let blob = sqlx::query_as("SELECT b.digest,b.size FROM package_files f JOIN package_versions v ON v.id=f.version_id JOIN package_blobs b ON b.digest=f.digest WHERE f.package_id=$1 AND f.path=$2 AND v.deleted_at IS NULL AND b.ready AND NOT b.deleting")
        .bind(package).bind(path).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok((p, blob))
}

pub async fn put_maven_file(
    pool: &SqlitePool,
    token: i64,
    file: packages::MavenFile<'_>,
) -> Result<()> {
    let mut tx = begin(pool).await?;
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
    let now = packages::now();
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
    // Replaced SNAPSHOT files are collectible; deleted versions retain their own files for recovery.
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

pub async fn publish(pool: &SqlitePool, token: i64, new: Publish<'_>) -> Result<Version> {
    let mut tx = begin(pool).await?;
    let package = authorize(
        &mut tx,
        new.package_id,
        Some(token),
        Permission::Write,
        false,
    )
    .await?;
    let now = packages::now();
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
    pool: &SqlitePool,
    package: i64,
    version: Option<i64>,
    token: Credential<'_>,
    restore: bool,
) -> Result<()> {
    let mut tx = begin(pool).await?;
    let p = authorize(&mut tx, package, Some(token), Permission::Admin, true).await?;
    let now = packages::now();
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
    pool: &SqlitePool,
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
    let current = authorize(&mut tx, package, Some(token), Permission::Admin, false).await?;
    if repo != current.repo_id {
        if let Some(repo) = repo {
            let actor = actor(&mut tx, Some(token), Permission::Admin).await?;
            let user = actor.id.ok_or(Refusal::Unauthorized)?;
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
            let access =
                super::repositories::access::user_repo_access_in_transaction(&mut tx, user, repo)
                    .await?;
            ensure!(access.can_admin(), Refusal::Forbidden);
        }
    }
    sqlx::query("UPDATE packages SET visibility=$2,updated_at=$3,repo_id=$4 WHERE id=$1")
        .bind(package)
        .bind(visibility)
        .bind(packages::now())
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn reserve_blob(
    pool: &SqlitePool,
    package: i64,
    token: i64,
    blob: &Blob,
) -> Result<bool> {
    let mut tx = begin(pool).await?;
    authorize(&mut tx, package, Some(token), Permission::Write, false).await?;
    let now = packages::now();
    sqlx::query("INSERT INTO package_blobs(digest,size,expires_at) VALUES($1,$2,$3) ON CONFLICT(digest) DO NOTHING")
        .bind(&blob.digest).bind(blob.size).bind(now+86400).execute(&mut *tx).await?;
    let (size, ready, deleting): (i64, bool, bool) =
        sqlx::query_as("SELECT size,ready,deleting FROM package_blobs WHERE digest=$1")
            .bind(&blob.digest)
            .fetch_one(&mut *tx)
            .await?;
    ensure!(size == blob.size && !deleting, Refusal::Blob);
    sqlx::query("UPDATE package_blobs SET expires_at=$2 WHERE digest=$1")
        .bind(&blob.digest)
        .bind(now + 86400)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO package_blob_grants(package_id,digest,expires_at) VALUES($1,$2,$3) ON CONFLICT(package_id,digest) DO UPDATE SET expires_at=excluded.expires_at")
        .bind(package).bind(&blob.digest).bind(now+86400).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(ready)
}

pub async fn ready_blob(pool: &SqlitePool, package: i64, token: i64, blob: &Blob) -> Result<()> {
    let mut tx = begin(pool).await?;
    authorize(&mut tx, package, Some(token), Permission::Write, false).await?;
    let changed = sqlx::query("UPDATE package_blobs SET ready=true WHERE digest=$1 AND size=$2 AND NOT deleting AND expires_at>$3 AND EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.package_id=$4 AND g.digest=package_blobs.digest)")
        .bind(&blob.digest).bind(blob.size).bind(packages::now()).bind(package).execute(&mut *tx).await?;
    ensure!(changed.rows_affected() == 1, Refusal::Blob);
    tx.commit().await?;
    Ok(())
}

pub async fn blob(
    pool: &SqlitePool,
    package: i64,
    token: Option<Credential<'_>>,
    digest: &str,
) -> Result<Blob> {
    let mut tx = begin(pool).await?;
    authorize(&mut tx, package, token, Permission::Read, false).await?;
    let result = sqlx::query_as("SELECT b.digest,b.size FROM package_blobs b WHERE b.digest=$2 AND b.ready AND NOT b.deleting AND (EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.package_id=$1 AND g.digest=b.digest AND g.expires_at>$3) OR EXISTS(SELECT 1 FROM package_version_blobs vb JOIN package_versions v ON v.id=vb.version_id WHERE v.package_id=$1 AND v.deleted_at IS NULL AND vb.digest=b.digest))")
        .bind(package).bind(digest).bind(packages::now()).fetch_optional(&mut *tx).await?.ok_or(Refusal::Missing)?;
    tx.commit().await?;
    Ok(result)
}

pub async fn start_upload(pool: &SqlitePool, package: i64, token: i64, id: &str) -> Result<Upload> {
    let mut tx = begin(pool).await?;
    authorize(&mut tx, package, Some(token), Permission::Write, false).await?;
    let result = sqlx::query_as("INSERT INTO package_uploads(id,package_id,token_id,expires_at) VALUES($1,$2,$3,$4) RETURNING *")
        .bind(id).bind(package).bind(token).bind(packages::now()+3600).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}

async fn get_upload(tx: &mut Tx<'_>, package: i64, token: i64, id: &str) -> Result<Upload> {
    authorize(tx, package, Some(token), Permission::Write, false).await?;
    Ok(sqlx::query_as("SELECT * FROM package_uploads WHERE id=$1 AND package_id=$2 AND token_id=$3 AND expires_at>$4")
        .bind(id).bind(package).bind(token).bind(packages::now()).fetch_optional(&mut **tx).await?.ok_or(Refusal::Missing)?)
}

pub async fn upload(pool: &SqlitePool, package: i64, token: i64, id: &str) -> Result<Upload> {
    let mut tx = begin(pool).await?;
    let result = get_upload(&mut tx, package, token, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn append_upload(
    pool: &SqlitePool,
    package: i64,
    token: i64,
    id: &str,
    offset: i64,
    chunk: &Blob,
) -> Result<Upload> {
    let mut tx = begin(pool).await?;
    let mut upload = get_upload(&mut tx, package, token, id).await?;
    ensure!(upload.offset == offset, Refusal::Offset);
    let mut chunks: Vec<Blob> = serde_json::from_str(&upload.chunks)?;
    ensure!(chunks.len() < 10000, Refusal::Offset);
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

pub async fn cancel_upload(pool: &SqlitePool, package: i64, token: i64, id: &str) -> Result<()> {
    let mut tx = begin(pool).await?;
    get_upload(&mut tx, package, token, id).await?;
    sqlx::query("DELETE FROM package_uploads WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn garbage(pool: &SqlitePool) -> Result<Vec<String>> {
    let mut tx = begin(pool).await?;
    let now = packages::now();
    sqlx::query("DELETE FROM package_registry_tokens WHERE expires_at<=$1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM package_uploads WHERE expires_at<=$1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM packages WHERE id IN (SELECT id FROM packages WHERE deleted_at<=$1 LIMIT 100)",
    )
    .bind(now - 30 * 86400)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM package_versions WHERE id IN (SELECT id FROM package_versions WHERE deleted_at<=$1 LIMIT 100)").bind(now-30*86400).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM package_blob_grants WHERE expires_at<=$1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE package_blobs SET deleting=true WHERE digest IN (SELECT b.digest FROM package_blobs b WHERE b.expires_at<=$1 AND NOT EXISTS(SELECT 1 FROM package_version_blobs vb WHERE vb.digest=b.digest) AND NOT EXISTS(SELECT 1 FROM package_blob_grants g WHERE g.digest=b.digest) LIMIT 100)")
        .bind(now).execute(&mut *tx).await?;
    let result = sqlx::query_scalar("SELECT digest FROM package_blobs WHERE deleting LIMIT 100")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn issue_registry_token(
    pool: &SqlitePool,
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
        .bind(hash).bind(token).bind(resources).bind(write).bind(packages::now()+300).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn registry_token(
    pool: &SqlitePool,
    hash: &str,
) -> Result<Option<packages::RegistryToken>> {
    Ok(sqlx::query_as("SELECT token_id,resources,write,expires_at FROM package_registry_tokens WHERE hash=$1 AND expires_at>$2")
        .bind(hash).bind(packages::now()).fetch_optional(pool).await?)
}

pub async fn registry_source(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<super::models::PersonalAccessToken>> {
    Ok(sqlx::query_as("SELECT id,user_id,name,scope,expires_at,created_at FROM personal_access_tokens WHERE id=$1").bind(id).fetch_optional(pool).await?)
}

pub async fn removed_blob(pool: &SqlitePool, digest: &str) -> Result<()> {
    sqlx::query("DELETE FROM package_blobs WHERE digest=$1 AND deleting")
        .bind(digest)
        .execute(pool)
        .await?;
    Ok(())
}
