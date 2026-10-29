use crate::backend::db::{
    models::Repository,
    relationships::{Kind, Link, Mutation, Refusal, RelationCounts},
};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};
pub async fn list(
    pool: &PgPool,
    viewer: Option<i64>,
    root: i64,
    kind: Kind,
    offset: i64,
    limit: i64,
) -> Result<Vec<Link>> {
    anyhow::ensure!(offset >= 0 && (1..=101).contains(&limit), Refusal::Invalid);
    let (target, source) = kind.selection();
    Ok(sqlx::query_as(&format!("SELECT i.id,i.repo_id,i.number,i.title,i.state,COALESCE(owners.username,orgs.name) AS owner,r.name AS repo FROM issue_relations e JOIN issues i ON i.id={target} JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id=$1 LEFT JOIN users owners ON r.owner_kind='user' AND owners.id=r.owner_user_id LEFT JOIN organizations orgs ON r.owner_kind='org' AND orgs.id=r.owner_org_id WHERE {source}=$2 AND e.kind=$3 AND {} AND (COALESCE(u.username,'')<>$6 OR i.repo_id=(SELECT repo_id FROM issues WHERE id=$2)) ORDER BY e.position,i.id LIMIT $4 OFFSET $5",super::repositories::access::readable_condition())).bind(viewer).bind(root).bind(kind.storage()).bind(limit).bind(offset).bind(crate::backend::db::ci_tokens::BOT).fetch_all(pool).await?)
}
pub async fn summaries(pool: &PgPool, roots: &[i64]) -> Result<Vec<RelationCounts>> {
    anyhow::ensure!(roots.len() <= 100, Refusal::Limit);
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "WITH edges(root, side, repo_id, state) AS (
    SELECT e.source_id, 'child', i.repo_id, i.state FROM issue_relations e JOIN issues i ON i.id=e.target_id
        WHERE e.kind='parent' AND e.source_id IN (SELECT unnest($1::BIGINT[]))
    UNION ALL SELECT e.target_id, 'blocked_by', i.repo_id, i.state FROM issue_relations e JOIN issues i ON i.id=e.source_id
        WHERE e.kind='blocks' AND e.target_id IN (SELECT unnest($1::BIGINT[]))
    UNION ALL SELECT e.source_id, 'blocking', i.repo_id, i.state FROM issue_relations e JOIN issues i ON i.id=e.target_id
        WHERE e.kind='blocks' AND e.source_id IN (SELECT unnest($1::BIGINT[])))
SELECT root, repo_id,
    SUM(CASE WHEN side='child' THEN 1 ELSE 0 END) AS total,
    SUM(CASE WHEN side='child' AND state='closed' THEN 1 ELSE 0 END) AS completed,
    SUM(CASE WHEN side='blocked_by' AND state='open' THEN 1 ELSE 0 END) AS blocked_by,
    SUM(CASE WHEN side='blocking' AND state='open' THEN 1 ELSE 0 END) AS blocking,
    SUM(CASE WHEN side='blocked_by' THEN 1 ELSE 0 END) AS total_blocked_by,
    SUM(CASE WHEN side='blocking' THEN 1 ELSE 0 END) AS total_blocking
FROM edges GROUP BY root, repo_id",
    )
    .bind(roots)
    .fetch_all(pool)
    .await?)
}
pub async fn change(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    root: i64,
    mutation: Mutation,
) -> Result<()> {
    super::retry_on_deadlock(|| change_once(pool, actor, repo, root, mutation)).await
}
async fn change_once(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    root: i64,
    mutation: Mutation,
) -> Result<()> {
    let tx = pool.begin().await?;
    let tx = change_in_transaction(tx, actor, repo, root, mutation, false).await?;
    tx.commit().await?;
    Ok(())
}
async fn authority(
    tx: &mut Transaction<'_, Postgres>,
    actor: i64,
    repo: i64,
    triage: bool,
) -> Result<bool> {
    if triage {
        super::discussions::lock_triage_authority(tx, actor, repo).await
    } else {
        super::discussions::lock_write_authority(tx, actor, repo).await
    }
}
pub(crate) async fn change_in_transaction<'a>(
    mut tx: Transaction<'a, Postgres>,
    actor: i64,
    repo: i64,
    root: i64,
    mutation: Mutation,
    triage: bool,
) -> Result<Transaction<'a, Postgres>> {
    let other = mutation.target();
    anyhow::ensure!(other > 0 && root != other, Refusal::Invalid);
    anyhow::ensure!(
        authority(&mut tx, actor, repo, triage).await?,
        Refusal::Denied
    );
    let restricted: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND username=$2)")
            .bind(actor)
            .bind(crate::backend::db::ci_tokens::BOT)
            .fetch_one(&mut *tx)
            .await?;
    // Membership authority precedes every row lock. This guard serializes even disjoint edge
    // insertions that could otherwise jointly close a cycle under READ COMMITTED.
    sqlx::query_scalar::<_, i64>("SELECT id FROM issue_relation_guard WHERE id=1 FOR UPDATE")
        .fetch_one(&mut *tx)
        .await?;
    let target_repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM issues WHERE id=$1")
        .bind(other)
        .fetch_optional(&mut *tx)
        .await?;
    let target_repo = target_repo.ok_or(Refusal::Missing)?;
    anyhow::ensure!(!restricted || target_repo == repo, Refusal::Missing);
    let access = super::repositories::access::lock_user_repo_access_in_transaction(
        &mut tx,
        actor,
        target_repo,
        super::repositories::access::RepositoryRowLock::Shared,
        true,
    )
    .await?;
    anyhow::ensure!(
        access.is_some_and(|a| a.access.can_read() || (!a.organization && a.is_admin)),
        Refusal::Missing
    );
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM issues WHERE (id=$1 AND repo_id=$3) OR id=$2 ORDER BY id FOR UPDATE",
    )
    .bind(root)
    .bind(other)
    .bind(repo)
    .fetch_all(&mut *tx)
    .await?;
    anyhow::ensure!(ids.len() == 2, Refusal::Missing);
    if matches!(
        mutation,
        Mutation::Move { .. }
            | Mutation::Add {
                kind: Kind::Children | Kind::Parent,
                ..
            }
            | Mutation::Remove {
                kind: Kind::Children | Kind::Parent,
                ..
            }
    ) {
        anyhow::ensure!(
            authority(&mut tx, actor, target_repo, triage).await?,
            Refusal::Denied
        );
    }
    let mut changed = true;
    match mutation {
        Mutation::Add { kind, replace, .. } => {
            let (source, target) = kind.edge(root, other);
            let storage = kind.storage();
            let existing:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_relations WHERE kind=$1 AND source_id=$2 AND target_id=$3)").bind(storage).bind(source).bind(target).fetch_one(&mut *tx).await?;
            if existing {
                changed = false;
            } else {
                if storage == "parent" {
                    anyhow::ensure!(
                        authority(&mut tx, actor, target_repo, triage).await?,
                        Refusal::Denied
                    );
                    let a: Repository = sqlx::query_as("SELECT * FROM repositories WHERE id=$1")
                        .bind(repo)
                        .fetch_one(&mut *tx)
                        .await?;
                    let b: Repository = sqlx::query_as("SELECT * FROM repositories WHERE id=$1")
                        .bind(target_repo)
                        .fetch_one(&mut *tx)
                        .await?;
                    anyhow::ensure!(
                        a.owner_kind == b.owner_kind
                            && a.owner_user_id == b.owner_user_id
                            && a.owner_org_id == b.owner_org_id,
                        Refusal::Invalid
                    );
                    let old:Option<(i64,i64)>=sqlx::query_as("SELECT e.source_id,i.repo_id FROM issue_relations e JOIN issues i ON i.id=e.source_id WHERE e.kind='parent' AND e.target_id=$1").bind(target).fetch_optional(&mut *tx).await?;
                    if let Some((_, old_repo)) = old {
                        anyhow::ensure!(!restricted || old_repo == repo, Refusal::Missing);
                        anyhow::ensure!(replace, Refusal::HasParent);
                        anyhow::ensure!(
                            authority(&mut tx, actor, old_repo, triage).await?,
                            Refusal::Denied
                        );
                    }
                }
                if storage != "related" {
                    let cycle:bool=sqlx::query_scalar("WITH RECURSIVE reach(id) AS (SELECT $1::bigint UNION SELECT e.target_id FROM issue_relations e JOIN reach r ON r.id=e.source_id WHERE e.kind=$3) SELECT EXISTS(SELECT 1 FROM reach WHERE id=$2)").bind(target).bind(source).bind(storage).fetch_one(&mut *tx).await?;
                    anyhow::ensure!(!cycle, Refusal::Cycle);
                }
                if storage == "parent" {
                    let depth:i64=sqlx::query_scalar("WITH RECURSIVE ancestors(id,depth) AS (SELECT $1::bigint,1 UNION ALL SELECT e.source_id,a.depth+1 FROM issue_relations e JOIN ancestors a ON a.id=e.target_id WHERE e.kind='parent' AND a.depth<9), descendants(id,depth) AS (SELECT $2::bigint,1 UNION ALL SELECT e.target_id,d.depth+1 FROM issue_relations e JOIN descendants d ON d.id=e.source_id WHERE e.kind='parent' AND d.depth<9) SELECT CAST((SELECT MAX(depth) FROM ancestors)+(SELECT MAX(depth) FROM descendants) AS BIGINT)").bind(source).bind(target).fetch_one(&mut *tx).await?;
                    anyhow::ensure!(depth <= 8, Refusal::Limit);
                    sqlx::query("DELETE FROM issue_relations WHERE kind='parent' AND target_id=$1")
                        .bind(target)
                        .execute(&mut *tx)
                        .await?;
                }
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM issue_relations WHERE kind=$1 AND source_id=$2",
                )
                .bind(storage)
                .bind(source)
                .fetch_one(&mut *tx)
                .await?;
                anyhow::ensure!(count < 100, Refusal::Limit);
                sqlx::query("INSERT INTO issue_relations(kind,source_id,target_id,position) VALUES ($1,$2,$3,COALESCE((SELECT MAX(position)+1 FROM issue_relations WHERE kind=$1 AND source_id=$2),0))").bind(storage).bind(source).bind(target).execute(&mut *tx).await?;
            }
        }
        Mutation::Remove { kind, .. } => {
            let (source, target) = kind.edge(root, other);
            let deleted = sqlx::query(
                "DELETE FROM issue_relations WHERE kind=$1 AND source_id=$2 AND target_id=$3",
            )
            .bind(kind.storage())
            .bind(source)
            .bind(target)
            .execute(&mut *tx)
            .await?;
            anyhow::ensure!(deleted.rows_affected() == 1, Refusal::Missing);
        }
        Mutation::Move {
            child,
            before,
            after,
        } => {
            anyhow::ensure!(
                before.is_some() != after.is_some()
                    && before != Some(child)
                    && after != Some(child),
                Refusal::Invalid
            );
            let mut children:Vec<i64>=sqlx::query_scalar("SELECT target_id FROM issue_relations WHERE kind='parent' AND source_id=$1 ORDER BY position,target_id").bind(root).fetch_all(&mut *tx).await?;
            anyhow::ensure!(children.contains(&child), Refusal::Missing);
            children.retain(|id| *id != child);
            let anchor = before.or(after).ok_or(Refusal::Invalid)?;
            let at = children
                .iter()
                .position(|id| *id == anchor)
                .ok_or(Refusal::Invalid)?
                + usize::from(after.is_some());
            children.insert(at, child);
            for (position, id) in children.iter().enumerate() {
                sqlx::query("UPDATE issue_relations SET position=$3 WHERE kind='parent' AND source_id=$1 AND target_id=$2").bind(root).bind(id).bind(position as i64).execute(&mut *tx).await?;
            }
        }
    }
    if changed {
        touch(&mut tx, root, other).await?;
    }
    Ok(tx)
}
async fn touch(tx: &mut Transaction<'_, Postgres>, a: i64, b: i64) -> Result<()> {
    sqlx::query("UPDATE issues SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1 OR id=$2").bind(a).bind(b).execute(&mut **tx).await?;
    Ok(())
}
