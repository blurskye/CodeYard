//! Remote identity metadata retains local foreign keys, never personal credentials.
use crate::{
    models::{Organization, Repository, User},
    DbPool,
};
use anyhow::{ensure, Result};
use codeyard_types::cell_ids::Prefix;
use codeyard_types::cells::{Key, KeyKind, ObjectKind};

fn home(pool: &DbPool, source: Prefix, id: i64) -> Result<()> {
    Prefix::for_id(id)?;
    ensure!(
        source != pool.cell_id(),
        "A local account cannot be replaced by a stub"
    );
    Ok(())
}
fn text(value: Option<&str>, maximum: usize) -> Result<()> {
    ensure!(
        value.is_none_or(|value| !value.contains('\0')
            && value.len() <= maximum * 4
            && value.chars().count() <= maximum),
        "Stub metadata is too long"
    );
    Ok(())
}
fn website(value: Option<&str>) -> Result<()> {
    text(value, 255)?;
    ensure!(
        value.is_none_or(
            |value| value.is_empty() || codeyard_types::website::parse(value).is_some()
        ),
        "Invalid stub website"
    );
    Ok(())
}
fn name(value: &str, kind: ObjectKind) -> Result<()> {
    let key = Key::new(KeyKind::Name, value)?;
    ensure!(key.belongs_to(kind), "Invalid stub name");
    Ok(())
}
const USER: &str = "INSERT INTO users(id,username,display_name,bio,location,website,is_banned,must_change_password,two_factor_enabled,home_cell,is_stub,created_at,updated_at)
VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,TRUE,$11,$12)
ON CONFLICT(id) DO UPDATE SET home_cell=excluded.home_cell,username=excluded.username,display_name=excluded.display_name,bio=excluded.bio,location=excluded.location,website=excluded.website,is_banned=excluded.is_banned,must_change_password=excluded.must_change_password,two_factor_enabled=excluded.two_factor_enabled,updated_at=excluded.updated_at
WHERE users.is_stub AND users.updated_at<=excluded.updated_at";
const ORGANIZATION: &str = "INSERT INTO organizations(id,name,display_name,description,is_verified,home_cell,is_stub,created_at,updated_at)
VALUES($1,$2,$3,$4,$5,$6,TRUE,$7,$8)
ON CONFLICT(id) DO UPDATE SET home_cell=excluded.home_cell,name=excluded.name,display_name=excluded.display_name,description=excluded.description,is_verified=excluded.is_verified,updated_at=excluded.updated_at
WHERE organizations.is_stub AND organizations.updated_at<=excluded.updated_at";
const REPOSITORY: &str = "INSERT INTO repositories(id,owner_kind,owner_user_id,owner_org_id,name,description,website,is_private,is_archived,is_fork,is_gist,is_template,is_internal,default_branch,home_cell,stars_count,forks_count,watchers_count,open_issues_count,allow_merge_commit,allow_squash_merge,allow_rebase_merge,created_at,updated_at,is_stub,disk_path,forked_from_id)
VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,TRUE,'',(SELECT id FROM repositories WHERE id=$25))
ON CONFLICT(id) DO UPDATE SET home_cell=excluded.home_cell,owner_kind=excluded.owner_kind,owner_user_id=excluded.owner_user_id,owner_org_id=excluded.owner_org_id,name=excluded.name,description=excluded.description,website=excluded.website,is_private=excluded.is_private,is_archived=excluded.is_archived,is_fork=excluded.is_fork,forked_from_id=excluded.forked_from_id,is_gist=excluded.is_gist,is_template=excluded.is_template,is_internal=excluded.is_internal,default_branch=excluded.default_branch,stars_count=excluded.stars_count,forks_count=excluded.forks_count,watchers_count=excluded.watchers_count,open_issues_count=excluded.open_issues_count,allow_merge_commit=excluded.allow_merge_commit,allow_squash_merge=excluded.allow_squash_merge,allow_rebase_merge=excluded.allow_rebase_merge,updated_at=excluded.updated_at
WHERE repositories.is_stub AND repositories.updated_at<=excluded.updated_at";

/// The caller verifies the source node against the directory's current object home, so a
/// refresh from an account's new home after a move updates the stub's home too.
/// False means the row is local, belongs to another home, or already has newer metadata.
/// No password, email, session, key, token, private billing data or local storage is copied.
pub async fn user(pool: &DbPool, source: Prefix, value: &User) -> Result<bool> {
    home(pool, source, value.id)?;
    ensure!(
        !value.is_stub && value.home_cell == i32::from(source.get()),
        "Stub source is not authoritative"
    );
    name(&value.username, ObjectKind::User)?;
    text(value.display_name.as_deref(), 255)?;
    text(value.bio.as_deref(), 4096)?;
    text(value.location.as_deref(), 255)?;
    website(value.website.as_deref())?;
    let changed = match pool {
        DbPool::Postgres(pool) => sqlx::query(USER)
            .bind(value.id)
            .bind(&value.username)
            .bind(&value.display_name)
            .bind(&value.bio)
            .bind(&value.location)
            .bind(&value.website)
            .bind(value.is_banned)
            .bind(value.must_change_password)
            .bind(value.two_factor_enabled)
            .bind(i32::from(source.get()))
            .bind(value.created_at)
            .bind(value.updated_at)
            .execute(pool)
            .await?
            .rows_affected(),
        DbPool::Sqlite(pool) => sqlx::query(USER)
            .bind(value.id)
            .bind(&value.username)
            .bind(&value.display_name)
            .bind(&value.bio)
            .bind(&value.location)
            .bind(&value.website)
            .bind(value.is_banned)
            .bind(value.must_change_password)
            .bind(value.two_factor_enabled)
            .bind(i32::from(source.get()))
            .bind(
                value
                    .created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            )
            .bind(
                value
                    .updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            )
            .execute(pool)
            .await?
            .rows_affected(),
    };
    Ok(changed == 1)
}
pub async fn organization(pool: &DbPool, source: Prefix, value: &Organization) -> Result<bool> {
    home(pool, source, value.id)?;
    ensure!(
        !value.is_stub && value.home_cell == i32::from(source.get()),
        "Stub source is not authoritative"
    );
    name(&value.name, ObjectKind::Organization)?;
    text(value.display_name.as_deref(), 255)?;
    text(value.description.as_deref(), 4096)?;
    let changed = match pool {
        DbPool::Postgres(pool) => sqlx::query(ORGANIZATION)
            .bind(value.id)
            .bind(&value.name)
            .bind(&value.display_name)
            .bind(&value.description)
            .bind(value.is_verified)
            .bind(i32::from(source.get()))
            .bind(value.created_at)
            .bind(value.updated_at)
            .execute(pool)
            .await?
            .rows_affected(),
        DbPool::Sqlite(pool) => sqlx::query(ORGANIZATION)
            .bind(value.id)
            .bind(&value.name)
            .bind(&value.display_name)
            .bind(&value.description)
            .bind(value.is_verified)
            .bind(i32::from(source.get()))
            .bind(
                value
                    .created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            )
            .bind(
                value
                    .updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            )
            .execute(pool)
            .await?
            .rows_affected(),
    };
    Ok(changed == 1)
}
pub async fn repository(pool: &DbPool, source: Prefix, value: &Repository) -> Result<bool> {
    home(pool, source, value.id)?;
    ensure!(
        !value.is_stub && value.home_cell == i32::from(source.get()),
        "Stub source is not authoritative"
    );
    ensure!(
        codeyard_types::repository_names::valid(&value.name),
        "Invalid stub repository name"
    );
    text(value.description.as_deref(), 4096)?;
    website(value.website.as_deref())?;
    text(Some(&value.default_branch), 1024)?;
    ensure!(
        codeyard_git::branch_ref(&value.default_branch).is_some(),
        "Invalid stub default branch"
    );
    ensure!(
        matches!(value.owner_kind.as_str(), "user" | "org"),
        "Invalid stub owner"
    );
    Prefix::for_id(value.owner_id())?;
    if let Some(parent) = value.forked_from_id {
        Prefix::for_id(parent)?;
        ensure!(parent != value.id, "Repository cannot fork itself");
    }
    let changed = match pool {
        DbPool::Postgres(pool) => sqlx::query(REPOSITORY)
            .bind(value.id)
            .bind(&value.owner_kind)
            .bind(value.owner_user_id)
            .bind(value.owner_org_id)
            .bind(&value.name)
            .bind(&value.description)
            .bind(&value.website)
            .bind(value.is_private)
            .bind(value.is_archived)
            .bind(value.is_fork)
            .bind(value.is_gist)
            .bind(value.is_template)
            .bind(value.is_internal)
            .bind(&value.default_branch)
            .bind(i32::from(source.get()))
            .bind(value.stars_count)
            .bind(value.forks_count)
            .bind(value.watchers_count)
            .bind(value.open_issues_count)
            .bind(value.allow_merge_commit)
            .bind(value.allow_squash_merge)
            .bind(value.allow_rebase_merge)
            .bind(value.created_at)
            .bind(value.updated_at)
            .bind(value.forked_from_id)
            .execute(pool)
            .await?
            .rows_affected(),
        DbPool::Sqlite(pool) => sqlx::query(REPOSITORY)
            .bind(value.id)
            .bind(&value.owner_kind)
            .bind(value.owner_user_id)
            .bind(value.owner_org_id)
            .bind(&value.name)
            .bind(&value.description)
            .bind(&value.website)
            .bind(value.is_private)
            .bind(value.is_archived)
            .bind(value.is_fork)
            .bind(value.is_gist)
            .bind(value.is_template)
            .bind(value.is_internal)
            .bind(&value.default_branch)
            .bind(i32::from(source.get()))
            .bind(value.stars_count)
            .bind(value.forks_count)
            .bind(value.watchers_count)
            .bind(value.open_issues_count)
            .bind(value.allow_merge_commit)
            .bind(value.allow_squash_merge)
            .bind(value.allow_rebase_merge)
            .bind(
                value
                    .created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            )
            .bind(
                value
                    .updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            )
            .bind(value.forked_from_id)
            .execute(pool)
            .await?
            .rows_affected(),
    };
    Ok(changed == 1)
}
