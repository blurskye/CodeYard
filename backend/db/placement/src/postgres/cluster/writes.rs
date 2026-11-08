use crate::cluster::writes::MAX_FAMILY;
use crate::cluster::writes::MAX_REFS;
use crate::cluster::writes::MAX_VOTERS;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use codeyard_database::models::RecoveryView;
use codeyard_database::models::RefChange;
use codeyard_database::models::RefValue;
use codeyard_database::models::WriteRecord;
use codeyard_database::models::WriteSettings;
use codeyard_database::models::WriteVote;
use codeyard_database::models::WriteVoter;
use codeyard_database::pool::PgPool;
use sqlx::FromRow;
use sqlx::PgConnection;

#[derive(FromRow)]
struct Control {
    primary_id: i64,
    term: i64,
    committed_version: i64,
    serving: bool,
}

const CONTROL: &str =
    "SELECT r.storage_node_id AS primary_id,c.term,c.committed_version,c.serving=1 AS serving
    FROM repositories r JOIN repository_write_control c ON c.repository_id=r.id WHERE r.id=$1";

async fn lock_control(conn: &mut PgConnection, repo: i64) -> Result<Control> {
    // Placement first, then control, then nodes: the same order for every mutator.
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
        .bind(repo)
        .fetch_one(&mut *conn)
        .await?;
    Ok(sqlx::query_as(&format!("{CONTROL} FOR UPDATE OF c"))
        .bind(repo)
        .fetch_one(&mut *conn)
        .await?)
}

/// Whether the repository or its owner is frozen or has left for another cell. The share
/// lock makes a freeze wait for a decision already in progress.
async fn moving(conn: &mut PgConnection, repo: i64) -> Result<bool> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT m.state,m.expires_at>clock_timestamp() FROM account_moves m JOIN repositories r ON r.id=$1
        WHERE (m.account_kind='repository' AND m.account_id=r.id)
           OR (m.account_kind=CASE WHEN r.owner_kind='org' THEN 'organization' ELSE 'user' END
               AND m.account_id=CASE WHEN r.owner_kind='org' THEN r.owner_org_id ELSE r.owner_user_id END)
        ORDER BY m.account_kind FOR KEY SHARE OF m",
    )
    .bind(repo)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .iter()
        .any(|(state, unexpired)| crate::cluster::writes::move_blocks_writes(state, *unexpired)))
}

async fn active_node(conn: &mut PgConnection, node: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT id FROM nodes WHERE id=$1 AND status='active' AND 'storage'=ANY(roles) FOR SHARE",
    )
    .bind(node)
    .fetch_optional(conn)
    .await?
    .is_some())
}

async fn settings(conn: &mut PgConnection) -> Result<WriteSettings> {
    Ok(sqlx::query_as(
        "SELECT write_quorum,write_timeout_seconds FROM node_configuration WHERE id=1",
    )
    .fetch_optional(conn)
    .await?
    .unwrap_or_default())
}

pub(crate) async fn effective_settings(connection: &mut PgConnection) -> Result<WriteSettings> {
    Ok(sqlx::query_as(crate::cluster::writes::EFFECTIVE_SETTINGS)
        .fetch_optional(connection)
        .await?
        .unwrap_or_default())
}

pub async fn write_settings(pool: &PgPool) -> Result<WriteSettings> {
    effective_settings(&mut *pool.acquire().await?).await
}

async fn quorum(conn: &mut PgConnection, repo: i64, term: i64) -> Result<i64> {
    let settings = settings(conn).await?;
    let setting = settings.write_quorum.context("write quorum is disabled")?;
    crate::cluster::writes::required_votes(&setting, voter_count(conn, repo, term).await?)
}

async fn voter_count(conn: &mut PgConnection, repo: i64, term: i64) -> Result<usize> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM repository_voters WHERE repository_id=$1 AND term=$2",
    )
    .bind(repo)
    .bind(term)
    .fetch_one(conn)
    .await?;
    Ok(usize::try_from(count)?)
}

async fn canonical_refs(conn: &mut PgConnection, repo: i64) -> Result<Vec<RefValue>> {
    let refs = sqlx::query_as("SELECT name,kind,target FROM repository_committed_refs WHERE repository_id=$1 ORDER BY name LIMIT $2")
        .bind(repo).bind((MAX_REFS + 1) as i64).fetch_all(conn).await?;
    crate::cluster::writes::validate_values(&refs)?;
    Ok(refs)
}

async fn record(conn: &mut PgConnection, id: &str) -> Result<Option<WriteRecord>> {
    let Some(mut write) = sqlx::query_as::<_, WriteRecord>("SELECT id,repository_id,term,primary_id,base_version,required_votes,deadline,decision,committed_version,finished=1 AS finished,manifest_hash FROM repository_writes WHERE id=$1")
        .bind(id).fetch_optional(&mut *conn).await? else { return Ok(None); };
    write.changes = sqlx::query_as("SELECT name,kind,old_target,new_target FROM repository_write_refs WHERE write_id=$1 ORDER BY name LIMIT $2")
        .bind(id).bind((MAX_REFS + 1) as i64).fetch_all(&mut *conn).await?;
    crate::cluster::writes::validate_changes(&write.changes)?;
    write.votes = sqlx::query_as::<_, WriteVote>("SELECT node_id,durable_at FROM repository_write_votes WHERE write_id=$1 ORDER BY node_id LIMIT $2")
        .bind(id).bind((MAX_VOTERS + 1) as i64).fetch_all(conn).await?;
    ensure!(write.votes.len() <= MAX_VOTERS, "too many write votes");
    Ok(Some(write))
}

async fn write_control(conn: &mut PgConnection, id: &str) -> Result<Control> {
    let repo: i64 = sqlx::query_scalar("SELECT repository_id FROM repository_writes WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    lock_control(conn, repo).await
}

async fn published(
    conn: &mut PgConnection,
    repo: i64,
    primary: i64,
    term: i64,
    version: i64,
    required: i64,
) -> Result<bool> {
    let rows: Vec<i64> = sqlx::query_scalar("SELECT v.node_id FROM repository_voters v JOIN nodes n ON n.id=v.node_id WHERE v.repository_id=$1 AND v.term=$2 AND v.applied_version >= $3 AND n.status='active' AND 'storage'=ANY(n.roles) ORDER BY v.node_id LIMIT $4 FOR SHARE OF n")
        .bind(repo).bind(term).bind(version).bind((MAX_VOTERS + 1) as i64).fetch_all(conn).await?;
    ensure!(rows.len() <= MAX_VOTERS, "too many voters");
    Ok(rows.contains(&primary) && rows.len() as i64 >= required)
}

pub async fn bootstrap_writes(
    pool: &PgPool,
    repo: i64,
    primary: i64,
    voters: &[i64],
    refs: &[RefValue],
) -> Result<()> {
    crate::cluster::writes::validate_voters(primary, voters)?;
    crate::cluster::writes::validate_values(refs)?;
    let mut tx = pool.begin().await?;
    let placed: Option<i64> =
        sqlx::query_scalar("SELECT storage_node_id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
    ensure!(placed == Some(primary), "repository primary changed");
    let mut voters = voters.to_vec();
    voters.sort_unstable();
    for node in &voters {
        ensure!(
            active_node(&mut tx, *node).await?,
            "voter is not active storage"
        );
    }
    sqlx::query("INSERT INTO repository_write_control(repository_id) VALUES($1)")
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    for node in voters {
        sqlx::query("INSERT INTO repository_voters(repository_id,node_id,term,applied_version) VALUES($1,$2,1,$3)")
            .bind(repo).bind(node).bind(if node == primary { 0_i64 } else { -1_i64 }).execute(&mut *tx).await?;
    }
    quorum(&mut tx, repo, 1).await?;
    for value in refs {
        sqlx::query("INSERT INTO repository_committed_refs(repository_id,name,kind,target) VALUES($1,$2,$3,$4)")
            .bind(repo).bind(&value.name).bind(&value.kind).bind(&value.target).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn enable_initialized_repository(pool: &PgPool, repo: i64, primary: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let control = lock_control(&mut tx, repo).await?;
    if control.primary_id != primary
        || control.term != 1
        || control.committed_version != 0
        || control.serving
    {
        return Ok(false);
    }
    let refs = canonical_refs(&mut tx, repo).await?;
    if refs.len() != 1 || refs[0].name != "HEAD" || refs[0].kind != "symbolic" {
        return Ok(false);
    }
    let required = quorum(&mut tx, repo, 1).await?;
    if !published(&mut tx, repo, primary, 1, 0, required).await? {
        return Ok(false);
    }
    sqlx::query("UPDATE repository_write_control SET serving=1 WHERE repository_id=$1 AND term=1 AND committed_version=0 AND serving=0")
        .bind(repo).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn begin_write(
    pool: &PgPool,
    repo: i64,
    primary: i64,
    term: i64,
    id: &str,
    refs: &[RefChange],
) -> Result<WriteRecord> {
    crate::cluster::writes::validate_id(id)?;
    crate::cluster::writes::validate_changes(refs)?;
    let mut tx = pool.begin().await?;
    ensure!(
        !moving(&mut tx, repo).await?,
        crate::cluster::writes::MOVING
    );
    let control = lock_control(&mut tx, repo).await?;
    ensure!(
        control.primary_id == primary && control.term == term && control.serving,
        "repository term is not serving"
    );
    ensure!(
        active_node(&mut tx, primary).await?,
        "primary is not active storage"
    );
    let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_voters WHERE repository_id=$1 AND node_id=$2 AND term=$3 AND applied_version=$4)")
        .bind(repo).bind(primary).bind(term).bind(control.committed_version).fetch_one(&mut *tx).await?;
    ensure!(current, "primary has not applied committed refs");
    let settings = settings(&mut tx).await?;
    let setting = settings.write_quorum.context("write quorum is disabled")?;
    let required =
        crate::cluster::writes::required_votes(&setting, voter_count(&mut tx, repo, term).await?)?;
    crate::cluster::writes::changed_refs(canonical_refs(&mut tx, repo).await?, refs)?;
    let hash = crate::cluster::writes::manifest_hash(
        repo,
        primary,
        term,
        control.committed_version,
        refs,
    )?;
    sqlx::query("INSERT INTO repository_writes(id,repository_id,term,primary_id,base_version,required_votes,deadline,decision,manifest_hash)
        VALUES($1,$2,$3,$4,$5,$6,clock_timestamp()+($8::bigint * INTERVAL '1 second'),'pending',$7)")
        .bind(id).bind(repo).bind(term).bind(primary).bind(control.committed_version).bind(required).bind(hash).bind(settings.write_timeout_seconds).execute(&mut *tx).await?;
    for change in refs {
        sqlx::query("INSERT INTO repository_write_refs(write_id,name,kind,old_target,new_target) VALUES($1,$2,$3,$4,$5)")
            .bind(id).bind(&change.name).bind(&change.kind).bind(&change.old_target).bind(&change.new_target).execute(&mut *tx).await?;
    }
    let write = record(&mut tx, id).await?.context("write disappeared")?;
    tx.commit().await?;
    Ok(write)
}

pub async fn vote_write(pool: &PgPool, id: &str, node: i64, term: i64) -> Result<bool> {
    crate::cluster::writes::validate_id(id)?;
    let mut tx = pool.begin().await?;
    let control = write_control(&mut tx, id).await?;
    if control.term != term || !control.serving || !active_node(&mut tx, node).await? {
        return Ok(false);
    }
    let added = sqlx::query("INSERT INTO repository_write_votes(write_id,node_id,durable_at)
        SELECT w.id,$2,clock_timestamp() FROM repository_writes w
        JOIN repository_voters v ON v.repository_id=w.repository_id AND v.node_id=$2 AND v.term=w.term
        WHERE w.id=$1 AND w.term=$3 AND w.primary_id=$4 AND w.base_version=$5
          AND v.applied_version=w.base_version AND w.decision='pending' AND w.finished=0 AND w.deadline>clock_timestamp()
        ON CONFLICT(write_id,node_id) DO NOTHING")
        .bind(id).bind(node).bind(term).bind(control.primary_id).bind(control.committed_version).execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(added)
}

pub async fn decide_write(
    pool: &PgPool,
    id: &str,
    primary: i64,
    term: i64,
    commit: bool,
) -> Result<WriteRecord> {
    crate::cluster::writes::validate_id(id)?;
    let mut tx = pool.begin().await?;
    // The move row before the repository row, the order a move's release takes them in.
    let repo: Option<i64> =
        sqlx::query_scalar("SELECT repository_id FROM repository_writes WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let moving = match repo {
        Some(repo) => moving(&mut tx, repo).await?,
        None => false,
    };
    let control = write_control(&mut tx, id).await?;
    let write = record(&mut tx, id).await?.context("write disappeared")?;
    ensure!(
        control.term == term
            && control.primary_id == primary
            && write.term == term
            && write.primary_id == primary,
        "stale write term"
    );
    if write.decision != "pending" {
        return Ok(write);
    }
    let mut can_commit = commit
        && control.serving
        && control.committed_version == write.base_version
        && !moving
        && active_node(&mut tx, primary).await?;
    if can_commit {
        let votes: Vec<i64> = sqlx::query_scalar("SELECT v.node_id FROM repository_write_votes v
            JOIN repository_voters m ON m.repository_id=$2 AND m.node_id=v.node_id AND m.term=$3
            JOIN nodes n ON n.id=v.node_id
            WHERE v.write_id=$1 AND m.applied_version=$4 AND n.status='active' AND 'storage'=ANY(n.roles)
            ORDER BY v.node_id LIMIT $5 FOR SHARE OF n")
            .bind(id).bind(write.repository_id).bind(term).bind(write.base_version).bind((MAX_VOTERS + 1) as i64).fetch_all(&mut *tx).await?;
        ensure!(votes.len() <= MAX_VOTERS, "too many write votes");
        can_commit = votes.len() as i64 >= write.required_votes;
    }
    let mut version = None;
    if can_commit {
        // The deadline is checked in the deciding UPDATE, not before waiting for a lock.
        let next = write
            .base_version
            .checked_add(1)
            .context("committed version exhausted")?;
        let changed = sqlx::query("UPDATE repository_write_control SET committed_version=$4
            WHERE repository_id=$1 AND term=$2 AND committed_version=$3 AND serving=1
              AND EXISTS(SELECT 1 FROM repository_writes WHERE id=$5 AND decision='pending' AND deadline>clock_timestamp())")
            .bind(write.repository_id).bind(term).bind(write.base_version).bind(next).bind(id).execute(&mut *tx).await?.rows_affected() == 1;
        if changed {
            crate::cluster::writes::changed_refs(
                canonical_refs(&mut tx, write.repository_id).await?,
                &write.changes,
            )?;
            for change in &write.changes {
                if let Some(target) = &change.new_target {
                    sqlx::query("INSERT INTO repository_committed_refs(repository_id,name,kind,target) VALUES($1,$2,$3,$4)
                        ON CONFLICT(repository_id,name) DO UPDATE SET kind=excluded.kind,target=excluded.target")
                        .bind(write.repository_id).bind(&change.name).bind(&change.kind).bind(target).execute(&mut *tx).await?;
                } else {
                    sqlx::query(
                        "DELETE FROM repository_committed_refs WHERE repository_id=$1 AND name=$2",
                    )
                    .bind(write.repository_id)
                    .bind(&change.name)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            version = Some(next);
        }
    }
    sqlx::query("UPDATE repository_writes SET decision=$2,committed_version=$3 WHERE id=$1 AND decision='pending'")
        .bind(id).bind(if version.is_some() { "commit" } else { "abort" }).bind(version).execute(&mut *tx).await?;
    let result = record(&mut tx, id).await?.context("write disappeared")?;
    tx.commit().await?;
    Ok(result)
}

pub async fn load_write(pool: &PgPool, id: &str) -> Result<Option<WriteRecord>> {
    crate::cluster::writes::validate_id(id)?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let write = record(&mut tx, id).await?;
    tx.commit().await?;
    Ok(write)
}

pub async fn recovery_view(pool: &PgPool, repo: i64) -> Result<RecoveryView> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let control: Control = sqlx::query_as(CONTROL)
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
    let voters = sqlx::query_as::<_, WriteVoter>("SELECT node_id,term,applied_version FROM repository_voters WHERE repository_id=$1 ORDER BY node_id LIMIT $2")
        .bind(repo).bind((MAX_VOTERS + 1) as i64).fetch_all(&mut *tx).await?;
    ensure!(voters.len() <= MAX_VOTERS, "too many voters");
    let refs = canonical_refs(&mut tx, repo).await?;
    let id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM repository_writes WHERE repository_id=$1 AND finished=0",
    )
    .bind(repo)
    .fetch_optional(&mut *tx)
    .await?;
    let unfinished = match id {
        Some(id) => record(&mut tx, &id).await?,
        None => None,
    };
    tx.commit().await?;
    Ok(RecoveryView {
        repository_id: repo,
        primary_id: control.primary_id,
        term: control.term,
        committed_version: control.committed_version,
        serving: control.serving,
        voters,
        refs,
        unfinished,
    })
}

pub async fn mark_write_applied(
    pool: &PgPool,
    repo: i64,
    node: i64,
    term: i64,
    version: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let control = lock_control(&mut tx, repo).await?;
    if control.term != term
        || version < 0
        || version > control.committed_version
        || !active_node(&mut tx, node).await?
    {
        return Ok(false);
    }
    let changed = sqlx::query("UPDATE repository_voters SET applied_version=$4 WHERE repository_id=$1 AND node_id=$2 AND term=$3 AND applied_version<=$4")
        .bind(repo).bind(node).bind(term).bind(version).execute(&mut *tx).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(changed)
}

pub async fn finish_write(pool: &PgPool, id: &str, current_term: i64) -> Result<bool> {
    crate::cluster::writes::validate_id(id)?;
    let mut tx = pool.begin().await?;
    let control = write_control(&mut tx, id).await?;
    if control.term != current_term {
        return Ok(false);
    }
    let write = record(&mut tx, id).await?.context("write disappeared")?;
    if write.decision == "pending" {
        return Ok(false);
    }
    if write.decision == "commit"
        && !published(
            &mut tx,
            write.repository_id,
            control.primary_id,
            current_term,
            write.committed_version.context("commit has no version")?,
            write.required_votes,
        )
        .await?
    {
        return Ok(false);
    }
    sqlx::query("UPDATE repository_writes SET finished=1 WHERE id=$1 AND decision<>'pending'")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

async fn lock_family(
    conn: &mut PgConnection,
    repo: i64,
    terms: &[(i64, i64)],
) -> Result<Option<Vec<(i64, Control)>>> {
    let family: Vec<(i64, Option<i64>, i64)> = sqlx::query_as(&format!(
        "{} LIMIT {} FOR UPDATE OF r",
        crate::postgres::cluster::FORK_FAMILY,
        MAX_FAMILY + 1
    ))
    .bind(repo)
    .fetch_all(&mut *conn)
    .await?;
    if family.len() != terms.len()
        || family
            .iter()
            .any(|(id, _, _)| !terms.iter().any(|(expected, _)| expected == id))
    {
        return Ok(None);
    }
    let current: Vec<(i64, Option<i64>, i64)> = sqlx::query_as(&format!(
        "{} LIMIT {}",
        crate::postgres::cluster::FORK_FAMILY,
        MAX_FAMILY + 1
    ))
    .bind(repo)
    .fetch_all(&mut *conn)
    .await?;
    if current != family {
        return Ok(None);
    }
    let mut controls = Vec::with_capacity(family.len());
    for (id, _, _) in family {
        let control = lock_control(conn, id).await?;
        if !terms.contains(&(id, control.term)) {
            return Ok(None);
        }
        controls.push((id, control));
    }
    Ok(Some(controls))
}

pub async fn write_family(pool: &PgPool, repo: i64) -> Result<Vec<(i64, Option<i64>, i64)>> {
    let family: Vec<(i64, Option<i64>, i64)> = sqlx::query_as(&format!(
        "{} LIMIT {}",
        crate::postgres::cluster::FORK_FAMILY,
        MAX_FAMILY + 1
    ))
    .bind(repo)
    .fetch_all(pool)
    .await?;
    ensure!(
        !family.is_empty() && family.len() <= MAX_FAMILY,
        "invalid fork family size"
    );
    Ok(family)
}

pub async fn switch_write_family(
    pool: &PgPool,
    repo: i64,
    expected_primary: i64,
    terms: &[(i64, i64)],
    target: i64,
    voters: &[i64],
) -> Result<bool> {
    crate::cluster::writes::validate_terms(terms)?;
    crate::cluster::writes::validate_voters(target, voters)?;
    let mut tx = pool.begin().await?;
    let Some(controls) = lock_family(&mut tx, repo, terms).await? else {
        return Ok(false);
    };
    if controls
        .iter()
        .any(|(_, c)| c.primary_id != expected_primary || c.term == i64::MAX)
    {
        return Ok(false);
    }
    for (member, _) in &controls {
        let moving_elsewhere: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_moves WHERE repository_id=$1 AND target_node_id<>$2)")
            .bind(member).bind(target).fetch_one(&mut *tx).await?;
        if moving_elsewhere {
            return Ok(false);
        }
    }
    let mut voters = voters.to_vec();
    voters.sort_unstable();
    for node in &voters {
        let allocating: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND state='active')")
                .bind(node)
                .fetch_one(&mut *tx)
                .await?;
        if !allocating {
            return Ok(false);
        }
        if !active_node(&mut tx, *node).await? {
            return Ok(false);
        }
    }
    let setting = settings(&mut tx)
        .await?
        .write_quorum
        .context("write quorum is disabled")?;
    crate::cluster::writes::required_votes(&setting, voters.len())?;
    for (repo, control) in &controls {
        // Replacing failed voters must not silently lower the denominator.
        if voters.len() < voter_count(&mut tx, *repo, control.term).await? {
            return Ok(false);
        }
    }
    for (repo, control) in &controls {
        for node in &voters {
            let retained: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_voters WHERE repository_id=$1 AND node_id=$2 AND term=$3)")
                .bind(repo).bind(node).bind(control.term).fetch_one(&mut *tx).await?;
            if !retained {
                let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repository_replicas WHERE repository_id=$1 AND node_id=$2 AND applied_version=$3)")
                    .bind(repo).bind(node).bind(control.committed_version).fetch_one(&mut *tx).await?;
                if !current {
                    return Ok(false);
                }
            }
        }
    }
    for (repo, control) in controls {
        sqlx::query("UPDATE repository_write_control SET term=term+1,serving=0 WHERE repository_id=$1 AND term=$2 AND committed_version=$3")
            .bind(repo).bind(control.term).bind(control.committed_version).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE repositories SET storage_node_id=$2 WHERE id=$1 AND storage_node_id=$3",
        )
        .bind(repo)
        .bind(target)
        .bind(expected_primary)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE repository_writes SET decision='abort' WHERE repository_id=$1 AND decision='pending'")
            .bind(repo).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM repository_voters WHERE repository_id=$1")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
        for node in &voters {
            // New term directories must be reconstructed and explicitly marked applied.
            sqlx::query(
                "INSERT INTO repository_voters(repository_id,node_id,term) VALUES($1,$2,$3)",
            )
            .bind(repo)
            .bind(node)
            .bind(control.term + 1)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn enable_write_family(pool: &PgPool, terms: &[(i64, i64)]) -> Result<bool> {
    crate::cluster::writes::validate_terms(terms)?;
    let mut tx = pool.begin().await?;
    let Some(controls) = lock_family(&mut tx, terms[0].0, terms).await? else {
        return Ok(false);
    };
    if controls
        .iter()
        .any(|(_, c)| c.primary_id != controls[0].1.primary_id)
    {
        return Ok(false);
    }
    for (repo, control) in &controls {
        let unfinished: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM repository_writes WHERE repository_id=$1 AND finished=0)",
        )
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
        let required = quorum(&mut tx, *repo, control.term).await?;
        if unfinished
            || !published(
                &mut tx,
                *repo,
                control.primary_id,
                control.term,
                control.committed_version,
                required,
            )
            .await?
        {
            return Ok(false);
        }
    }
    for (repo, control) in controls {
        sqlx::query("UPDATE repository_write_control SET serving=1 WHERE repository_id=$1 AND term=$2 AND committed_version=$3")
            .bind(repo).bind(control.term).bind(control.committed_version).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Canonical voting copies participate in takeover without an asynchronous copy row.
pub async fn held_voters(
    pool: &PgPool,
    node: i64,
) -> Result<Vec<codeyard_database::models::HeldReplica>> {
    let limit = crate::drain::repository_limit()?;
    let rows: Vec<codeyard_database::models::HeldReplica> = sqlx::query_as(
        "SELECT v.repository_id,r.disk_path,r.storage_node_id AS primary_node_id,
            v.applied_version,c.committed_version AS write_version,c.serving=0 AS fenced
         FROM repository_voters v JOIN repositories r ON r.id=v.repository_id
         JOIN repository_write_control c ON c.repository_id=r.id
         WHERE v.node_id=$1 AND v.term=c.term ORDER BY v.repository_id LIMIT $2",
    )
    .bind(node)
    .bind(limit + 1)
    .fetch_all(pool)
    .await?;
    ensure!(rows.len() as i64 <= limit, "voter holding limit exceeded");
    Ok(rows)
}

pub async fn write_family_terms(pool: &PgPool, repo: i64) -> Result<Vec<(i64, i64)>> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(&format!(
        "WITH family AS ({}) SELECT r.id,c.term FROM family f JOIN repositories r ON r.id=f.id
         JOIN repository_write_control c ON c.repository_id=r.id ORDER BY r.id LIMIT $2",
        crate::postgres::cluster::FORK_FAMILY,
    ))
    .bind(repo)
    .bind((MAX_FAMILY + 1) as i64)
    .fetch_all(pool)
    .await?;
    crate::cluster::writes::validate_terms(&rows)?;
    Ok(rows)
}
