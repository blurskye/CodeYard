use crate::backend::db::models::{Repository, SharedRepository};
use crate::backend::db::repositories::records::{
    MergeMethodUpdate, RepositoryFilter, RepositorySort,
};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

pub async fn network_fork_count(pool: &SqlitePool, repo: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(crate::backend::db::repositories::records::NETWORK_FORK_COUNT)
        .bind(repo).fetch_one(pool).await?)
}

pub async fn create_repository(
    pool: &SqlitePool,
    owner_id: i64,
    name: &str,
    description: Option<&str>,
    is_private: bool,
    disk_path: &str,
) -> Result<Repository> {
    let result = sqlx::query_as::<_, Repository>(
        r#"
        INSERT INTO repositories (owner_kind, owner_user_id, name, description, is_private, disk_path, created_by_id)
        VALUES ('user', ?1, ?2, ?3, ?4, ?5, ?1)
        RETURNING *
        "#,
    )
    .bind(owner_id)
    .bind(name)
    .bind(description)
    .bind(is_private)
    .bind(disk_path)
    .fetch_one(pool)
    .await?;

    Ok(result)
}

pub async fn get_repository(
    pool: &SqlitePool,
    owner_name: &str,
    repo_name: &str,
) -> Result<Option<Repository>> {
    let repo = sqlx::query_as::<_, Repository>(
        r#"
        SELECT r.* FROM repositories r
        LEFT JOIN users u ON r.owner_user_id = u.id AND r.owner_kind = 'user'
        LEFT JOIN organizations o ON r.owner_org_id = o.id AND r.owner_kind = 'org'
        WHERE (u.username = ?1 OR o.name = ?1) AND r.name = ?2 AND NOT r.is_gist
        "#,
    )
    .bind(owner_name)
    .bind(repo_name)
    .fetch_optional(pool)
    .await?;

    Ok(repo)
}

pub async fn repository_pushed_at(
    pool: &SqlitePool,
    repo_id: i64,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    Ok(
        sqlx::query_scalar("SELECT MAX(created_at) FROM repository_pushes WHERE repo_id=?1")
            .bind(repo_id)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn get_repository_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Repository>> {
    let repo = sqlx::query_as::<_, Repository>("SELECT * FROM repositories WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(repo)
}

pub async fn get_repositories_by_ids(pool: &SqlitePool, ids: &[i64]) -> Result<Vec<Repository>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "SELECT * FROM repositories WHERE id IN (SELECT value FROM json_each(?1)) AND NOT is_gist",
    )
    .bind(serde_json::to_string(ids)?)
    .fetch_all(pool)
    .await?)
}

/// Project only the namespace of a well-formed, active repository owner.
/// This read is not organization profile or repository-access authority.
pub async fn repository_owner_name(pool: &SqlitePool, repo_id: i64) -> Result<Option<String>> {
    let name = sqlx::query_scalar::<_, String>(
        r#"
        SELECT CASE WHEN r.owner_kind = 'user' THEN u.username ELSE o.name END
        FROM repositories r
        LEFT JOIN users u ON r.owner_kind = 'user' AND r.owner_user_id = u.id
        LEFT JOIN organizations o ON r.owner_kind = 'org' AND r.owner_org_id = o.id
        WHERE r.id = ?1
          AND ((r.owner_kind = 'user' AND r.owner_user_id IS NOT NULL
                AND r.owner_org_id IS NULL AND u.id IS NOT NULL)
            OR (r.owner_kind = 'org' AND r.owner_org_id IS NOT NULL
                AND r.owner_user_id IS NULL AND o.id IS NOT NULL))
        "#,
    )
    .bind(repo_id)
    .fetch_optional(pool)
    .await?;

    Ok(name)
}

pub async fn list_user_repositories(pool: &SqlitePool, user_id: i64) -> Result<Vec<Repository>> {
    let repos = sqlx::query_as::<_, Repository>(
        "SELECT * FROM repositories WHERE owner_kind = 'user' AND owner_user_id = ?1 AND NOT is_gist ORDER BY name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(repos)
}

/// How many repositories `user_id` owns that a profile lists. Private ones are counted only when
/// `include_private` is set, which is the owner's own view.
pub async fn count_user_repositories(
    pool: &SqlitePool,
    user_id: i64,
    include_private: bool,
) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM repositories WHERE owner_kind = 'user' AND owner_user_id = ?1 AND NOT is_gist AND (?2 OR is_private = false)",
    )
    .bind(user_id)
    .bind(include_private)
    .fetch_one(pool)
    .await?)
}

pub async fn list_public_repositories(
    pool: &SqlitePool,
    sort: RepositorySort,
    limit: i64,
) -> Result<Vec<Repository>> {
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    // The order is part of the query, so the limit keeps the top rows of that order.
    // The stars index lets SQLite find that top row without sorting every public repository.
    let sql = match sort {
        RepositorySort::Recent => {
            "SELECT * FROM repositories WHERE is_private = false AND NOT is_gist ORDER BY created_at DESC, id DESC LIMIT ?1"
        }
        RepositorySort::Stars => {
            "SELECT * FROM repositories INDEXED BY idx_repositories_public_stars WHERE is_private = false AND NOT is_gist ORDER BY stars_count DESC, created_at DESC, id DESC LIMIT ?1"
        }
        RepositorySort::Forks => {
            "SELECT * FROM repositories WHERE is_private = false AND NOT is_gist ORDER BY forks_count DESC, created_at DESC, id DESC LIMIT ?1"
        }
        RepositorySort::Updated => {
            "SELECT * FROM repositories WHERE is_private = false AND NOT is_gist ORDER BY updated_at DESC, id DESC LIMIT ?1"
        }
    };
    let repos = sqlx::query_as::<_, Repository>(sql)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    Ok(repos)
}

fn shared_repository_condition() -> String {
    format!("NOT r.is_gist AND NOT (r.owner_kind='user' AND r.owner_user_id=u.id) AND ({})>=1 AND (EXISTS(SELECT 1 FROM org_memberships m JOIN organization_settings s ON s.org_id=m.org_id AND s.default_repository_permission IN (1,3,5) WHERE m.org_id=r.owner_org_id AND m.user_id=u.id AND m.role IN ('owner','admin','member')) OR EXISTS(SELECT 1 FROM org_memberships m JOIN organization_role_assignments assigned ON assigned.org_id=m.org_id JOIN organization_roles role ON role.id=assigned.role_id AND role.base_level BETWEEN 1 AND 5 WHERE m.org_id=r.owner_org_id AND m.user_id=u.id AND m.role IN ('owner','admin','member') AND ((assigned.user_id=u.id AND assigned.team_id IS NULL) OR (assigned.user_id IS NULL AND EXISTS(SELECT 1 FROM teams t JOIN effective_team_memberships tm ON tm.team_id=t.id AND tm.user_id=u.id AND tm.role IN ('member','maintainer') WHERE t.id=assigned.team_id AND t.org_id=assigned.org_id AND t.privacy IN ('visible','secret'))))) OR EXISTS(SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=u.id AND a.access_level BETWEEN 1 AND 5) OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=u.id AND (m.role IN ('owner','admin') OR (m.role='member' AND EXISTS(SELECT 1 FROM teams t JOIN effective_team_memberships tm ON tm.team_id=t.id AND tm.user_id=u.id AND tm.role IN ('member','maintainer') JOIN team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id AND a.permission BETWEEN 1 AND 5 WHERE t.org_id=m.org_id AND t.privacy IN ('visible','secret'))))))", super::access::USER_REPO_ACCESS_SQL)
}

/// Repositories shared directly or through organization and team authority.
pub async fn list_shared_repositories(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<SharedRepository>> {
    Ok(sqlx::query_as::<_, SharedRepository>(
        &format!("SELECT r.*, ({}) AS access_level FROM repositories r JOIN users u ON u.id=?1 WHERE {} ORDER BY r.name,r.id", super::access::USER_REPO_ACCESS_SQL, shared_repository_condition()),
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_user_repository_page(
    pool: &SqlitePool,
    user: i64,
    include_private: bool,
    text: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Repository>, i64)> {
    let condition = "NOT is_gist AND owner_kind='user' AND owner_user_id=?1 AND (?2 OR NOT is_private) AND (name REGEXP ?3 OR COALESCE(description,'') REGEXP ?3)";
    let needle = super::super::search::contains(text.trim());
    let count = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM repositories WHERE {condition}"
    ))
    .bind(user)
    .bind(include_private)
    .bind(&needle)
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query_as(&format!(
        "SELECT * FROM repositories WHERE {condition} ORDER BY name,id LIMIT ?4 OFFSET ?5"
    ))
    .bind(user)
    .bind(include_private)
    .bind(&needle)
    .bind(limit.clamp(1, 100))
    .bind(offset.max(0))
    .fetch_all(pool)
    .await?;
    Ok((rows, count))
}

pub async fn list_shared_repository_page(
    pool: &SqlitePool,
    user: i64,
    text: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<SharedRepository>, i64)> {
    let condition = format!(
        "{} AND (r.name REGEXP ?2 OR COALESCE(r.description,'') REGEXP ?2)",
        shared_repository_condition()
    );
    let needle = super::super::search::contains(text.trim());
    let count = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM repositories r JOIN users u ON u.id=?1 WHERE {condition}"
    ))
    .bind(user)
    .bind(&needle)
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query_as(&format!("SELECT r.*, ({}) AS access_level FROM repositories r JOIN users u ON u.id=?1 WHERE {condition} ORDER BY r.name,r.id LIMIT ?3 OFFSET ?4", super::access::USER_REPO_ACCESS_SQL))
        .bind(user).bind(&needle).bind(limit.clamp(1,100)).bind(offset.max(0)).fetch_all(pool).await?;
    Ok((rows, count))
}

/// The public repositories whose name or description contains `text`, ignoring case, that pass
/// `filter`, in the order `list_public_repositories` uses. An empty text matches every public
/// repository.
pub async fn search_public_repositories(
    pool: &SqlitePool,
    text: &str,
    filter: RepositoryFilter<'_>,
    sort: RepositorySort,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>> {
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    anyhow::ensure!(offset >= 0, "Result offset must be nonnegative");
    let pattern = super::super::search::contains(text);
    let (index, order) = match sort {
        RepositorySort::Recent => ("", "created_at DESC, id DESC"),
        RepositorySort::Stars => (
            "INDEXED BY idx_repositories_public_stars",
            "stars_count DESC, created_at DESC, id DESC",
        ),
        RepositorySort::Forks => ("", "forks_count DESC, created_at DESC, id DESC"),
        RepositorySort::Updated => ("", "updated_at DESC, id DESC"),
    };
    let sql = format!(
        r#"SELECT * FROM repositories {index} WHERE is_private = false AND NOT is_gist
            AND (name REGEXP ?2 OR description REGEXP ?2)
            AND (?4 = '' OR id IN (SELECT repo_id FROM repo_languages WHERE language = ?4))
            AND (?5 = '' OR id IN (SELECT repo_id FROM repository_topics WHERE topic = ?5))
            AND {kind}
            ORDER BY {order} LIMIT ?1 OFFSET ?3"#,
        kind = filter.kind.condition(),
    );
    Ok(sqlx::query_as::<_, Repository>(&sql)
        .bind(limit)
        .bind(pattern)
        .bind(offset)
        .bind(filter.language)
        .bind(filter.topic)
        .fetch_all(pool)
        .await?)
}

/// The languages found in public repositories, the most used first.
pub async fn public_languages(pool: &SqlitePool, limit: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        r#"SELECT l.language FROM repo_languages l JOIN repositories r ON r.id = l.repo_id
            WHERE r.is_private = false AND NOT r.is_gist GROUP BY l.language
            ORDER BY COUNT(*) DESC, l.language LIMIT ?1"#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn update_repository(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    description: Option<&str>,
    is_private: impl Into<crate::backend::db::repositories::records::VisibilityUpdate>,
    merge_methods: MergeMethodUpdate,
) -> Result<()> {
    let update = is_private.into();
    patch_repository(
        pool,
        actor_id,
        repo_id,
        &crate::backend::db::repositories::records::RepositoryChanges {
            description: Some(description),
            is_private: Some(update.is_private),
            visibility_expected: update.expected,
            merge_methods,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

pub async fn patch_repository(
    pool: &SqlitePool,
    actor: i64,
    repo_id: i64,
    changes: &crate::backend::db::repositories::records::RepositoryChanges<&str>,
) -> Result<Repository> {
    patch_core(pool, actor, repo_id, changes, None, None).await
}
pub async fn patch_repository_and_default(
    pool: &SqlitePool,
    actor: i64,
    repo_id: i64,
    changes: &crate::backend::db::repositories::records::RepositoryChanges<&str>,
    name: &str,
    expected_owner: Option<&str>,
) -> Result<Repository> {
    patch_core(pool, actor, repo_id, changes, Some(name), expected_owner).await
}
async fn patch_core(
    pool: &SqlitePool,
    actor: i64,
    repo_id: i64,
    changes: &crate::backend::db::repositories::records::RepositoryChanges<&str>,
    default_branch: Option<&str>,
    expected_owner: Option<&str>,
) -> Result<Repository> {
    use crate::backend::db::repositories::records::{
        validate_repository_alias, RepositorySettingsRefusal as Refusal,
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_archive_authority(&mut tx, actor, repo_id)
            .await?,
        Refusal::Denied
    );
    let previous = sqlx::query_as::<_, Repository>("SELECT * FROM repositories WHERE id=?1")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(name) = changes.name {
        validate_repository_alias(name)?;
        // Under a legacy `gists` owner an issued gist identifier names the gist, not this.
        anyhow::ensure!(
            super::super::gists::ordinary_name_available(
                &mut tx,
                previous.owner_user_id,
                previous.owner_org_id,
                name
            )
            .await?,
            Refusal::NameConflict
        );
        check_namespace(
            &mut tx,
            repo_id,
            previous.owner_user_id,
            previous.owner_org_id,
            Some(name),
            expected_owner,
        )
        .await?;
    }
    let website = changes
        .website
        .map(|value| crate::backend::db::repositories::topics::parse_website(value.unwrap_or("")))
        .transpose()?;
    if previous.is_archived && (changes.changes_metadata() || default_branch.is_some()) {
        anyhow::ensure!(changes.archived == Some(false), Refusal::Denied);
    }
    if previous.is_archived && changes.archived != Some(false) {
        tx.commit().await?;
        return Ok(previous);
    }
    let locked: bool = sqlx::query_scalar("SELECT visibility_locked FROM repositories WHERE id=?1")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    let private = changes.is_private.unwrap_or(previous.is_private);
    anyhow::ensure!(
        changes
            .visibility_expected
            .is_none_or(|value| value == previous.is_private),
        Refusal::Denied
    );
    anyhow::ensure!(private || !locked, Refusal::VisibilityLocked);
    if previous.is_archived {
        sqlx::query("UPDATE repositories SET is_archived=FALSE WHERE id=?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    }
    apply_merge_methods(&mut tx, repo_id, changes.merge_methods).await?;
    let updated=sqlx::query_as::<_,Repository>("UPDATE repositories SET name=COALESCE(?2,name),description=CASE WHEN ?3 THEN ?4 ELSE description END,website=CASE WHEN ?5 THEN ?6 ELSE website END,is_private=?7,is_archived=?8,is_template=COALESCE(?9,is_template),default_branch=COALESCE(?10,default_branch),updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND NOT is_archived RETURNING *")
        .bind(repo_id).bind(changes.name).bind(changes.description.is_some()).bind(changes.description.flatten()).bind(website.is_some()).bind(website.as_ref().and_then(|value|value.as_deref())).bind(private).bind(changes.archived.unwrap_or(previous.is_archived)).bind(changes.is_template).bind(default_branch).fetch_one(&mut *tx).await.map_err(name_conflict_error)?;
    tx.commit().await?;
    Ok(updated)
}

pub async fn set_merge_methods(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    update: MergeMethodUpdate,
) -> Result<()> {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
            .await?,
        Refusal::Denied
    );
    apply_merge_methods(&mut tx, repo_id, update).await?;
    tx.commit().await?;
    Ok(())
}

/// Writes a merge-method change inside the caller's transaction. A method left as `None` keeps
/// its stored value, and at least one method must stay allowed.
async fn apply_merge_methods(
    tx: &mut Transaction<'_, Sqlite>,
    repo_id: i64,
    update: MergeMethodUpdate,
) -> Result<()> {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let (merge, squash, rebase): (bool, bool, bool) = sqlx::query_as(
        "SELECT allow_merge_commit, allow_squash_merge, allow_rebase_merge FROM repositories WHERE id=?1",
    )
    .bind(repo_id)
    .fetch_one(&mut **tx)
    .await?;
    let merge = update.merge.unwrap_or(merge);
    let squash = update.squash.unwrap_or(squash);
    let rebase = update.rebase.unwrap_or(rebase);
    anyhow::ensure!(merge || squash || rebase, Refusal::NoMergeMethod);
    // The guard keeps the invariant on the write itself, so no later change to the read above
    // can leave every method off.
    let result = sqlx::query(
        "UPDATE repositories SET allow_merge_commit = ?1, allow_squash_merge = ?2, allow_rebase_merge = ?3, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?4 AND NOT is_archived AND (?1 OR ?2 OR ?3)",
    )
    .bind(merge)
    .bind(squash)
    .bind(rebase)
    .bind(repo_id)
    .execute(&mut **tx)
    .await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::Denied);
    Ok(())
}

pub async fn update_repo_default_branch(
    pool: &SqlitePool,
    repo_id: i64,
    branch: &str,
) -> Result<()> {
    sqlx::query("UPDATE repositories SET default_branch=?1, updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?2")
        .bind(branch).bind(repo_id).execute(pool).await?;
    Ok(())
}

pub async fn advance_contribution_version(pool: &SqlitePool, repo_id: i64) -> Result<()> {
    sqlx::query("UPDATE repositories SET contribution_version=contribution_version+1 WHERE id=?1")
        .bind(repo_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_repo_size(pool: &SqlitePool, repo_id: i64, size: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    let changed =
        sqlx::query("UPDATE repositories SET size_bytes=?1 WHERE id=?2 AND size_bytes<>?1")
            .bind(size)
            .bind(repo_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if changed > 0 {
        sqlx::query("INSERT INTO repo_storage_history (repo_id, size_bytes) VALUES (?1, ?2)")
            .bind(repo_id)
            .bind(size)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Deletes a repository only while it has accepted no writes. A failed creation uses this, so
/// a push that landed while creation was failing keeps its repository.
pub async fn delete_unwritten_repository(
    pool: &SqlitePool,
    repo_id: i64,
) -> Result<Option<Repository>> {
    Ok(sqlx::query_as::<_, Repository>(
        "DELETE FROM repositories WHERE id=?1 AND contribution_version=0 RETURNING *",
    )
    .bind(repo_id)
    .fetch_optional(pool)
    .await?)
}

/// Deletes the repository row (child rows cascade) and returns the row as it was
/// deleted, so the caller removes the files where that row says they are.
pub async fn delete_repository(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Repository> {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_archive_authority(&mut tx, actor_id, repo_id)
            .await?,
        Refusal::Denied
    );
    let files: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT DISTINCT a.file_id FROM release_assets a JOIN releases r ON r.id=a.release_id WHERE r.repo_id=?1 ORDER BY a.file_id")
        .bind(repo_id).fetch_all(&mut *tx).await?;
    // Deletion detaches the repository's packages. Those that its privacy hid stay private.
    sqlx::query("UPDATE packages SET visibility='private' WHERE repo_id=?1 AND (SELECT is_private FROM repositories WHERE id=?1)")
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    let deleted =
        sqlx::query_as::<_, Repository>("DELETE FROM repositories WHERE id=?1 RETURNING *")
            .bind(repo_id)
            .fetch_optional(&mut *tx)
            .await?;
    let deleted = deleted.ok_or(Refusal::Denied)?;
    for file in files {
        crate::backend::db::sqlite::files::delete_unreferenced_file_in_transaction(&mut tx, file)
            .await?;
    }
    tx.commit().await?;
    Ok(deleted)
}

pub async fn rename_repository(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    new_name: &str,
) -> Result<Repository> {
    use crate::backend::db::repositories::records::{
        validate_repository_alias, RepositorySettingsRefusal as Refusal,
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
            .await?,
        Refusal::Denied
    );
    validate_repository_alias(new_name)?;
    let (user, org): (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT owner_user_id,owner_org_id FROM repositories WHERE id=?1")
            .bind(repo_id)
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(
        super::super::gists::ordinary_name_available(&mut tx, user, org, new_name).await?,
        Refusal::NameConflict
    );
    // A name that an immutable release reserved stays with the repository that published it.
    check_namespace(&mut tx, repo_id, user, org, Some(new_name), None).await?;
    // Alias only: no physical rename or new storage identity.
    let result = sqlx::query_as::<_, Repository>(
        "UPDATE repositories SET name = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?2 AND NOT is_archived RETURNING *",
    )
    .bind(new_name)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(name_conflict_error)?
    .ok_or_else(|| anyhow::anyhow!(Refusal::Denied))?;
    tx.commit().await?;
    Ok(result)
}

/// Moves the repository to `target`. Its id and storage stay put; only the owner columns change.
/// The actor must administer the repository and be allowed to create repositories for `target`.
// Pin owner names through metadata commit. Legacy callers cannot inspect Git,
// so ambiguous historical destinations require the primary checked path.
async fn check_namespace(
    tx: &mut Transaction<'_, Sqlite>,
    repo_id: i64,
    user: Option<i64>,
    org: Option<i64>,
    name: Option<&str>,
    expected_owner: Option<&str>,
) -> Result<()> {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let owner:String=sqlx::query_scalar("SELECT COALESCE((SELECT username FROM users WHERE id=?1),(SELECT name FROM organizations WHERE id=?2))").bind(user).bind(org).fetch_one(&mut **tx).await?;
    if let Some(expected) = expected_owner {
        anyhow::ensure!(owner.eq_ignore_ascii_case(expected), Refusal::NameConflict);
    } else {
        let reserved:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM immutable_release_repository_names n JOIN immutable_release_owner_names o ON o.owner_kind=n.owner_kind AND o.owner_id=n.owner_id WHERE n.repo_id<>?1 AND n.name_key=LOWER(COALESCE(?3,(SELECT name FROM repositories WHERE id=?1))) AND o.name_key=LOWER(?2))").bind(repo_id).bind(&owner).bind(name).fetch_one(&mut **tx).await?;
        anyhow::ensure!(!reserved, Refusal::NameConflict);
    }
    Ok(())
}
pub async fn transfer_repository(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    target: crate::backend::db::repositories::creation::RepositoryTarget,
) -> Result<Repository> {
    transfer_repository_inner(pool, actor_id, repo_id, target, None).await
}
pub async fn transfer_repository_checked(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    target: crate::backend::db::repositories::creation::RepositoryTarget,
    expected_owner: &str,
) -> Result<Repository> {
    transfer_repository_inner(pool, actor_id, repo_id, target, Some(expected_owner)).await
}
async fn transfer_repository_inner(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    target: crate::backend::db::repositories::creation::RepositoryTarget,
    expected_owner: Option<&str>,
) -> Result<Repository> {
    use crate::backend::db::repositories::creation::{RepositoryCreationOwner, RepositoryTarget};
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
            .await?,
        Refusal::Denied
    );
    crate::backend::db::sqlite::repositories::creation::creation_authority(
        &mut tx,
        RepositoryCreationOwner { actor_id, target },
    )
    .await?;
    let private: bool = sqlx::query_scalar("SELECT is_private FROM repositories WHERE id=?1")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    super::creation::validate_creation_visibility(
        &mut tx,
        RepositoryCreationOwner { actor_id, target },
        private,
    )
    .await?;
    let previous: (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT owner_kind,owner_user_id,owner_org_id FROM repositories WHERE id=?1",
    )
    .bind(repo_id)
    .fetch_one(&mut *tx)
    .await?;
    let (kind, user, org) = match target {
        RepositoryTarget::Personal(id) => ("user", Some(id), None),
        RepositoryTarget::Organization(id) => ("org", None, Some(id)),
    };
    check_namespace(&mut tx, repo_id, user, org, None, expected_owner).await?;
    let changed = previous != (kind.to_owned(), user, org);
    if changed {
        if let Some(org) = org {
            super::super::organizations::properties::creation_values_on(
                &mut tx,
                actor_id,
                org,
                &[],
            )
            .await?;
        }
        // Organization metadata must not cross an ownership boundary or prevent FK-safe transfer.
        sqlx::query("DELETE FROM repository_property_values WHERE repo_id=?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    }
    if let (true, Some(org)) = (changed, org) {
        // One fork of a source per organization. The write transaction keeps this check exact.
        let duplicate: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories AS other JOIN repositories AS moved ON moved.id=?1 WHERE other.owner_kind='org' AND other.owner_org_id=?2 AND other.forked_from_id=moved.forked_from_id AND other.id<>moved.id)")
            .bind(repo_id)
            .bind(org)
            .fetch_one(&mut *tx)
            .await?;
        anyhow::ensure!(!duplicate, Refusal::NameConflict);
    }
    let name: String = sqlx::query_scalar("SELECT name FROM repositories WHERE id=?1")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(
        super::super::gists::ordinary_name_available(&mut tx, user, org, &name).await?,
        Refusal::NameConflict
    );
    let result = sqlx::query_as::<_, Repository>(
        "UPDATE repositories SET owner_kind = ?1, owner_user_id = ?2, owner_org_id = ?3, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?4 AND NOT is_archived RETURNING *",
    )
    .bind(kind)
    .bind(user)
    .bind(org)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(name_conflict_error)?
    .ok_or_else(|| anyhow::anyhow!(Refusal::Denied))?;
    // Direct personal grants cannot be managed in an organization. Drop them when ownership
    // changes, and stop work admitted under the former owner's authority.
    if changed && org.is_some() {
        sqlx::query("DELETE FROM repo_access WHERE repo_id=?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    }
    if changed {
        super::creation::grant_creator(
            &mut tx,
            RepositoryCreationOwner { actor_id, target },
            repo_id,
        )
        .await?;
        let pulls: Vec<(i64, String)> = sqlx::query_as("SELECT id,updated_at FROM pull_requests WHERE head_repo_id=?1 AND maintainer_can_modify")
            .bind(repo_id).fetch_all(&mut *tx).await?;
        for (id, revision) in pulls {
            let next = super::super::time::next_revision(super::super::time::decode(&revision)?)?;
            sqlx::query("UPDATE pull_requests SET maintainer_can_modify=0,maintainer_edit_granted_by_id=NULL,updated_at=?2 WHERE id=?1")
                .bind(id).bind(next).execute(&mut *tx).await?;
        }
        // Selected organization values must not silently regain access on a transfer back.
        sqlx::query("DELETE FROM org_actions_value_repositories WHERE repo_id=?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        // Detach the former owner's policy without discarding already applied settings.
        sqlx::query(
            "UPDATE repository_security_settings SET configuration_id=NULL WHERE repo_id=?1",
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
        super::super::code_security::apply_default(&mut tx, &result).await?;
        // Packages stay in their publishing namespace; the new owner must not inherit access.
        // A package that the private repository hid stays private once it is detached.
        sqlx::query("UPDATE packages SET repo_id=NULL,visibility=CASE WHEN (SELECT is_private FROM repositories WHERE id=$2) THEN 'private' ELSE visibility END,updated_at=$1 WHERE repo_id=$2")
            .bind(crate::backend::db::packages::now())
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        // Inventory describes the former namespace. Do not transfer it or reserve its deployment names.
        sqlx::query("DELETE FROM artifact_metadata_records WHERE artifact_id IN (SELECT id FROM artifact_metadata WHERE repo_id=?1)")
            .bind(repo_id).execute(&mut *tx).await?;
        // Team grants belong to the former owner's teams and must not come back later.
        sqlx::query("DELETE FROM pr_team_reviewers WHERE pull_request_id IN (SELECT id FROM pull_requests WHERE repo_id=?1)")
            .bind(repo_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM team_repo_access WHERE repo_id=?1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE repo_id=?1 AND (status IN ('queued','running') OR completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) ORDER BY id")
            .bind(repo_id).fetch_all(&mut *tx).await?;
        super::super::ci::stop_ci_runs(&mut tx, &runs).await?;
        super::super::runners::remove_repository_runners(&mut tx, repo_id).await?;
    }
    tx.commit().await?;
    Ok(result)
}

/// A unique owner/name collision is a refusal the caller can name; other errors pass through.
fn name_conflict_error(error: sqlx::Error) -> anyhow::Error {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    if error.as_database_error().is_some_and(|e| {
        e.code().as_deref() == Some("2067")
            && matches!(
                e.message(),
                "UNIQUE constraint failed: repositories.owner_user_id, repositories.name"
                    | "UNIQUE constraint failed: repositories.owner_org_id, repositories.name"
            )
    }) {
        anyhow::Error::new(Refusal::NameConflict)
    } else {
        anyhow::Error::new(error)
    }
}

pub(crate) async fn repository_owner_names(
    pool: &SqlitePool,
    repository_ids: &[i64],
) -> Result<Vec<(i64, String)>> {
    debug_assert!(!repository_ids.is_empty() && repository_ids.len() <= 500);
    let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT r.id, CASE WHEN r.owner_kind='user' THEN u.username ELSE o.name END FROM repositories r LEFT JOIN users u ON r.owner_kind='user' AND r.owner_user_id=u.id LEFT JOIN organizations o ON r.owner_kind='org' AND r.owner_org_id=o.id WHERE ((r.owner_kind='user' AND r.owner_user_id IS NOT NULL AND r.owner_org_id IS NULL AND u.id IS NOT NULL) OR (r.owner_kind='org' AND r.owner_org_id IS NOT NULL AND r.owner_user_id IS NULL AND o.id IS NOT NULL)) AND r.id IN (",
    );
    let mut ids = query.separated(",");
    for id in repository_ids {
        ids.push_bind(id);
    }
    query.push(")");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

/// Undo a default change whose HEAD publication failed, without overwriting a later setting.
pub async fn restore_repository_default_branch(
    pool: &SqlitePool,
    repo_id: i64,
    expected: &str,
    previous: &str,
) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE repositories SET default_branch = ?1 WHERE id = ?2 AND default_branch = ?3",
    )
    .bind(previous)
    .bind(repo_id)
    .bind(expected)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Archives or unarchives under current administrator authority, including archived rows.
pub async fn set_repository_archived(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    archived: bool,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_archive_authority(&mut tx, actor_id, repo_id)
            .await?,
        crate::backend::db::repositories::records::RepositorySettingsRefusal::Denied
    );
    let result = sqlx::query(
        "UPDATE repositories SET is_archived = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?2 AND is_archived <> ?1",
    )
    .bind(archived)
    .bind(repo_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn repository_wiki(pool: &SqlitePool, repo_id: i64) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT wiki_enabled FROM repository_features WHERE repo_id=?1")
            .bind(repo_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or_default(),
    )
}

pub async fn set_repository_wiki(
    pool: &SqlitePool,
    actor: i64,
    repo_id: i64,
    enabled: bool,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::super::discussions::lock_admin_authority(&mut tx, actor, repo_id).await?,
        crate::backend::db::repositories::records::RepositorySettingsRefusal::Denied
    );
    sqlx::query("INSERT INTO repository_features (repo_id,wiki_enabled) VALUES (?1,?2) ON CONFLICT(repo_id) DO UPDATE SET wiki_enabled=?2")
        .bind(repo_id)
        .bind(enabled)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Stores the default branch when the actor may administer the repository. The check and the
/// update share one transaction. An archived repository keeps its branch. Returns whether it changed.
pub async fn set_repository_default_branch(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    branch: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::backend::db::sqlite::discussions::lock_admin_authority(&mut tx, actor_id, repo_id)
        .await?
    {
        return Ok(false);
    }
    let result = sqlx::query(
        "UPDATE repositories SET default_branch = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id = ?2 AND NOT is_archived",
    )
    .bind(branch)
    .bind(repo_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}
