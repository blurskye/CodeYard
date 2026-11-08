//! Organization presence regions from counted member requests. A directory-enrolled
//! PostgreSQL cell evaluates its own organizations once an hour on one of its web nodes;
//! writes always stay in the home cell, presence only moves reads.
use anyhow::{ensure, Result};
use codeyard_database::{cells::directory::Refusal, pool::PgPool, queries::credentials::Actor};
use codeyard_types::cluster::RegionPolicy;

/// Additions and removals considered in one evaluation.
pub const MAXIMUM_CHANGES: i64 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub organization: i64,
    pub region: String,
    pub present: bool,
    /// True when the directory changed; false in dry run, when the directory refused, and
    /// when it already matched.
    pub applied: bool,
}

fn decode(raw: Option<String>) -> Result<RegionPolicy> {
    let policy = match raw {
        Some(raw) => serde_json::from_str(&raw)?,
        None => RegionPolicy::default(),
    };
    policy.validate().map_err(anyhow::Error::msg)?;
    Ok(policy)
}

pub async fn policy(pool: &PgPool) -> Result<RegionPolicy> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT configuration::text FROM cell_region_policy WHERE id=1")
            .fetch_optional(pool)
            .await?;
    decode(raw)
}

/// The administrator is checked in the writing transaction, before and after the write.
pub async fn save(pool: &PgPool, actor: &Actor, policy: &RegionPolicy) -> Result<()> {
    policy.validate().map_err(anyhow::Error::msg)?;
    let configuration = serde_json::to_value(policy)?;
    let mut tx = pool.begin().await?;
    ensure!(
        codeyard_db_identity::postgres::users::transitions::active_administrator(&mut tx, actor)
            .await?,
        crate::drain::Refusal::Denied
    );
    sqlx::query("INSERT INTO cell_region_policy(id,configuration) VALUES(1,$1) ON CONFLICT(id) DO UPDATE SET configuration=EXCLUDED.configuration")
        .bind(&configuration)
        .execute(&mut *tx)
        .await?;
    ensure!(
        codeyard_db_identity::postgres::users::transitions::active_administrator(&mut tx, actor)
            .await?,
        crate::drain::Refusal::Denied
    );
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,metadata) VALUES($1,'regions.policy','site',$2)")
        .bind(actor.user_id)
        .bind(&configuration)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The policy when an hour has passed since any web node of this cell last evaluated it.
/// The conditional update lets exactly one node take each hour.
pub async fn due(pool: &PgPool) -> Result<Option<RegionPolicy>> {
    let raw: Option<String> = sqlx::query_scalar("INSERT INTO cell_region_policy(id,configuration,evaluated_at) VALUES(1,$1,clock_timestamp()) ON CONFLICT(id) DO UPDATE SET evaluated_at=clock_timestamp() WHERE cell_region_policy.evaluated_at IS NULL OR cell_region_policy.evaluated_at<=clock_timestamp()-INTERVAL '1 hour' RETURNING configuration::text")
        .bind(serde_json::to_value(RegionPolicy::default())?)
        .fetch_optional(pool)
        .await?;
    raw.map(|raw| decode(Some(raw))).transpose()
}

// Shares are computed per day over the window ending that day, for each of the last
// `$1` UTC days. Totals include every region, the home region `$6` too, which is never added.
const ADDITIONS: &str = "WITH days AS (SELECT ((clock_timestamp() AT TIME ZONE 'UTC')::date-g) AS day FROM generate_series(0,$1::int-1) AS g),
shares AS (SELECT m.org_id,c.region,d.day,SUM(c.requests) AS requests FROM days d
    JOIN cell_request_counts c ON c.day>d.day-$2::int AND c.day<=d.day
    JOIN org_memberships m ON m.user_id=c.user_id
    JOIN organizations o ON o.id=m.org_id AND NOT o.is_stub
    GROUP BY m.org_id,c.region,d.day),
totals AS (SELECT org_id,day,SUM(requests) AS total FROM shares GROUP BY org_id,day)
SELECT s.org_id,s.region FROM shares s JOIN totals t ON t.org_id=s.org_id AND t.day=s.day
WHERE t.total>=$3::bigint AND s.requests*100>=t.total*$4::int AND s.region<>$6::text
    AND NOT EXISTS(SELECT 1 FROM cell_presence p WHERE p.organization_id=s.org_id AND p.region=s.region)
GROUP BY s.org_id,s.region HAVING COUNT(*)=$1::int
ORDER BY s.org_id,s.region LIMIT $5";

// A presence is removed when no day of the last `$1` had its share at `$3` percent or more.
// A day without counted requests counts as below.
const REMOVALS: &str = "SELECT p.organization_id,p.region FROM cell_presence p
WHERE p.added_at<=clock_timestamp()-$1::int*INTERVAL '1 day'
AND NOT EXISTS(SELECT 1 FROM generate_series(0,$1::int-1) AS g
    CROSS JOIN LATERAL (SELECT COALESCE(SUM(c.requests),0) AS total,
        COALESCE(SUM(c.requests) FILTER (WHERE c.region=p.region),0) AS here
        FROM cell_request_counts c JOIN org_memberships m ON m.user_id=c.user_id AND m.org_id=p.organization_id
        WHERE c.day>(clock_timestamp() AT TIME ZONE 'UTC')::date-g-$2::int
            AND c.day<=(clock_timestamp() AT TIME ZONE 'UTC')::date-g) s
    WHERE s.total>0 AND s.here*100>=s.total*$3::int)
ORDER BY p.organization_id,p.region LIMIT $4";

/// Adds and removes presence for this cell's organizations. Paused does nothing; dry run
/// records "would add" and "would remove" audit entries and changes nothing. Each applied
/// change is audited; the directory call comes first, so a failure repeats next hour.
pub async fn evaluate(pool: &PgPool, policy: &RegionPolicy) -> Result<Vec<Change>> {
    policy.validate().map_err(anyhow::Error::msg)?;
    let Some(directory) = pool.directory() else {
        return Ok(Vec::new());
    };
    if policy.paused {
        return Ok(Vec::new());
    }
    let home = directory
        .cell(pool.cell_prefix())
        .await?
        .ok_or(Refusal::Unavailable)?
        .region;
    let additions: Vec<(i64, String)> = sqlx::query_as(ADDITIONS)
        .bind(policy.held_days)
        .bind(policy.window_days)
        .bind(policy.minimum_requests)
        .bind(policy.share_percent)
        .bind(MAXIMUM_CHANGES)
        .bind(&home)
        .fetch_all(pool)
        .await?;
    let removals: Vec<(i64, String)> = sqlx::query_as(REMOVALS)
        .bind(policy.remove_days)
        .bind(policy.window_days)
        .bind(policy.remove_percent)
        .bind(MAXIMUM_CHANGES)
        .fetch_all(pool)
        .await?;
    let mut changes = Vec::new();
    let candidates = additions
        .into_iter()
        .map(|(organization, region)| (organization, region, true))
        .chain(
            removals
                .into_iter()
                .map(|(organization, region)| (organization, region, false)),
        );
    for (organization, region, present) in candidates {
        // `settled` means the directory now matches, so the local row follows it, also when
        // the region was already set by hand.
        let (settled, applied) = if policy.dry_run {
            (false, false)
        } else {
            match directory.set_presence(organization, &region, present).await {
                Ok(changed) => (true, changed),
                // This organization is gone from the directory or at its region limit.
                Err(error) if matches!(error.downcast_ref::<Refusal>(), Some(Refusal::Denied)) => {
                    (false, false)
                }
                Err(error) => return Err(error),
            }
        };
        let mut tx = pool.begin().await?;
        if settled && present {
            sqlx::query("INSERT INTO cell_presence(organization_id,region) SELECT id,$2 FROM organizations WHERE id=$1 AND NOT is_stub ON CONFLICT DO NOTHING")
                .bind(organization)
                .bind(&region)
                .execute(&mut *tx)
                .await?;
        } else if settled {
            sqlx::query("DELETE FROM cell_presence WHERE organization_id=$1 AND region=$2")
                .bind(organization)
                .bind(&region)
                .execute(&mut *tx)
                .await?;
        }
        let action = match (present, policy.dry_run) {
            (true, false) => "regions.presence_added",
            (false, false) => "regions.presence_removed",
            (true, true) => "regions.presence_would_add",
            (false, true) => "regions.presence_would_remove",
        };
        if policy.dry_run || applied {
            sqlx::query("INSERT INTO audit_log(org_id,action,target_type,target_id,metadata) SELECT id,$2,'organization',id,$3 FROM organizations WHERE id=$1")
                .bind(organization)
                .bind(action)
                .bind(serde_json::json!({"region": region}))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        changes.push(Change {
            organization,
            region,
            present,
            applied,
        });
    }
    Ok(changes)
}

/// A user account that the region policy moves, and the region it moves to.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct UserMove {
    pub user_id: i64,
    pub login: String,
    pub region: String,
}

// Native, unpinned users past their cooldown and not moving already, whose own counted
// requests from one other region met the share of at least the minimum on each of the last
// `$1` days. Shares are computed as for presence above; `$6` is this cell's region.
const USER_MOVES: &str = "WITH days AS (SELECT ((clock_timestamp() AT TIME ZONE 'UTC')::date-g) AS day FROM generate_series(0,$1::int-1) AS g),
shares AS (SELECT c.user_id,c.region,d.day,SUM(c.requests) AS requests FROM days d
    JOIN cell_request_counts c ON c.day>d.day-$2::int AND c.day<=d.day
    GROUP BY c.user_id,c.region,d.day),
totals AS (SELECT user_id,day,SUM(requests) AS total FROM shares GROUP BY user_id,day)
SELECT u.id AS user_id,u.username AS login,s.region FROM shares s JOIN totals t ON t.user_id=s.user_id AND t.day=s.day
JOIN users u ON u.id=s.user_id AND NOT u.is_stub AND NOT u.region_pinned
    AND (u.region_moved_at IS NULL OR u.region_moved_at<=clock_timestamp()-$7::int*INTERVAL '1 day')
WHERE t.total>=$3::bigint AND s.requests*100>=t.total*$4::int AND s.region<>$6::text
    AND NOT EXISTS(SELECT 1 FROM account_moves m WHERE m.account_kind='user' AND m.account_id=u.id)
GROUP BY u.id,u.username,s.region HAVING COUNT(*)=$1::int
ORDER BY u.id LIMIT $5";

/// The user accounts the policy would move from this cell, whose region is `home`.
pub async fn user_moves(pool: &PgPool, policy: &RegionPolicy, home: &str) -> Result<Vec<UserMove>> {
    policy.validate().map_err(anyhow::Error::msg)?;
    Ok(sqlx::query_as(USER_MOVES)
        .bind(policy.move_held_days)
        .bind(policy.move_window_days)
        .bind(policy.move_minimum_requests)
        .bind(policy.move_share_percent)
        .bind(MAXIMUM_CHANGES)
        .bind(home)
        .bind(policy.move_cooldown_days)
        .fetch_all(pool)
        .await?)
}

/// Starts the account's cooldown and records the move in one transaction, so a failing
/// move is not tried again every hour and counts toward the daily limit. False when the
/// account was pinned, moved or claimed meanwhile.
pub async fn claim_user_move(
    pool: &PgPool,
    user: i64,
    cooldown_days: i32,
    region: &str,
    cell: u16,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let claimed = sqlx::query("UPDATE users SET region_moved_at=clock_timestamp() WHERE id=$1 AND NOT is_stub AND NOT region_pinned AND (region_moved_at IS NULL OR region_moved_at<=clock_timestamp()-$2::int*INTERVAL '1 day')")
        .bind(user)
        .bind(cooldown_days)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if claimed {
        audit_user_move(&mut tx, user_move_action(false), user, region, cell).await?;
    }
    tx.commit().await?;
    Ok(claimed)
}

/// Records the move that dry run would start.
pub async fn audit_would_move(pool: &PgPool, user: i64, region: &str, cell: u16) -> Result<()> {
    let mut connection = pool.acquire().await?;
    audit_user_move(&mut connection, user_move_action(true), user, region, cell).await
}

async fn audit_user_move(
    connection: &mut sqlx::PgConnection,
    action: &str,
    user: i64,
    region: &str,
    cell: u16,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_log(action,target_type,target_id,metadata) VALUES($1,'user',$2,$3)",
    )
    .bind(action)
    .bind(user)
    .bind(serde_json::json!({"region": region, "cell": cell}))
    .execute(connection)
    .await?;
    Ok(())
}

/// Pins or unpins a native user account; a pinned account never moves automatically.
pub async fn pin_user(pool: &PgPool, login: &str, pinned: bool) -> Result<()> {
    let changed = sqlx::query(
        "UPDATE users SET region_pinned=$2 WHERE lower(username)=lower($1) AND NOT is_stub",
    )
    .bind(login)
    .bind(pinned)
    .execute(pool)
    .await?
    .rows_affected();
    ensure!(changed == 1, "No such user in this cell");
    Ok(())
}

fn user_move_action(dry_run: bool) -> &'static str {
    if dry_run {
        "regions.user_would_move"
    } else {
        "regions.user_moved"
    }
}

/// How many automatic moves, or dry-run moves, this cell started to `region` today (UTC).
pub async fn user_moves_today(pool: &PgPool, dry_run: bool, region: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action=$1 AND target_type='user' AND metadata->>'region'=$2 AND created_at>=((clock_timestamp() AT TIME ZONE 'UTC')::date)::timestamp AT TIME ZONE 'UTC'")
        .bind(user_move_action(dry_run))
        .bind(region)
        .fetch_one(pool)
        .await?)
}

/// The automatic user moves of this cell that stopped before they finished, with the
/// target cell. The hourly evaluation runs each again, since every move step repeats safely.
pub async fn unfinished_user_moves(pool: &PgPool) -> Result<Vec<(i64, String, i32)>> {
    Ok(sqlx::query_as("SELECT u.id,u.username,m.target_cell FROM account_moves m JOIN users u ON u.id=m.account_id WHERE m.account_kind='user' AND m.state<>'receiving' AND u.region_moved_at IS NOT NULL ORDER BY m.account_id LIMIT $1")
        .bind(MAXIMUM_CHANGES)
        .fetch_all(pool)
        .await?)
}
