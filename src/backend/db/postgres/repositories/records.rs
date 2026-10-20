use crate::backend::db::models::{Repository, SharedRepository};
use crate::backend::db::repositories::records::{
    MergeMethodUpdate, RepositoryFilter, RepositorySort,
};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

pub async fn create_repository(
    pool: &PgPool,
    owner_id: i64,
    name: &str,
    description: Option<&str>,
    is_private: bool,
    disk_path: &str,
) -> Result<Repository> {
    let result = sqlx::query_as::<_, Repository>(
        r#"
        INSERT INTO repositories (owner_kind, owner_user_id, name, description, is_private, disk_path, created_by_id)
        VALUES ('user', $1, $2, $3, $4, $5, $1)
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
    pool: &PgPool,
    owner_name: &str,
    repo_name: &str,
) -> Result<Option<Repository>> {
    let repo = sqlx::query_as::<_, Repository>(
        r#"
        SELECT r.* FROM repositories r
        LEFT JOIN users u ON r.owner_user_id = u.id AND r.owner_kind = 'user'
        LEFT JOIN organizations o ON r.owner_org_id = o.id AND r.owner_kind = 'org'
        WHERE (u.username = $1 OR o.name = $1) AND r.name = $2
        "#,
    )
    .bind(owner_name)
    .bind(repo_name)
    .fetch_optional(pool)
    .await?;

    Ok(repo)
}

pub async fn get_repository_by_id(pool: &PgPool, id: i64) -> Result<Option<Repository>> {
    let repo = sqlx::query_as::<_, Repository>("SELECT * FROM repositories WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(repo)
}

pub async fn get_repositories_by_ids(pool: &PgPool, ids: &[i64]) -> Result<Vec<Repository>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(
        sqlx::query_as("SELECT * FROM repositories WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await?,
    )
}

/// Project only the namespace of a well-formed, active repository owner.
/// This read is not organization profile or repository-access authority.
pub async fn repository_owner_name(pool: &PgPool, repo_id: i64) -> Result<Option<String>> {
    let name = sqlx::query_scalar::<_, String>(
        r#"
        SELECT CASE WHEN r.owner_kind = 'user' THEN u.username ELSE o.name END
        FROM repositories r
        LEFT JOIN users u ON r.owner_kind = 'user' AND r.owner_user_id = u.id
        LEFT JOIN organizations o ON r.owner_kind = 'org' AND r.owner_org_id = o.id
        WHERE r.id = $1
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

pub async fn list_user_repositories(pool: &PgPool, user_id: i64) -> Result<Vec<Repository>> {
    let repos = sqlx::query_as::<_, Repository>(
        "SELECT * FROM repositories WHERE owner_kind = 'user' AND owner_user_id = $1 ORDER BY name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(repos)
}

pub async fn list_profile_repositories(
    pool: &PgPool,
    user_id: i64,
    include_private: bool,
    offset: i64,
) -> Result<Vec<Repository>> {
    Ok(sqlx::query_as("SELECT * FROM repositories WHERE owner_kind='user' AND owner_user_id=$1 AND ($2 OR NOT is_private) ORDER BY name,id LIMIT 30 OFFSET $3")
        .bind(user_id).bind(include_private).bind(offset).fetch_all(pool).await?)
}

/// How many repositories `user_id` owns that a profile lists. Private ones are counted only when
/// `include_private` is set, which is the owner's own view.
pub async fn count_user_repositories(
    pool: &PgPool,
    user_id: i64,
    include_private: bool,
) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM repositories WHERE owner_kind = 'user' AND owner_user_id = $1 AND ($2 OR is_private = FALSE)",
    )
    .bind(user_id)
    .bind(include_private)
    .fetch_one(pool)
    .await?)
}

pub async fn list_public_repositories(
    pool: &PgPool,
    sort: RepositorySort,
    limit: i64,
) -> Result<Vec<Repository>> {
    // The order is part of the query, so the limit keeps the top rows of that order.
    let sql = match sort {
        RepositorySort::Recent => {
            "SELECT * FROM repositories WHERE is_private = false ORDER BY created_at DESC, id DESC LIMIT $1"
        }
        RepositorySort::Stars => {
            "SELECT * FROM repositories WHERE is_private = false ORDER BY stars_count DESC, created_at DESC, id DESC LIMIT $1"
        }
        RepositorySort::Forks => {
            "SELECT * FROM repositories WHERE is_private = false ORDER BY forks_count DESC, created_at DESC, id DESC LIMIT $1"
        }
        RepositorySort::Updated => {
            "SELECT * FROM repositories WHERE is_private = false ORDER BY updated_at DESC, id DESC LIMIT $1"
        }
    };
    let repos = sqlx::query_as::<_, Repository>(sql)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    Ok(repos)
}

fn shared_repository_condition() -> String {
    format!("NOT (r.owner_kind='user' AND r.owner_user_id=u.id) AND ({})>=1 AND (EXISTS(SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=u.id AND a.access_level BETWEEN 1 AND 5) OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=u.id AND (m.role IN ('owner','admin') OR (m.role='member' AND EXISTS(SELECT 1 FROM teams t JOIN team_memberships tm ON tm.team_id=t.id AND tm.user_id=u.id AND tm.role IN ('member','maintainer') JOIN team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id AND a.permission BETWEEN 1 AND 5 WHERE t.org_id=m.org_id AND t.privacy IN ('visible','secret'))))))", super::access::USER_REPO_ACCESS_SQL)
}

/// Repositories shared directly or through organization and team authority.
pub async fn list_shared_repositories(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<SharedRepository>> {
    Ok(sqlx::query_as::<_, SharedRepository>(
        &format!("SELECT r.*, ({}) AS access_level FROM repositories r JOIN users u ON u.id=$1 WHERE {} ORDER BY r.name,r.id", super::access::USER_REPO_ACCESS_SQL, shared_repository_condition()),
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

pub async fn list_user_repository_page(
    pool: &PgPool,
    user: i64,
    include_private: bool,
    text: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Repository>, i64)> {
    let condition = "owner_kind='user' AND owner_user_id=$1 AND ($2 OR NOT is_private) AND (strpos(lower(name),$3)>0 OR strpos(lower(COALESCE(description,'')),$3)>0)";
    let needle = text.trim().to_lowercase();
    let count = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM repositories WHERE {condition}"
    ))
    .bind(user)
    .bind(include_private)
    .bind(&needle)
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query_as(&format!(
        "SELECT * FROM repositories WHERE {condition} ORDER BY name,id LIMIT $4 OFFSET $5"
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
    pool: &PgPool,
    user: i64,
    text: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<SharedRepository>, i64)> {
    let condition = format!(
        "{} AND (strpos(lower(r.name),$2)>0 OR strpos(lower(COALESCE(r.description,'')),$2)>0)",
        shared_repository_condition()
    );
    let needle = text.trim().to_lowercase();
    let count = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM repositories r JOIN users u ON u.id=$1 WHERE {condition}"
    ))
    .bind(user)
    .bind(&needle)
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query_as(&format!("SELECT r.*, ({}) AS access_level FROM repositories r JOIN users u ON u.id=$1 WHERE {condition} ORDER BY r.name,r.id LIMIT $3 OFFSET $4", super::access::USER_REPO_ACCESS_SQL))
        .bind(user).bind(&needle).bind(limit.clamp(1,100)).bind(offset.max(0)).fetch_all(pool).await?;
    Ok((rows, count))
}

/// The public repositories whose name or description contains `text`, ignoring case, in the
/// order `list_public_repositories` uses. An empty text matches every public repository.
pub async fn search_public_repositories(
    pool: &PgPool,
    text: &str,
    filter: RepositoryFilter<'_>,
    sort: RepositorySort,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>> {
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    anyhow::ensure!(offset >= 0, "Result offset must be nonnegative");
    let pattern = crate::backend::db::discussions::contains_pattern(text);
    let order = match sort {
        RepositorySort::Recent => "created_at DESC, id DESC",
        RepositorySort::Stars => "stars_count DESC, created_at DESC, id DESC",
        RepositorySort::Forks => "forks_count DESC, created_at DESC, id DESC",
        RepositorySort::Updated => "updated_at DESC, id DESC",
    };
    let kind = filter.kind.condition();
    Ok(sqlx::query_as::<_, Repository>(&format!(
        r#"SELECT * FROM repositories WHERE is_private = false
            AND (LOWER(name COLLATE "C") LIKE LOWER($2::TEXT COLLATE "C") ESCAPE '\'
                OR LOWER(description COLLATE "C") LIKE LOWER($2::TEXT COLLATE "C") ESCAPE '\')
            AND ($4::TEXT = '' OR id IN (SELECT repo_id FROM repo_languages WHERE language = $4::TEXT))
            AND ($5::TEXT = '' OR id IN (SELECT repo_id FROM repository_topics WHERE topic = $5::TEXT))
            AND {kind}
        ORDER BY {order} LIMIT $1 OFFSET $3"#
    ))
    .bind(limit)
    .bind(pattern)
    .bind(offset)
    .bind(filter.language)
    .bind(filter.topic)
    .fetch_all(pool)
    .await?)
}

/// The languages found in public repositories, the most used first.
pub async fn public_languages(pool: &PgPool, limit: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        r#"SELECT l.language FROM repo_languages l JOIN repositories r ON r.id = l.repo_id
            WHERE r.is_private = false GROUP BY l.language
            ORDER BY COUNT(*) DESC, l.language LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// The ancestors of the fork bound to $1, found by walking forked_from_id upward.
const ANCESTORS: &str = "WITH RECURSIVE ancestors(id) AS (
    SELECT forked_from_id FROM repositories WHERE id = $1 AND forked_from_id IS NOT NULL
    UNION
    SELECT r.forked_from_id FROM repositories r JOIN ancestors a ON r.id = a.id
    WHERE r.forked_from_id IS NOT NULL
) SELECT id FROM ancestors";

/// Locks the ancestors of the fork bound to `repo_id`, before the fork's own row. This is
/// the one lock order for a fork and its sources: the organizations that own the sources
/// FOR SHARE, then the sources by id (a source always has a lower id than its fork), each
/// with `lock`. Reads take FOR SHARE. A deletion or privacy change takes FOR NO KEY UPDATE,
/// the lock its forks_count trigger needs on the source, so it never upgrades a shared
/// source lock that another transaction holds. Authority checks call this through
/// `lock_user_repo_access_in_transaction`, before they lock the fork.
pub(crate) async fn lock_fork_ancestors(
    connection: &mut sqlx::PgConnection,
    repo_id: i64,
    lock: &str,
) -> Result<()> {
    let sql = format!(
        "SELECT id FROM organizations WHERE id IN (
            SELECT owner_org_id FROM repositories
            WHERE owner_kind = 'org' AND id IN ({ANCESTORS})
        ) ORDER BY id FOR SHARE"
    );
    let _: Vec<i64> = sqlx::query_scalar(&sql)
        .bind(repo_id)
        .fetch_all(&mut *connection)
        .await?;
    let sql = format!("SELECT id FROM repositories WHERE id IN ({ANCESTORS}) ORDER BY id {lock}");
    let _: Vec<i64> = sqlx::query_scalar(&sql)
        .bind(repo_id)
        .fetch_all(connection)
        .await?;
    Ok(())
}

pub async fn update_repository(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    description: Option<&str>,
    is_private: bool,
    merge_methods: MergeMethodUpdate,
) -> Result<()> {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_repository_mutation_authority(
            &mut tx, actor_id, repo_id
        )
        .await?,
        Refusal::Denied
    );
    let locked: bool =
        sqlx::query_scalar("SELECT visibility_locked FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(repo_id)
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(is_private || !locked, Refusal::VisibilityLocked);
    apply_merge_methods(&mut tx, repo_id, merge_methods).await?;
    let result = sqlx::query(
        "UPDATE repositories SET description = $1, is_private = $2, updated_at = NOW() WHERE id = $3 AND NOT is_archived AND ($2 OR NOT visibility_locked)",
    )
    .bind(description)
    .bind(is_private)
    .bind(repo_id)
    .execute(&mut *tx)
    .await?;
    anyhow::ensure!(result.rows_affected() == 1, Refusal::Denied);
    tx.commit().await?;
    Ok(())
}

pub async fn set_merge_methods(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    update: MergeMethodUpdate,
) -> Result<()> {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_repository_mutation_authority(
            &mut tx, actor_id, repo_id
        )
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
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    update: MergeMethodUpdate,
) -> Result<()> {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let (merge, squash, rebase): (bool, bool, bool) = sqlx::query_as(
        "SELECT allow_merge_commit, allow_squash_merge, allow_rebase_merge FROM repositories WHERE id=$1 FOR UPDATE",
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
        "UPDATE repositories SET allow_merge_commit = $1, allow_squash_merge = $2, allow_rebase_merge = $3, updated_at = NOW() WHERE id = $4 AND NOT is_archived AND ($1 OR $2 OR $3)",
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

pub async fn update_repo_default_branch_and_size(
    pool: &PgPool,
    repo_id: i64,
    default_branch: &str,
    size_bytes: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE repositories SET default_branch = $1, size_bytes = $2, updated_at = NOW() WHERE id = $3",
    )
    .bind(default_branch)
    .bind(size_bytes)
    .bind(repo_id)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn insert_repo_storage_snapshot(
    pool: &PgPool,
    repo_id: i64,
    size_bytes: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO repo_storage_history (repo_id, size_bytes) VALUES ($1, $2)")
        .bind(repo_id)
        .bind(size_bytes)
        .execute(pool)
        .await?;

    Ok(())
}

/// Deletes the repository row (child rows cascade) and returns the row as it was
/// deleted, so the caller removes the files where that row says they are.
pub async fn delete_repository(pool: &PgPool, actor_id: i64, repo_id: i64) -> Result<Repository> {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_repository_mutation_authority(
            &mut tx, actor_id, repo_id
        )
        .await?,
        Refusal::Denied
    );
    let files: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT DISTINCT a.file_id FROM release_assets a JOIN releases r ON r.id=a.release_id WHERE r.repo_id=$1 ORDER BY a.file_id")
        .bind(repo_id).fetch_all(&mut *tx).await?;
    let deleted =
        sqlx::query_as::<_, Repository>("DELETE FROM repositories WHERE id=$1 RETURNING *")
            .bind(repo_id)
            .fetch_optional(&mut *tx)
            .await?;
    let deleted = deleted.ok_or(Refusal::Denied)?;
    for file in files {
        crate::backend::db::postgres::files::delete_unlinked_release_file(&mut tx, file).await?;
    }
    tx.commit().await?;
    Ok(deleted)
}

pub async fn rename_repository(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    new_name: &str,
) -> Result<Repository> {
    use crate::backend::db::repositories::records::{
        validate_repository_alias, RepositorySettingsRefusal as Refusal,
    };
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_repository_mutation_authority(
            &mut tx, actor_id, repo_id
        )
        .await?,
        Refusal::Denied
    );
    validate_repository_alias(new_name)?;
    // Alias only: no physical rename or new storage identity.
    let result = sqlx::query_as::<_, Repository>(
        "UPDATE repositories SET name = $1, updated_at = NOW() WHERE id = $2 AND NOT is_archived RETURNING *",
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
pub async fn transfer_repository(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    target: crate::backend::db::repositories::creation::RepositoryTarget,
) -> Result<Repository> {
    for attempt in 0..3 {
        let result = transfer_repository_once(pool, actor_id, repo_id, target).await;
        let deadlock = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<sqlx::Error>())
            .and_then(sqlx::Error::as_database_error)
            .is_some_and(|e| e.code().as_deref() == Some("40P01"));
        if !deadlock || attempt == 2 {
            return result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10 << attempt)).await;
    }
    unreachable!()
}

async fn transfer_repository_once(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    target: crate::backend::db::repositories::creation::RepositoryTarget,
) -> Result<Repository> {
    use crate::backend::db::repositories::creation::{RepositoryCreationOwner, RepositoryTarget};
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_repository_mutation_authority(
            &mut tx, actor_id, repo_id
        )
        .await?,
        Refusal::Denied
    );
    crate::backend::db::postgres::repositories::creation::creation_authority(
        &mut tx,
        RepositoryCreationOwner { actor_id, target },
    )
    .await?;
    let previous: (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT owner_kind,owner_user_id,owner_org_id FROM repositories WHERE id=$1",
    )
    .bind(repo_id)
    .fetch_one(&mut *tx)
    .await?;
    let (kind, user, org) = match target {
        RepositoryTarget::Personal(id) => ("user", Some(id), None),
        RepositoryTarget::Organization(id) => ("org", None, Some(id)),
    };
    let result = sqlx::query_as::<_, Repository>(
        "UPDATE repositories SET owner_kind = $1, owner_user_id = $2, owner_org_id = $3, updated_at = NOW() WHERE id = $4 AND NOT is_archived RETURNING *",
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
    let changed = previous != (kind.to_owned(), user, org);
    if changed && org.is_some() {
        sqlx::query("DELETE FROM repo_access WHERE repo_id=$1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    }
    if changed {
        let runs: Vec<i64> = sqlx::query_scalar("SELECT id FROM ci_runs WHERE repo_id=$1 AND status IN ('queued','running') ORDER BY id FOR UPDATE")
            .bind(repo_id).fetch_all(&mut *tx).await?;
        super::super::ci::stop_ci_runs(&mut tx, &runs).await?;
    }
    tx.commit().await?;
    Ok(result)
}

/// A unique owner/name collision is a refusal the caller can name; other errors pass through.
fn name_conflict_error(error: sqlx::Error) -> anyhow::Error {
    use crate::backend::db::repositories::records::RepositorySettingsRefusal as Refusal;
    if error.as_database_error().is_some_and(|e| {
        e.code().as_deref() == Some("23505")
            && e.table() == Some("repositories")
            && matches!(e.constraint(), Some("repo_unique_user" | "repo_unique_org"))
    }) {
        anyhow::Error::new(Refusal::NameConflict)
    } else {
        anyhow::Error::new(error)
    }
}

pub(crate) async fn repository_owner_names(
    pool: &PgPool,
    repository_ids: &[i64],
) -> Result<Vec<(i64, String)>> {
    debug_assert!(!repository_ids.is_empty() && repository_ids.len() <= 500);
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
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
    pool: &PgPool,
    repo_id: i64,
    expected: &str,
    previous: &str,
) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE repositories SET default_branch = $1 WHERE id = $2 AND default_branch = $3",
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
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    archived: bool,
) -> Result<bool> {
    use super::access::{lock_user_repo_access_in_transaction, RepositoryRowLock};
    use crate::backend::db::repositories::records::RepositorySettingsRefusal;
    let mut tx = pool.begin().await?;
    let authority = lock_user_repo_access_in_transaction(
        &mut tx,
        actor_id,
        repo_id,
        RepositoryRowLock::Exclusive,
        true,
    )
    .await?;
    anyhow::ensure!(
        authority.is_some_and(|a| {
            (!a.organization || a.organization_usable)
                && (a.access.can_admin()
                    || (!a.organization && (a.is_admin || a.owner_user_id == Some(actor_id))))
        }),
        RepositorySettingsRefusal::Denied
    );
    let result = sqlx::query(
        "UPDATE repositories SET is_archived = $1, updated_at = NOW() WHERE id = $2 AND is_archived <> $1",
    )
    .bind(archived)
    .bind(repo_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

/// Stores the default branch when the actor may administer the repository. The check and the
/// update share one transaction. An archived repository keeps its branch. Returns whether it changed.
pub async fn set_repository_default_branch(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    branch: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if !crate::backend::db::postgres::discussions::lock_repository_mutation_authority(
        &mut tx, actor_id, repo_id,
    )
    .await?
    {
        return Ok(false);
    }
    let result = sqlx::query(
        "UPDATE repositories SET default_branch = $1, updated_at = NOW() WHERE id = $2 AND NOT is_archived",
    )
    .bind(branch)
    .bind(repo_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}
