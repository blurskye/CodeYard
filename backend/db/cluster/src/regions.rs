//! Organization presence regions from counted member requests. A directory-enrolled
//! PostgreSQL cell evaluates its own organizations once an hour on one of its web nodes;
//! writes always stay in the home cell, presence only moves reads.
use anyhow::{ensure, Result};
use codeyard_db_core::{cells::directory::Refusal, pool::PgPool, queries::credentials::Actor};
use codeyard_support::cluster::PresencePolicy;

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

fn decode(raw: Option<String>) -> Result<PresencePolicy> {
    let policy = match raw {
        Some(raw) => serde_json::from_str(&raw)?,
        None => PresencePolicy::default(),
    };
    policy.validate().map_err(anyhow::Error::msg)?;
    Ok(policy)
}

pub async fn policy(pool: &PgPool) -> Result<PresencePolicy> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT configuration::text FROM cell_region_policy WHERE id=1")
            .fetch_optional(pool)
            .await?;
    decode(raw)
}

/// The administrator is checked in the writing transaction, before and after the write.
pub async fn save(pool: &PgPool, actor: &Actor, policy: &PresencePolicy) -> Result<()> {
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
pub async fn due(pool: &PgPool) -> Result<Option<PresencePolicy>> {
    let raw: Option<String> = sqlx::query_scalar("INSERT INTO cell_region_policy(id,configuration,evaluated_at) VALUES(1,$1,clock_timestamp()) ON CONFLICT(id) DO UPDATE SET evaluated_at=clock_timestamp() WHERE cell_region_policy.evaluated_at IS NULL OR cell_region_policy.evaluated_at<=clock_timestamp()-INTERVAL '1 hour' RETURNING configuration::text")
        .bind(serde_json::to_value(PresencePolicy::default())?)
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
pub async fn evaluate(pool: &PgPool, policy: &PresencePolicy) -> Result<Vec<Change>> {
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
