use crate::backend::db::{
    gists::{
        validate_comment, Admission, Comment, Gist, GistList, GistRefusal, ListScope, NewGist,
        PendingWrite,
    },
    models::{Repository, User},
    users::accounts::AccountCredential,
};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};
const FIELDS:&str="g.repo_id,g.id,r.owner_user_id AS owner_id,r.forked_from_id,g.description,g.is_public,g.ready,g.created_at,g.updated_at,(SELECT COUNT(*) FROM gist_comments c WHERE c.repo_id=g.repo_id) AS comments";
const FROM:&str="FROM gists g JOIN repositories r ON r.id=g.repo_id AND r.is_gist AND r.owner_kind='user' JOIN users u ON u.id=r.owner_user_id AND NOT u.is_banned";

/// An unready gist with no pending commit is a creation that died before its first write; one
/// an hour old is no longer in flight.
/// Bound recovery of backings that outlived provisioning without acquiring gist metadata.
pub async fn orphaned_for_owner(pool: &SqlitePool, owner: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar("SELECT r.id FROM repositories r JOIN users u ON u.id=r.owner_user_id WHERE r.is_gist AND r.owner_kind='user' AND r.owner_user_id=?1 AND r.owner_org_id IS NULL AND NOT u.is_banned AND julianday(r.created_at)<julianday('now','-2 hours') AND NOT EXISTS(SELECT 1 FROM gists g WHERE g.repo_id=r.id) ORDER BY r.created_at,r.id LIMIT 10").bind(owner).fetch_all(pool).await?)
}

pub async fn unready_for_owner(pool: &SqlitePool, owner: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT g.id FROM gists g JOIN repositories r ON r.id=g.repo_id JOIN users u ON u.id=r.owner_user_id WHERE r.is_gist AND r.owner_kind='user' AND r.owner_user_id=?1 AND r.owner_org_id IS NULL AND NOT u.is_banned AND NOT g.ready AND (g.pending_sha IS NOT NULL OR g.created_at < strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) ORDER BY g.created_at,g.id LIMIT 10").bind(owner).fetch_all(pool).await?)
}
pub async fn is_identifier(pool: &SqlitePool, id: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gist_identifiers WHERE id=lower(?1))")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}
pub(super) async fn ordinary_name_available(
    connection: &mut sqlx::SqliteConnection,
    user: Option<i64>,
    org: Option<i64>,
    name: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM gist_identifiers WHERE id=lower(?3) AND (EXISTS(SELECT 1 FROM users WHERE id=?1 AND lower(username) IN ('gist','gists')) OR EXISTS(SELECT 1 FROM organizations WHERE id=?2 AND lower(name) IN ('gist','gists'))))").bind(user).bind(org).bind(name).fetch_one(connection).await?)
}
pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Gist>> {
    Ok(
        sqlx::query_as(&format!("SELECT {FIELDS} {FROM} WHERE g.id=?1"))
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn by_repository(pool: &SqlitePool, repo: i64) -> Result<Option<Gist>> {
    Ok(
        sqlx::query_as(&format!("SELECT {FIELDS} {FROM} WHERE g.repo_id=?1"))
            .bind(repo)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn git_repository(pool: &SqlitePool, id: &str) -> Result<Option<Repository>> {
    Ok(
        sqlx::query_as(&format!("SELECT r.* {FROM} WHERE g.id=?1 AND g.ready"))
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn list(pool: &SqlitePool, input: &GistList) -> Result<Vec<Gist>> {
    let (mode, id, secret) = match input.scope {
        ListScope::Public => (0, 0, false),
        ListScope::Owner(id, secret) => (1, id, secret),
        ListScope::Starred(id) => (2, id, true),
        ListScope::Forks(id) => (3, id, true),
    };
    Ok(sqlx::query_as(&format!("SELECT {FIELDS} {FROM} WHERE g.ready AND (?1 IS NULL OR julianday(g.updated_at)>julianday(?1)) AND ((?2=0 AND g.is_public) OR (?2=1 AND r.owner_user_id=?3 AND (?4 OR g.is_public)) OR (?2=2 AND EXISTS(SELECT 1 FROM gist_stars s WHERE s.repo_id=g.repo_id AND s.user_id=?3)) OR (?2=3 AND r.forked_from_id=?3)) ORDER BY g.updated_at DESC,g.id DESC LIMIT ?5 OFFSET ?6")).bind(input.since).bind(mode).bind(id).bind(secret).bind(input.limit.clamp(0,101)).bind(input.offset.max(0)).fetch_all(pool).await?)
}
const OWNED: &str = "WHERE g.ready AND r.owner_user_id=?1 AND (?2 OR g.is_public) AND (?3 IS NULL OR g.is_public=?3)";
pub async fn count_owned(
    pool: &SqlitePool,
    owner: i64,
    secret: bool,
    privacy: Option<bool>,
) -> Result<i64> {
    Ok(
        sqlx::query_scalar(&format!("SELECT COUNT(*) {FROM} {OWNED}"))
            .bind(owner)
            .bind(secret)
            .bind(privacy)
            .fetch_one(pool)
            .await?,
    )
}
pub async fn list_created(
    pool: &SqlitePool,
    owner: i64,
    secret: bool,
    privacy: Option<bool>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Gist>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {FIELDS} {FROM} {OWNED} ORDER BY g.created_at DESC,g.id DESC LIMIT ?4 OFFSET ?5"
    ))
    .bind(owner)
    .bind(secret)
    .bind(privacy)
    .bind(limit.clamp(0, 100))
    .bind(offset.max(0))
    .fetch_all(pool)
    .await?)
}
async fn authority(
    tx: &mut Transaction<'_, Sqlite>,
    actor: i64,
    repo: i64,
    own: bool,
    interact: bool,
) -> Result<Repository> {
    let repository:Repository=sqlx::query_as("SELECT r.* FROM repositories r JOIN users u ON u.id=r.owner_user_id WHERE r.id=?1 AND r.is_gist AND r.owner_kind='user' AND NOT u.is_banned").bind(repo).fetch_optional(&mut **tx).await?.ok_or(GistRefusal::Missing)?;
    anyhow::ensure!(
        !own || repository.owner_user_id == Some(actor),
        GistRefusal::Denied
    );
    if !own {
        let ready: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gists WHERE repo_id=?1 AND ready)")
                .bind(repo)
                .fetch_one(&mut **tx)
                .await?;
        anyhow::ensure!(ready, GistRefusal::Missing);
    }
    if interact {
        anyhow::ensure!(
            !super::users::blocks::repository_interaction_blocked(tx, actor, repo).await?,
            GistRefusal::Denied
        );
    }
    Ok(repository)
}
pub async fn reserve(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    input: &NewGist<'_>,
) -> Result<()> {
    anyhow::ensure!(
        input.id.len() == 32
            && input
                .id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && input.description.chars().count() <= 65535
            && !input.description.contains('\0'),
        GistRefusal::Invalid
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await?;
    let repo = authority(&mut tx, actor, input.repo_id, true, false).await?;
    if let Some(source) = repo.forked_from_id {
        let public: bool = sqlx::query_scalar(&format!(
            "SELECT g.is_public {FROM} WHERE g.repo_id=?1 AND g.ready"
        ))
        .bind(source)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(GistRefusal::Missing)?;
        anyhow::ensure!(
            public == input.is_public
                && !super::users::blocks::repository_interaction_blocked(&mut tx, actor, source)
                    .await?,
            GistRefusal::Denied
        );
    }
    let occupied:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON r.owner_kind='user' AND r.owner_user_id=u.id LEFT JOIN organizations o ON r.owner_kind='org' AND r.owner_org_id=o.id WHERE NOT r.is_gist AND lower(r.name)=?1 AND (lower(u.username) IN ('gist','gists') OR lower(o.name) IN ('gist','gists')))").bind(input.id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(!occupied, GistRefusal::Conflict);
    let reserved =
        sqlx::query("INSERT INTO gist_identifiers(id) VALUES(?1) ON CONFLICT DO NOTHING")
            .bind(input.id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    anyhow::ensure!(reserved == 1, GistRefusal::Conflict);
    sqlx::query("INSERT INTO gists(repo_id,id,description,is_public,ready) VALUES(?1,?2,?3,?4,?5)")
        .bind(repo.id)
        .bind(input.id)
        .bind(input.description)
        .bind(input.is_public)
        .bind(repo.is_fork)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn admit(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    input: &Admission<'_>,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await?;
    let repo = authority(&mut tx, actor, input.repo_id, true, false).await?;
    anyhow::ensure!(
        repo.disk_path == input.disk_path
            && input
                .storage_node_id
                .is_none_or(|node| node == repo.storage_node_id),
        GistRefusal::Conflict
    );
    let changed=sqlx::query("UPDATE gists SET pending_sha=?2,pending_base=?3,pending_description=COALESCE(?4,description),pending_storage_node_id=?5 WHERE repo_id=?1 AND pending_sha IS NULL").bind(repo.id).bind(input.sha).bind(input.base).bind(input.description).bind(repo.storage_node_id).execute(&mut *tx).await?.rows_affected();
    anyhow::ensure!(changed == 1, GistRefusal::Conflict);
    tx.commit().await?;
    Ok(())
}
pub async fn pending(pool: &SqlitePool, repo: i64) -> Result<Option<PendingWrite>> {
    Ok(sqlx::query_as("SELECT pending_sha AS sha,pending_base AS base,pending_storage_node_id AS storage_node_id FROM gists WHERE repo_id=?1 AND pending_sha IS NOT NULL").bind(repo).fetch_optional(pool).await?)
}
/// Finishing an admitted publication does not borrow a later token's authority.
pub async fn finish(
    pool: &SqlitePool,
    repo: i64,
    sha: &str,
    published: bool,
    placement: Option<Option<i64>>,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let node: Option<Option<i64>> =
        sqlx::query_scalar("SELECT storage_node_id FROM repositories WHERE id=?1 AND is_gist")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    let node = node.ok_or(GistRefusal::Missing)?;
    anyhow::ensure!(
        placement.is_none_or(|expected| expected == node),
        GistRefusal::Conflict
    );
    let previous: String = sqlx::query_scalar("SELECT updated_at FROM gists WHERE repo_id=?1")
        .bind(repo)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(GistRefusal::Missing)?;
    let updated = if published {
        super::time::next_revision(super::time::decode(&previous)?)?
    } else {
        previous
    };
    let changed=sqlx::query("UPDATE gists SET description=CASE WHEN ?3 THEN pending_description ELSE description END,ready=ready OR ?3,updated_at=?4,pending_sha=NULL,pending_base=NULL,pending_description=NULL,pending_storage_node_id=NULL WHERE repo_id=?1 AND pending_sha=?2").bind(repo).bind(sha).bind(published).bind(&updated).execute(&mut *tx).await?.rows_affected();
    anyhow::ensure!(changed == 1, GistRefusal::Conflict);
    if published {
        sqlx::query("UPDATE repositories SET updated_at=?2 WHERE id=?1")
            .bind(repo)
            .bind(&updated)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn pushed(pool: &SqlitePool, repo: i64, placement: Option<Option<i64>>) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let node: Option<Option<i64>> =
        sqlx::query_scalar("SELECT storage_node_id FROM repositories WHERE id=?1 AND is_gist")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    let node = node.ok_or(GistRefusal::Missing)?;
    anyhow::ensure!(
        placement.is_none_or(|expected| expected == node),
        GistRefusal::Conflict
    );
    let previous: String =
        sqlx::query_scalar("SELECT updated_at FROM gists WHERE repo_id=?1 AND ready")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(GistRefusal::Missing)?;
    let updated = super::time::next_revision(super::time::decode(&previous)?)?;
    sqlx::query("UPDATE gists SET updated_at=?2 WHERE repo_id=?1")
        .bind(repo)
        .bind(updated)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn delete(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    repo: i64,
) -> Result<Repository> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await?;
    let repo = authority(&mut tx, actor, repo, true, false).await?;
    sqlx::query("DELETE FROM repositories WHERE id=?1")
        .bind(repo.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(repo)
}
pub async fn discard(pool: &SqlitePool, repo: i64, orphan_only: bool) -> Result<Option<Repository>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let deleted = sqlx::query_as("DELETE FROM repositories WHERE id=?1 AND is_gist AND NOT EXISTS(SELECT 1 FROM gists g WHERE g.repo_id=repositories.id AND (?2 OR g.ready OR g.pending_sha IS NOT NULL)) RETURNING *").bind(repo).bind(orphan_only).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(deleted)
}
pub async fn starred(pool: &SqlitePool, actor: i64, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM gist_stars WHERE repo_id=?1 AND user_id=?2)",
    )
    .bind(repo)
    .bind(actor)
    .fetch_one(pool)
    .await?)
}
pub async fn star(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    repo: i64,
    adding: bool,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await?;
    authority(&mut tx, actor, repo, false, adding).await?;
    if adding {
        sqlx::query("INSERT INTO gist_stars(repo_id,user_id) VALUES(?1,?2) ON CONFLICT DO NOTHING")
            .bind(repo)
            .bind(actor)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("DELETE FROM gist_stars WHERE repo_id=?1 AND user_id=?2")
            .bind(repo)
            .bind(actor)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn comments(
    pool: &SqlitePool,
    repo: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Comment>> {
    Ok(sqlx::query_as(
        "SELECT * FROM gist_comments WHERE repo_id=?1 ORDER BY id LIMIT ?2 OFFSET ?3",
    )
    .bind(repo)
    .bind(limit.clamp(0, 101))
    .bind(offset.max(0))
    .fetch_all(pool)
    .await?)
}
pub async fn comment(pool: &SqlitePool, repo: i64, id: i64) -> Result<Option<Comment>> {
    Ok(
        sqlx::query_as("SELECT * FROM gist_comments WHERE repo_id=?1 AND id=?2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn create_comment(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    repo: i64,
    body: &str,
) -> Result<Comment> {
    validate_comment(body)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await?;
    authority(&mut tx, actor, repo, false, true).await?;
    let comment = sqlx::query_as(
        "INSERT INTO gist_comments(repo_id,user_id,body) VALUES(?1,?2,?3) RETURNING *",
    )
    .bind(repo)
    .bind(actor)
    .bind(body)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(comment)
}
pub async fn change_comment(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    repo: i64,
    id: i64,
    body: Option<&str>,
) -> Result<Comment> {
    if let Some(body) = body {
        validate_comment(body)?;
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await?;
    let repository = authority(&mut tx, actor, repo, false, body.is_some()).await?;
    let comment: Comment = sqlx::query_as("SELECT * FROM gist_comments WHERE repo_id=?1 AND id=?2")
        .bind(repo)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(GistRefusal::Missing)?;
    anyhow::ensure!(
        comment.user_id == Some(actor)
            || (body.is_none() && repository.owner_user_id == Some(actor)),
        GistRefusal::Denied
    );
    let comment = if let Some(body) = body {
        let updated = super::time::next_revision(comment.updated_at)?;
        sqlx::query_as("UPDATE gist_comments SET body=?2,updated_at=?3 WHERE id=?1 RETURNING *")
            .bind(id)
            .bind(body)
            .bind(updated)
            .fetch_one(&mut *tx)
            .await?
    } else {
        sqlx::query("DELETE FROM gist_comments WHERE id=?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        comment
    };
    tx.commit().await?;
    Ok(comment)
}
pub async fn commit_user(pool: &SqlitePool, email: &str) -> Result<Option<User>> {
    Ok(sqlx::query_as("SELECT u.* FROM users u WHERE NOT u.is_banned AND (LOWER(u.username||'@users.noreply')=LOWER(?1) OR (u.email_verified AND LOWER(u.email)=LOWER(?1)) OR EXISTS(SELECT 1 FROM user_emails e WHERE e.user_id=u.id AND e.verified AND LOWER(e.email)=LOWER(?1))) ORDER BY (LOWER(u.username||'@users.noreply')=LOWER(?1)) DESC,u.id LIMIT 1").bind(email).fetch_optional(pool).await?)
}
