use crate::scaling::Action;
use anyhow::{ensure, Result};
use codeyard_database::pool::PgPool;
use codeyard_database::queries::credentials::Actor;
use codeyard_types::cluster::ScalingPolicy;
use sqlx::PgConnection;

pub async fn policies(pool: &PgPool) -> Result<Vec<ScalingPolicy>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT SUBSTR(configuration::text,1,4097) FROM cluster_scaling_policies ORDER BY role LIMIT 4",
    )
    .fetch_all(pool)
    .await?;
    crate::scaling::decode(rows)
}

pub async fn save(pool: &PgPool, actor: &Actor, policy: &ScalingPolicy) -> Result<()> {
    policy.validate().map_err(anyhow::Error::msg)?;
    let mut tx = pool.begin().await?;
    ensure!(
        codeyard_db_identity::postgres::users::transitions::active_administrator(&mut tx, actor)
            .await?,
        crate::drain::Refusal::Denied
    );
    sqlx::query("INSERT INTO cluster_scaling_policies(role,configuration) VALUES($1,$2) ON CONFLICT(role) DO UPDATE SET configuration=EXCLUDED.configuration,trigger_since=NULL")
        .bind(&policy.role).bind(serde_json::to_value(policy)?).execute(&mut *tx).await?;
    ensure!(
        codeyard_db_identity::postgres::users::transitions::active_administrator(&mut tx, actor)
            .await?,
        crate::drain::Refusal::Denied
    );
    sqlx::query("INSERT INTO audit_log(actor_id,action,target_type,metadata) VALUES($1,'scaling.policy','site',$2)")
        .bind(actor.user_id).bind(serde_json::to_value(policy)?).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn counts(conn: &mut PgConnection, role: &str) -> Result<(i32, i32)> {
    let cost: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE status<>'revoked' AND $1=ANY(roles)")
            .bind(role)
            .fetch_one(&mut *conn)
            .await?;
    let reservations: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(max_uses-uses),0) FROM join_tokens WHERE max_uses IS NOT NULL AND uses<max_uses AND expires_at>clock_timestamp() AND $1=ANY(roles)")
        .bind(role).fetch_one(conn).await?;
    Ok((i32::try_from(cost)?, i32::try_from(reservations)?))
}

pub(crate) async fn permit_invite(
    conn: &mut PgConnection,
    leader: i64,
    role: &str,
    count: i32,
) -> Result<()> {
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cluster_leases l JOIN nodes n ON n.id=l.node_id WHERE l.name='scaling' AND l.node_id=$1 AND l.expires_at>clock_timestamp() AND n.status='active' AND n.state='active')")
        .bind(leader).fetch_one(&mut *conn).await?;
    ensure!(live, "scaling lease expired");
    let raw: Option<String> = sqlx::query_scalar(
        "SELECT SUBSTR(configuration::text,1,4097) FROM cluster_scaling_policies WHERE role=$1 FOR UPDATE",
    )
    .bind(role)
    .fetch_optional(&mut *conn)
    .await?;
    let policy: ScalingPolicy = raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_else(|| ScalingPolicy::defaults(role));
    policy.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        !policy.paused && !policy.dry_run,
        "scaling policy is paused or dry-run"
    );
    let (cost, reserved) = counts(conn, role).await?;
    ensure!(
        count > 0
            && i64::from(cost) + i64::from(reserved) + i64::from(count)
                <= i64::from(policy.maximum),
        "scaling maximum reached"
    );
    Ok(())
}

pub async fn evaluate(pool: &PgPool, node: i64) -> Result<Option<Action>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(37005)")
        .execute(&mut *tx)
        .await?;
    let eligible: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND status='active' AND state='active') AND EXISTS(SELECT 1 FROM node_configuration WHERE cluster_enabled)")
        .bind(node).fetch_one(&mut *tx).await?;
    if !eligible {
        return Ok(None);
    }
    let due: Option<bool> = sqlx::query_scalar("INSERT INTO cluster_leases(name,node_id,expires_at) VALUES('scaling',$1,clock_timestamp()+INTERVAL '90 seconds') ON CONFLICT(name) DO UPDATE SET node_id=EXCLUDED.node_id,expires_at=EXCLUDED.expires_at WHERE cluster_leases.node_id=$1 OR cluster_leases.expires_at<=clock_timestamp() RETURNING evaluated_at IS NULL OR evaluated_at<=clock_timestamp()-60::bigint*INTERVAL '1 second'")
        .bind(node).fetch_optional(&mut *tx).await?;
    if due != Some(true) {
        tx.commit().await?;
        return Ok(None);
    }
    sqlx::query("UPDATE cluster_leases SET evaluated_at=clock_timestamp() WHERE name='scaling' AND node_id=$1").bind(node).execute(&mut *tx).await?;
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT SUBSTR(configuration::text,1,4097) FROM cluster_scaling_policies ORDER BY role LIMIT 4 FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    let policies = crate::scaling::decode(rows)?;
    for policy in policies {
        if policy.paused {
            continue;
        }
        let cool: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cluster_scaling_policies WHERE role=$1 AND (last_action_at IS NULL OR last_action_at<=clock_timestamp()-$2::bigint*INTERVAL '1 second'))")
            .bind(&policy.role).bind(policy.cooldown_seconds).fetch_one(&mut *tx).await?;
        if !cool {
            continue;
        }
        let usable: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE status='active' AND state='active' AND last_seen BETWEEN clock_timestamp()-INTERVAL '60 seconds' AND clock_timestamp() AND $1=ANY(roles)")
            .bind(&policy.role).fetch_one(&mut *tx).await?;
        let (cost, reserved) = counts(&mut tx, &policy.role).await?;
        let trigger = match policy.role.as_str() {
            "storage" => {
                let percent: f64 = sqlx::query_scalar("SELECT COALESCE(100.0*SUM(CAST(disk_free AS DOUBLE PRECISION))/NULLIF(SUM(CAST(disk_total AS DOUBLE PRECISION)),0),100.0) FROM nodes WHERE status='active' AND state='active' AND disk_free IS NOT NULL AND disk_total IS NOT NULL AND last_seen BETWEEN clock_timestamp()-INTERVAL '60 seconds' AND clock_timestamp() AND $1=ANY(roles)")
                    .bind(&policy.role).fetch_one(&mut *tx).await?;
                percent < policy.trigger_threshold as f64
            }
            "runner" => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_runs WHERE status='queued' AND created_at<clock_timestamp()-$1::bigint*INTERVAL '1 second')")
                .bind(policy.trigger_threshold).fetch_one(&mut *tx).await?,
            _ => false,
        };
        let sustained: bool = sqlx::query_scalar("UPDATE cluster_scaling_policies SET trigger_since=CASE WHEN $2 THEN COALESCE(trigger_since,clock_timestamp()) ELSE NULL END WHERE role=$1 RETURNING trigger_since IS NOT NULL AND trigger_since<=clock_timestamp()-$3::bigint*INTERVAL '1 second'")
            .bind(&policy.role).bind(trigger).bind(policy.trigger_seconds).fetch_one(&mut *tx).await?;
        let usable = i32::try_from(usable)?;
        let count = crate::scaling::add_count(
            &policy,
            usable,
            cost.saturating_sub(usable).saturating_add(reserved),
            sustained,
        );
        let action = if count > 0 {
            Some(Action {
                role: policy.role.clone(),
                count,
                drain_node: None,
                dry_run: policy.dry_run,
            })
        } else if policy.role == "runner" && usable > policy.minimum && !trigger {
            let idle: Option<String> = sqlx::query_scalar("SELECT n.name FROM nodes n WHERE n.status='active' AND n.state='active' AND n.roles=ARRAY['runner']::text[] AND NOT EXISTS(SELECT 1 FROM ci_runs c WHERE c.worker LIKE n.public_key||'/%' AND c.status='running') AND COALESCE((SELECT MAX(COALESCE(c.completed_at,c.started_at)) FROM ci_runs c WHERE c.worker LIKE n.public_key||'/%'),n.created_at)<=clock_timestamp()-$1::bigint*INTERVAL '1 second' ORDER BY n.id LIMIT 1")
                .bind(policy.idle_seconds).fetch_optional(&mut *tx).await?;
            idle.map(|name| Action {
                role: policy.role.clone(),
                count: 1,
                drain_node: Some(name),
                dry_run: policy.dry_run,
            })
        } else {
            None
        };
        if let Some(action) = action {
            sqlx::query("UPDATE cluster_scaling_policies SET last_action_at=clock_timestamp() WHERE role=$1").bind(&policy.role).execute(&mut *tx).await?;
            let metadata = serde_json::json!({"role":action.role,"count":action.count,"node":action.drain_node,"dry_run":action.dry_run});
            sqlx::query("INSERT INTO audit_log(action,target_type,metadata) VALUES($1,'site',$2)")
                .bind(if action.drain_node.is_some() {
                    "scaling.drain"
                } else {
                    "scaling.add"
                })
                .bind(metadata)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Some(action));
        }
    }
    tx.commit().await?;
    Ok(None)
}

pub(crate) async fn permit_drain(conn: &mut PgConnection, leader: i64, name: &str) -> Result<()> {
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cluster_leases l JOIN nodes owner ON owner.id=l.node_id JOIN nodes n ON n.name=$2 WHERE l.name='scaling' AND l.node_id=$1 AND l.expires_at>clock_timestamp() AND owner.status='active' AND owner.state='active' AND n.status='active' AND n.state='active' AND n.roles=ARRAY['runner']::text[])")
        .bind(leader).bind(name).fetch_one(&mut *conn).await?;
    ensure!(live, "scaling lease expired or runner changed");
    let raw: String = sqlx::query_scalar("SELECT SUBSTR(configuration::text,1,4097) FROM cluster_scaling_policies WHERE role='runner' FOR UPDATE")
        .fetch_one(&mut *conn).await?;
    let policy: ScalingPolicy = serde_json::from_str(&raw)?;
    policy.validate().map_err(anyhow::Error::msg)?;
    let usable: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE status='active' AND state='active' AND roles=ARRAY['runner']::text[]").fetch_one(&mut *conn).await?;
    let running: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_runs c JOIN nodes n ON c.worker LIKE n.public_key||'/%' WHERE n.name=$1 AND c.status='running')")
        .bind(name).fetch_one(&mut *conn).await?;
    ensure!(
        !policy.paused && !policy.dry_run && usable > i64::from(policy.minimum) && !running,
        "runner scaling policy changed"
    );
    Ok(())
}
