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

/// The repositories another user shares with `user_id`: collaborator access on a repository the
/// user does not own, with the user's level on it.
pub async fn list_shared_repositories(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<SharedRepository>> {
    Ok(sqlx::query_as::<_, SharedRepository>(
        r#"SELECT r.*, a.access_level FROM repo_access a JOIN repositories r ON r.id = a.repo_id
        WHERE a.user_id = $1 AND a.access_level >= 1
            AND NOT (r.owner_kind = 'user' AND r.owner_user_id = $1)
        ORDER BY r.name, r.id"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
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
    let deleted =
        sqlx::query_as::<_, Repository>("DELETE FROM repositories WHERE id=$1 RETURNING *")
            .bind(repo_id)
            .fetch_optional(&mut *tx)
            .await?;
    let deleted = deleted.ok_or(Refusal::Denied)?;
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

/// Archives or unarchives the repository. Its callers check that the actor may administer it.
pub async fn set_repository_archived(pool: &PgPool, repo_id: i64, archived: bool) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE repositories SET is_archived = $1, updated_at = NOW() WHERE id = $2 AND is_archived <> $1",
    )
    .bind(archived)
    .bind(repo_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Stores the default branch. An archived repository keeps its branch; the result says whether it changed.
pub async fn set_repository_default_branch(
    pool: &PgPool,
    repo_id: i64,
    branch: &str,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE repositories SET default_branch = $1, updated_at = NOW() WHERE id = $2 AND NOT is_archived",
    )
    .bind(branch)
    .bind(repo_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}
