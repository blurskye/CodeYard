use crate::backend::db::models::{AdminAccountSummary, AdminActivityPoint, AdminUser, User};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn create_user(pool: &SqlitePool, username: &str, email: Option<&str>) -> Result<User> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::transitions::require_user_namespace(&mut tx, username).await?;
    let user = sqlx::query_as("INSERT INTO users(username,email) VALUES (?1,?2) RETURNING *")
        .bind(username)
        .bind(email)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(user)
}

pub async fn create_user_with_password(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
) -> Result<User> {
    create_user_with_password_and_flags(pool, username, email, password_hash, false, false).await
}

pub async fn create_user_with_password_and_flags(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
    is_admin: bool,
    must_change_password: bool,
) -> Result<User> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::transitions::require_user_namespace(&mut tx, username).await?;
    let user = sqlx::query_as("INSERT INTO users(username,email,password_hash,is_admin,must_change_password) VALUES (?1,?2,?3,?4,?5) RETURNING *")
        .bind(username).bind(email).bind(password_hash).bind(is_admin).bind(must_change_password)
        .fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(user)
}

pub async fn count_users(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?)
}

pub async fn replace_password_hash(
    pool: &SqlitePool,
    user_id: i64,
    old: &str,
    new: &str,
) -> Result<bool> {
    let changed = sqlx::query("UPDATE users SET password_hash=?1 WHERE id=?2 AND password_hash=?3")
        .bind(new)
        .bind(user_id)
        .bind(old)
        .execute(pool)
        .await?;
    Ok(changed.rows_affected() == 1)
}

pub async fn update_user_password(
    pool: &SqlitePool,
    user_id: i64,
    password_hash: &str,
    must_change_password: bool,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let organizations =
        super::transitions::owned_organizations_for_account_change(&mut tx, user_id).await?;
    sqlx::query("UPDATE users SET password_hash=?1,must_change_password=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?3")
        .bind(password_hash).bind(must_change_password).bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM sessions WHERE user_id=?1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM personal_access_tokens WHERE user_id=?1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if must_change_password {
        // A forced change locks the account out of CI like a ban does.
        crate::backend::db::sqlite::ci::stop_actor_ci_runs(&mut tx, user_id).await?;
        super::transitions::stop_ownerless_organization_ci(&mut tx, &organizations).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn get_user_by_id(pool: &SqlitePool, id: i64) -> Result<Option<User>> {
    Ok(sqlx::query_as("SELECT * FROM users WHERE id=?1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn get_user_by_username(pool: &SqlitePool, username: &str) -> Result<Option<User>> {
    Ok(sqlx::query_as("SELECT * FROM users WHERE username=?1")
        .bind(username)
        .fetch_optional(pool)
        .await?)
}

pub async fn get_user_by_email(pool: &SqlitePool, email: &str) -> Result<Option<User>> {
    Ok(sqlx::query_as("SELECT * FROM users WHERE email=?1")
        .bind(email)
        .fetch_optional(pool)
        .await?)
}

pub async fn get_users_by_emails(pool: &SqlitePool, emails: &[String]) -> Result<Vec<User>> {
    if emails.is_empty() {
        return Ok(vec![]);
    }
    Ok(
        sqlx::query_as("SELECT * FROM users WHERE email IN (SELECT value FROM json_each(?1))")
            .bind(serde_json::to_string(emails)?)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_users_by_usernames(pool: &SqlitePool, usernames: &[String]) -> Result<Vec<User>> {
    if usernames.is_empty() {
        return Ok(vec![]);
    }
    Ok(
        sqlx::query_as("SELECT * FROM users WHERE username IN (SELECT value FROM json_each(?1))")
            .bind(serde_json::to_string(usernames)?)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn get_users_by_ids(pool: &SqlitePool, ids: &[i64]) -> Result<Vec<User>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(
        sqlx::query_as("SELECT * FROM users WHERE id IN (SELECT value FROM json_each(?1))")
            .bind(serde_json::to_string(ids)?)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<User>> {
    Ok(sqlx::query_as("SELECT * FROM users ORDER BY username")
        .fetch_all(pool)
        .await?)
}

pub async fn create_user_from_oauth(
    pool: &SqlitePool,
    username: &str,
    email: Option<&str>,
    display_name: Option<&str>,
) -> Result<User> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::transitions::require_user_namespace(&mut tx, username).await?;
    let user = sqlx::query_as("INSERT INTO users(username,email,display_name,email_verified) VALUES (?1,?2,?3,?4) RETURNING *")
        .bind(username).bind(email).bind(display_name).bind(email.is_some()).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(user)
}

pub async fn set_user_avatar(
    pool: &SqlitePool,
    user_id: i64,
    avatar_file_id: Option<uuid::Uuid>,
) -> Result<crate::backend::db::users::accounts::UserAvatarOutcome> {
    use crate::backend::db::users::accounts::UserAvatarOutcome;

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let account: Option<(Option<uuid::Uuid>, bool)> =
        sqlx::query_as("SELECT avatar_file_id,is_banned FROM users WHERE id=?1")
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
            "UPDATE files SET is_active=TRUE,upload_expires_at=NULL,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND user_id=?2 AND (is_active OR (confirmed_at IS NOT NULL AND upload_expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))) AND reason='avatar' AND content_type IN ('image/png','image/jpeg','image/gif','image/webp') RETURNING id",
        )
        .bind(file)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if usable.is_none() {
            return Ok(UserAvatarOutcome::NotUsable);
        }
    }
    sqlx::query("UPDATE users SET avatar_file_id=?1,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?2")
        .bind(avatar_file_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if let Some(old) = previous.filter(|old| Some(*old) != avatar_file_id) {
        crate::backend::db::sqlite::files::delete_unreferenced_file_in_transaction(&mut tx, old)
            .await?;
    }
    tx.commit().await?;
    Ok(UserAvatarOutcome::Applied(previous))
}

pub async fn update_user_profile(
    pool: &SqlitePool,
    user_id: i64,
    session: &str,
    display_name: Option<&str>,
    bio: Option<&str>,
    location: Option<&str>,
    website: Option<&str>,
) -> Result<Option<User>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !super::transitions::live_session(&mut tx, user_id, session).await? {
        return Ok(None);
    }
    let user = sqlx::query_as("UPDATE users SET display_name=COALESCE(?2,display_name),bio=COALESCE(?3,bio),location=COALESCE(?4,location),website=COALESCE(?5,website),updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND NOT is_banned AND NOT must_change_password RETURNING *")
        .bind(user_id).bind(display_name).bind(bio).bind(location).bind(website).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(user)
}

/// Accounts that match the search: all of them, the administrators and the banned ones.
pub async fn count_matching_users(pool: &SqlitePool, text: &str) -> Result<(i64, i64, i64)> {
    Ok(sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(is_admin),0), COALESCE(SUM(is_banned),0) FROM users WHERE username REGEXP ?1 OR COALESCE(email,'') REGEXP ?1",
    )
    .bind(super::super::search::contains(text.trim()))
    .fetch_one(pool)
    .await?)
}

pub async fn list_admin_users(
    pool: &SqlitePool,
    text: &str,
    role: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AdminUser>> {
    Ok(sqlx::query_as("SELECT id,username,email,is_admin,is_banned,must_change_password,two_factor_enabled,created_at FROM users WHERE (username REGEXP ?1 OR COALESCE(email,'') REGEXP ?1) AND CASE ?4 WHEN 'admins' THEN is_admin WHEN 'banned' THEN is_banned ELSE 1 END ORDER BY username,id LIMIT ?2 OFFSET ?3")
        .bind(super::super::search::contains(text.trim())).bind(limit.clamp(1,100)).bind(offset.max(0)).bind(role).fetch_all(pool).await?)
}

pub async fn list_admin_account_summaries(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
) -> Result<Vec<AdminAccountSummary>> {
    // Independent correlated aggregates avoid multiplying storage/counts by the
    // cardinality of other joins. No PostgreSQL LATERAL or float quota arithmetic.
    Ok(sqlx::query_as(r#"WITH totals AS (
        SELECT u.id,u.username,u.email,u.is_admin,u.is_banned,u.must_change_password,u.two_factor_enabled,u.created_at,
            (SELECT COUNT(*) FROM files f WHERE f.user_id=u.id AND f.is_active) AS total_files,
            COALESCE((SELECT SUM(bytes) FROM (
                SELECT f.file_size AS bytes FROM files f WHERE f.user_id=u.id AND f.is_active
                UNION ALL SELECT r.size_bytes FROM repositories r WHERE r.owner_kind='user' AND r.owner_user_id=u.id
            )),0) AS total_bytes,
            COALESCE((SELECT SUM(c.commit_count) FROM repository_pushes c WHERE c.actor_id=u.id),0) AS commits_total,
            (SELECT COUNT(*) FROM ci_runs c WHERE c.triggered_by_id=u.id AND c.event='push') AS pushes_total,
            (SELECT COUNT(*) FROM pull_requests p WHERE p.author_id=u.id)
                + (SELECT COUNT(*) FROM issues i WHERE i.author_id=u.id)
                + (SELECT COUNT(*) FROM issue_comments c WHERE c.author_id=u.id)
                + (SELECT COUNT(*) FROM pr_comments c WHERE c.author_id=u.id AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state='pending'))
                + (SELECT COUNT(*) FROM pr_reviews c WHERE c.author_id=u.id AND c.state<>'pending')
                + (SELECT COUNT(*) FROM releases r WHERE r.author_id=u.id)
                + (SELECT COUNT(*) FROM repositories r WHERE r.owner_kind='user' AND r.owner_user_id=u.id) AS other_contributions
        FROM users u)
        SELECT id,username,email,is_admin,is_banned,must_change_password,two_factor_enabled,created_at,total_files,total_bytes,
            commits_total,pushes_total,commits_total+pushes_total+other_contributions AS contributions_total
        FROM totals ORDER BY contributions_total DESC,created_at ASC,id ASC LIMIT ?1 OFFSET ?2"#)
        .bind(limit.clamp(1,100)).bind(offset.max(0)).fetch_all(pool).await?)
}

pub async fn list_admin_activity_points(
    pool: &SqlitePool,
    days: i32,
) -> Result<Vec<AdminActivityPoint>> {
    let bounded = days.clamp(7, 730);
    // Stored timestamps are canonical UTC text, so they compare directly with `moment`
    // and keep the timestamp indexes usable. date() only names the output days.
    Ok(sqlx::query_as(r#"WITH RECURSIVE
        window_start(day,moment) AS (
            SELECT date('now',printf('%d days',1-?1)),strftime('%Y-%m-%dT00:00:00.000000Z','now',printf('%d days',1-?1))
        ), day_span(day) AS (
            SELECT day FROM window_start
            UNION ALL SELECT date(day,'+1 day') FROM day_span WHERE day<date('now')
        ), count_events(day,commits,pulls_opened,pulls_merged,issues_opened,issues_closed,runs) AS (
            SELECT date(created_at),COALESCE(SUM(commit_count),0),0,0,0,0,0 FROM repository_pushes,window_start
                WHERE created_at>=moment GROUP BY date(created_at)
            UNION ALL SELECT date(created_at),0,COUNT(*),0,0,0,0 FROM pull_requests,window_start
                WHERE created_at>=moment GROUP BY date(created_at)
            UNION ALL SELECT date(merged_at),0,0,COUNT(*),0,0,0 FROM pull_requests,window_start
                WHERE merged_at>=moment GROUP BY date(merged_at)
            UNION ALL SELECT date(created_at),0,0,0,COUNT(*),0,0 FROM issues,window_start
                WHERE created_at>=moment GROUP BY date(created_at)
            UNION ALL SELECT date(closed_at),0,0,0,0,COUNT(*),0 FROM issues,window_start
                WHERE closed_at>=moment GROUP BY date(closed_at)
            UNION ALL SELECT date(created_at),0,0,0,0,0,COUNT(*) FROM ci_runs,window_start
                WHERE created_at>=moment GROUP BY date(created_at)
        ), count_daily AS (
            SELECT day,SUM(commits) AS commits,SUM(pulls_opened) AS pulls_opened,SUM(pulls_merged) AS pulls_merged,
                SUM(issues_opened) AS issues_opened,SUM(issues_closed) AS issues_closed,SUM(runs) AS runs
            FROM count_events GROUP BY day
        ), repo_storage_recent AS (
            -- Use the time index; without it SQLite scans every snapshot to find the window.
            SELECT id,repo_id,recorded_at,size_bytes
            FROM repo_storage_history INDEXED BY idx_repo_storage_history_recorded_at,window_start
                WHERE recorded_at>=moment
        ), repo_storage_baseline AS (
            -- Only repositories with in-window snapshots need a preceding size.
            SELECT h.id,h.repo_id,h.recorded_at,h.size_bytes
            FROM (SELECT DISTINCT repo_id FROM repo_storage_recent) recent
            JOIN repo_storage_history h ON h.id=(
                SELECT id FROM repo_storage_history,window_start
                WHERE repo_id=recent.repo_id AND recorded_at<moment
                ORDER BY recorded_at DESC,id DESC LIMIT 1
            )
        ), repo_storage_events AS (
            SELECT repo_id,recorded_at,size_bytes,
                LAG(size_bytes) OVER(PARTITION BY repo_id ORDER BY recorded_at,id) AS previous
            FROM (
                SELECT id,repo_id,recorded_at,size_bytes FROM repo_storage_baseline
                UNION ALL SELECT id,repo_id,recorded_at,size_bytes FROM repo_storage_recent
            )
        ), storage_events(day,bytes) AS (
            SELECT date(recorded_at),max(size_bytes-COALESCE(previous,0),0) FROM repo_storage_events,window_start
                WHERE recorded_at>=moment
            UNION ALL SELECT date(uploaded_at),file_size FROM upload_rate_limit,window_start
                WHERE uploaded_at>=moment
        ), storage_daily AS (SELECT day,SUM(bytes) AS daily FROM storage_events GROUP BY day)
        SELECT d.day,COALESCE(c.commits,0) AS commits,COALESCE(c.pulls_opened,0) AS pulls_opened,
            COALESCE(c.pulls_merged,0) AS pulls_merged,COALESCE(c.issues_opened,0) AS issues_opened,
            COALESCE(c.issues_closed,0) AS issues_closed,COALESCE(c.runs,0) AS runs,
            COALESCE(s.daily,0) AS daily_storage_bytes,
            SUM(COALESCE(s.daily,0)) OVER(ORDER BY d.day) AS cumulative_storage_bytes
        FROM day_span d LEFT JOIN count_daily c ON c.day=d.day LEFT JOIN storage_daily s ON s.day=d.day
        ORDER BY d.day"#).bind(bounded).fetch_all(pool).await?)
}

pub async fn count_unbanned_admin_users(pool: &SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin AND NOT is_banned")
            .fetch_one(pool)
            .await?,
    )
}

/// Decode the author that discussion reads join in. The join is inner: each discussion's
/// `author_id` is NOT NULL and references `users`, so every row has an author.
pub(crate) fn author_display(
    row: &sqlx::sqlite::SqliteRow,
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
