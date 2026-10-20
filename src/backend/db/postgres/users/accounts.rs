use crate::backend::db::models::{AdminAccountSummary, AdminActivityPoint, AdminUser, User};
use anyhow::Result;
use sqlx::PgPool;

pub async fn create_user(pool: &PgPool, username: &str, email: Option<&str>) -> Result<User> {
    let mut tx = pool.begin().await?;
    super::transitions::lock_membership(&mut tx).await?;
    super::transitions::lock_namespace(&mut tx).await?;
    super::transitions::require_user_namespace(&mut tx, username).await?;
    let result = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (username, email)
        VALUES ($1, $2)
        RETURNING *
        "#,
    )
    .bind(username)
    .bind(email)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn create_user_with_password(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
) -> Result<User> {
    create_user_with_password_and_flags(pool, username, email, password_hash, false, false).await
}

pub async fn create_user_with_password_and_flags(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
    is_admin: bool,
    must_change_password: bool,
) -> Result<User> {
    let mut tx = pool.begin().await?;
    super::transitions::lock_membership(&mut tx).await?;
    super::transitions::lock_namespace(&mut tx).await?;
    super::transitions::require_user_namespace(&mut tx, username).await?;
    let result = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (username, email, password_hash, is_admin, must_change_password)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING *
        "#,
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .bind(is_admin)
    .bind(must_change_password)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn count_users(pool: &PgPool) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;

    Ok(count)
}

pub async fn update_user_password(
    pool: &PgPool,
    user_id: i64,
    password_hash: &str,
    must_change_password: bool,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let organizations =
        super::transitions::owned_organizations_for_account_change(&mut tx, user_id).await?;
    // The new hash and the session revocation commit together.
    sqlx::query(
        "UPDATE users SET password_hash = $1, must_change_password = $2, updated_at = NOW() WHERE id = $3",
    )
    .bind(password_hash)
    .bind(must_change_password)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if must_change_password {
        // A forced change locks the account out of CI like a ban does.
        crate::backend::db::postgres::ci::stop_actor_ci_runs(&mut tx, user_id).await?;
        super::transitions::stop_ownerless_organization_ci(&mut tx, &organizations).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn get_user_by_id(pool: &PgPool, id: i64) -> Result<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;

    Ok(user)
}

pub async fn get_user_by_username(pool: &PgPool, username: &str) -> Result<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = $1")
        .bind(username)
        .fetch_optional(pool)
        .await?;

    Ok(user)
}

pub async fn get_user_by_email(pool: &PgPool, email: &str) -> Result<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1")
        .bind(email)
        .fetch_optional(pool)
        .await?;

    Ok(user)
}

pub async fn get_users_by_emails(pool: &PgPool, emails: &[String]) -> Result<Vec<User>> {
    if emails.is_empty() {
        return Ok(vec![]);
    }

    let users = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = ANY($1)")
        .bind(emails)
        .fetch_all(pool)
        .await?;

    Ok(users)
}

pub async fn get_users_by_usernames(pool: &PgPool, usernames: &[String]) -> Result<Vec<User>> {
    if usernames.is_empty() {
        return Ok(vec![]);
    }
    Ok(
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ANY($1)")
            .bind(usernames)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_users_by_ids(pool: &PgPool, ids: &[i64]) -> Result<Vec<User>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn list_users(pool: &PgPool) -> Result<Vec<User>> {
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY username")
        .fetch_all(pool)
        .await?;

    Ok(users)
}

/// Port of the Node `createUserFromOAuth`, minus the avatar: this schema models
/// avatars as uploaded files (`users.avatar_file_id`), so there is no
/// `users.avatar_url` column to set. `display_name` comes straight from the
/// provider and the email is marked verified when one was provided.
pub async fn create_user_from_oauth(
    pool: &PgPool,
    username: &str,
    email: Option<&str>,
    display_name: Option<&str>,
) -> Result<User> {
    let mut tx = pool.begin().await?;
    super::transitions::lock_membership(&mut tx).await?;
    super::transitions::lock_namespace(&mut tx).await?;
    super::transitions::require_user_namespace(&mut tx, username).await?;
    let result = sqlx::query_as::<_, User>(
        r#"
        INSERT INTO users (username, email, display_name, email_verified)
        VALUES ($1, $2, $3, $4)
        RETURNING *
        "#,
    )
    .bind(username)
    .bind(email)
    .bind(display_name)
    .bind(email.is_some())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn set_user_avatar(
    pool: &PgPool,
    user_id: i64,
    avatar_file_id: Option<uuid::Uuid>,
) -> Result<crate::backend::db::users::accounts::UserAvatarOutcome> {
    use crate::backend::db::users::accounts::UserAvatarOutcome;

    let mut tx = pool.begin().await?;
    let account: Option<(Option<uuid::Uuid>, bool)> =
        sqlx::query_as("SELECT avatar_file_id,is_banned FROM users WHERE id=$1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((previous, banned)) = account else {
        return Ok(UserAvatarOutcome::NotUsable);
    };
    if banned {
        return Ok(UserAvatarOutcome::Banned);
    }
    if let Some(file) = avatar_file_id {
        let usable: Option<uuid::Uuid> = sqlx::query_scalar(
            "UPDATE files SET is_active=TRUE,upload_expires_at=NULL,updated_at=NOW() WHERE id=$1 AND user_id=$2 AND (is_active OR (confirmed_at IS NOT NULL AND upload_expires_at>NOW())) AND reason='avatar' AND content_type IN ('image/png','image/jpeg','image/gif','image/webp') RETURNING id",
        )
        .bind(file)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if usable.is_none() {
            return Ok(UserAvatarOutcome::NotUsable);
        }
    }
    sqlx::query("UPDATE users SET avatar_file_id=$1,updated_at=NOW() WHERE id=$2")
        .bind(avatar_file_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if let Some(old) = previous.filter(|old| Some(*old) != avatar_file_id) {
        crate::backend::db::postgres::files::delete_unreferenced_file_in_transaction(&mut tx, old)
            .await?;
    }
    tx.commit().await?;
    Ok(UserAvatarOutcome::Applied(previous))
}

pub async fn update_user_profile(
    pool: &PgPool,
    user_id: i64,
    session: &str,
    display_name: Option<&str>,
    bio: Option<&str>,
    location: Option<&str>,
    website: Option<&str>,
) -> Result<Option<User>> {
    let mut tx = pool.begin().await?;
    super::transitions::lock_membership_shared(&mut tx).await?;
    let active: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password FOR UPDATE")
        .bind(user_id).fetch_optional(&mut *tx).await?;
    if active.is_none() || !super::transitions::live_session(&mut tx, user_id, session).await? {
        return Ok(None);
    }
    let result = sqlx::query_as::<_, User>(
        r#"
        UPDATE users SET
            display_name = COALESCE($2, display_name),
            bio = COALESCE($3, bio),
            location = COALESCE($4, location),
            website = COALESCE($5, website),
            updated_at = NOW()
        WHERE id = $1 AND NOT is_banned AND NOT must_change_password
        RETURNING *
        "#,
    )
    .bind(user_id)
    .bind(display_name)
    .bind(bio)
    .bind(location)
    .bind(website)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn count_matching_users(pool: &PgPool, text: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE strpos(lower(username),$1)>0 OR strpos(lower(COALESCE(email,'')),$1)>0")
        .bind(text.trim().to_lowercase()).fetch_one(pool).await?)
}

pub async fn list_admin_users(
    pool: &PgPool,
    text: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AdminUser>> {
    Ok(sqlx::query_as("SELECT id,username,email,is_admin,is_banned,must_change_password,two_factor_enabled,created_at FROM users WHERE strpos(lower(username),$1)>0 OR strpos(lower(COALESCE(email,'')),$1)>0 ORDER BY username,id LIMIT $2 OFFSET $3")
        .bind(text.trim().to_lowercase()).bind(limit.clamp(1,100)).bind(offset.max(0)).fetch_all(pool).await?)
}

pub async fn admin_totals(pool: &PgPool) -> Result<(i64, i64, i64)> {
    Ok(sqlx::query_as("SELECT (SELECT COUNT(*) FROM users), (SELECT COUNT(*) FROM users WHERE is_banned), ((SELECT COALESCE(SUM(size_bytes),0) FROM repositories) + (SELECT COALESCE(SUM(file_size),0) FROM files WHERE is_active))::BIGINT")
        .fetch_one(pool).await?)
}

pub async fn recent_admin_accounts(pool: &PgPool) -> Result<Vec<AdminAccountSummary>> {
    Ok(sqlx::query_as("SELECT id,username,email,is_admin,is_banned,must_change_password,two_factor_enabled,created_at,0::BIGINT AS total_files,0::BIGINT AS total_bytes,0::BIGINT AS commits_total,0::BIGINT AS pushes_total,0::BIGINT AS contributions_total FROM users ORDER BY created_at DESC,id DESC LIMIT 5")
        .fetch_all(pool).await?)
}

pub async fn list_admin_account_summaries(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<Vec<AdminAccountSummary>> {
    let accounts = sqlx::query_as::<_, AdminAccountSummary>(
        r#"
        SELECT
            u.id,
            u.username,
            u.email,
            u.is_admin,
            u.is_banned,
            u.must_change_password,
            u.two_factor_enabled,
            u.created_at,
            storage.total_files,
            (storage.total_bytes + repo_storage.total_repo_bytes)::BIGINT AS total_bytes,
            commits.commits_total,
            pushes.pushes_total,
            (
                commits.commits_total
                + pushes.pushes_total
                + prs.pr_total
                + issues.issue_total
                + issue_comments.issue_comment_total
                + pr_comments.pr_comment_total
                + pr_reviews.pr_review_total
                + releases.release_total
                + repos.repo_total
            )::BIGINT AS contributions_total
        FROM users u
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS total_files, COALESCE(SUM(f.file_size), 0)::BIGINT AS total_bytes
            FROM files f
            WHERE f.user_id = u.id AND f.is_active = TRUE
        ) storage ON TRUE
        LEFT JOIN LATERAL (
            SELECT COALESCE(SUM(r.size_bytes), 0)::BIGINT AS total_repo_bytes
            FROM repositories r
            WHERE r.owner_kind = 'user' AND r.owner_user_id = u.id
        ) repo_storage ON TRUE
        LEFT JOIN LATERAL (
            SELECT COALESCE(SUM(rc.commit_count), 0)::BIGINT AS commits_total
            FROM repository_pushes rc
            WHERE rc.actor_id = u.id
        ) commits ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS pushes_total
            FROM ci_runs cr
            WHERE cr.triggered_by_id = u.id AND cr.event = 'push'
        ) pushes ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS pr_total
            FROM pull_requests pr
            WHERE pr.author_id = u.id
        ) prs ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS issue_total
            FROM issues i
            WHERE i.author_id = u.id
        ) issues ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS issue_comment_total
            FROM issue_comments ic
            WHERE ic.author_id = u.id
        ) issue_comments ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS pr_comment_total
            FROM pr_comments pc
            WHERE pc.author_id = u.id
            AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = pc.review_id AND v.state = 'pending')
        ) pr_comments ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS pr_review_total
            FROM pr_reviews prv
            WHERE prv.author_id = u.id
            AND prv.state <> 'pending'
        ) pr_reviews ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS release_total
            FROM releases rel
            WHERE rel.author_id = u.id
        ) releases ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::BIGINT AS repo_total
            FROM repositories r
            WHERE r.owner_kind = 'user' AND r.owner_user_id = u.id
        ) repos ON TRUE
        ORDER BY contributions_total DESC, u.created_at ASC, u.id ASC
        LIMIT $1 OFFSET $2
        "#,
    )
    .bind(limit.clamp(1,100)).bind(offset.max(0))
    .fetch_all(pool)
    .await?;

    Ok(accounts)
}

pub async fn list_admin_activity_points(
    pool: &PgPool,
    days: i32,
) -> Result<Vec<AdminActivityPoint>> {
    let bounded_days = days.clamp(7, 730);

    let points = sqlx::query_as::<_, AdminActivityPoint>(
        r#"
        WITH day_span AS (
            SELECT generate_series(
                (CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day')::DATE,
                CURRENT_DATE,
                INTERVAL '1 day'
            )::DATE AS day
        ),
        contribution_daily AS (
            SELECT day, SUM(weight)::BIGINT AS daily_contributions
            FROM (
                SELECT DATE(cr.created_at) AS day, COUNT(*)::BIGINT AS weight
                FROM ci_runs cr
                WHERE cr.triggered_by_id IS NOT NULL
                  AND cr.event = 'push'
                  AND cr.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(cr.created_at)
                UNION ALL
                SELECT DATE(pr.created_at) AS day, COUNT(*)::BIGINT AS weight
                FROM pull_requests pr
                WHERE pr.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(pr.created_at)
                UNION ALL
                SELECT DATE(i.created_at) AS day, COUNT(*)::BIGINT AS weight
                FROM issues i
                WHERE i.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(i.created_at)
                UNION ALL
                SELECT DATE(ic.created_at) AS day, COUNT(*)::BIGINT AS weight
                FROM issue_comments ic
                WHERE ic.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(ic.created_at)
                UNION ALL
                SELECT DATE(pc.created_at) AS day, COUNT(*)::BIGINT AS weight
                FROM pr_comments pc
                WHERE NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = pc.review_id AND v.state = 'pending') AND pc.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(pc.created_at)
                UNION ALL
                SELECT DATE(prv.created_at) AS day, COUNT(*)::BIGINT AS weight
                FROM pr_reviews prv
                WHERE prv.state <> 'pending' AND prv.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(prv.created_at)
                UNION ALL
                SELECT DATE(rel.created_at) AS day, COUNT(*)::BIGINT AS weight
                FROM releases rel
                WHERE rel.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(rel.created_at)
                UNION ALL
                SELECT DATE(r.created_at) AS day, COUNT(*)::BIGINT AS weight
                FROM repositories r
                WHERE r.owner_kind = 'user'
                  AND r.owner_user_id IS NOT NULL
                  AND r.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(r.created_at)
                UNION ALL
                SELECT DATE(rc.created_at) AS day, COALESCE(SUM(rc.commit_count), 0)::BIGINT AS weight
                FROM repository_pushes rc
                WHERE rc.created_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                GROUP BY DATE(rc.created_at)
            ) contribution_events
            GROUP BY day
        ),
        repo_storage_recent AS (
            SELECT id, repo_id, recorded_at, size_bytes FROM repo_storage_history
            WHERE recorded_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
        ),
        repo_storage_baseline AS (
            -- Only repositories with in-window snapshots need a preceding size.
            SELECT baseline.*
            FROM (SELECT DISTINCT repo_id FROM repo_storage_recent) recent
            CROSS JOIN LATERAL (
                SELECT id, repo_id, recorded_at, size_bytes FROM repo_storage_history
                WHERE repo_id = recent.repo_id
                  AND recorded_at < CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
                ORDER BY recorded_at DESC, id DESC LIMIT 1
            ) baseline
        ),
        repo_storage_events AS (
            SELECT
                DATE(h.recorded_at) AS day,
                h.recorded_at,
                h.size_bytes,
                LAG(h.size_bytes) OVER (PARTITION BY h.repo_id ORDER BY h.recorded_at, h.id) AS previous_size_bytes
            FROM (
                SELECT id, repo_id, recorded_at, size_bytes FROM repo_storage_baseline
                UNION ALL
                SELECT id, repo_id, recorded_at, size_bytes FROM repo_storage_recent
            ) h
        ),
        repo_storage_daily AS (
            SELECT
                day,
                SUM(GREATEST(size_bytes - COALESCE(previous_size_bytes, 0), 0))::BIGINT AS daily_repo_storage_bytes
            FROM repo_storage_events
            WHERE recorded_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
            GROUP BY day
        ),
        file_storage_daily AS (
            SELECT
                DATE(ul.uploaded_at) AS day,
                COALESCE(SUM(ul.file_size), 0)::BIGINT AS daily_file_storage_bytes
            FROM upload_rate_limit ul
            WHERE ul.uploaded_at >= CURRENT_DATE - ($1::INT - 1) * INTERVAL '1 day'
            GROUP BY DATE(ul.uploaded_at)
        ),
        storage_daily AS (
            SELECT
                d.day,
                (
                    COALESCE(rs.daily_repo_storage_bytes, 0)
                    + COALESCE(fs.daily_file_storage_bytes, 0)
                )::BIGINT AS daily_storage_bytes
            FROM day_span d
            LEFT JOIN repo_storage_daily rs ON rs.day = d.day
            LEFT JOIN file_storage_daily fs ON fs.day = d.day
        )
        SELECT
            d.day,
            COALESCE(c.daily_contributions, 0)::BIGINT AS daily_contributions,
            SUM(COALESCE(c.daily_contributions, 0)) OVER (ORDER BY d.day ASC)::BIGINT AS cumulative_contributions,
            COALESCE(s.daily_storage_bytes, 0)::BIGINT AS daily_storage_bytes,
            SUM(COALESCE(s.daily_storage_bytes, 0)) OVER (ORDER BY d.day ASC)::BIGINT AS cumulative_storage_bytes
        FROM day_span d
        LEFT JOIN contribution_daily c ON c.day = d.day
        LEFT JOIN storage_daily s ON s.day = d.day
        ORDER BY d.day ASC
        "#,
    )
    .bind(bounded_days)
    .fetch_all(pool)
    .await?;

    Ok(points)
}

pub async fn count_unbanned_admin_users(pool: &PgPool) -> Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM users WHERE is_admin = TRUE AND is_banned = FALSE",
    )
    .fetch_one(pool)
    .await?;

    Ok(count)
}

/// Decode the author that discussion reads join in. The join is inner: each discussion's
/// `author_id` is NOT NULL and references `users`, so every row has an author.
pub(crate) fn author_display(
    row: &sqlx::postgres::PgRow,
) -> Result<crate::backend::db::users::accounts::AuthorDisplay, sqlx::Error> {
    use crate::backend::db::users::accounts::AuthorDisplay;
    use sqlx::Row;
    Ok(AuthorDisplay {
        id: row.try_get("display_id")?,
        username: row.try_get("display_username")?,
        avatar_file_id: row.try_get("display_avatar_file_id")?,
        display_name: row.try_get("display_name")?,
        bio: row.try_get("display_bio")?,
        location: row.try_get("display_location")?,
        website: row.try_get("display_website")?,
        is_admin: row.try_get("display_is_admin")?,
        created_at: row.try_get("display_created_at")?,
    })
}

pub(crate) const AUTHOR_DISPLAY_COLUMNS: &str = "u.id AS display_id, u.username AS display_username, u.avatar_file_id AS display_avatar_file_id, u.display_name AS display_name, u.bio AS display_bio, u.location AS display_location, u.website AS display_website, u.is_admin AS display_is_admin, u.created_at AS display_created_at";
