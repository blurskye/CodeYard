use crate::backend::db::relationships::related_sql;
use crate::backend::db::relationships::window_sql;
use crate::backend::db::relationships::Kind;
use crate::backend::db::relationships::Link;
use crate::backend::db::relationships::Mutation;
use crate::backend::db::relationships::Refusal;
use crate::backend::db::relationships::RelationCounts;
use crate::backend::db::relationships::Window;
use anyhow::Result;
use codeyard_db_core::models::Repository;
use sqlx::{Sqlite, SqlitePool, Transaction};
pub async fn window(pool: &SqlitePool, page: &Window<'_>) -> Result<Vec<Link>> {
    anyhow::ensure!(
        page.offset >= 0 && (1..=101).contains(&page.limit),
        Refusal::Invalid
    );
    let readable = super::repositories::access::readable_condition();
    let sql = window_sql(
        page,
        &readable,
        "$5 IS NULL OR i.repo_id IN (SELECT value FROM json_each($5))",
    )
    .replace('$', "?");
    let repositories = page.repositories.map(serde_json::to_string).transpose()?;
    Ok(sqlx::query_as(&sql)
        .bind(page.viewer)
        .bind(page.root)
        .bind(page.kind.storage())
        .bind(crate::backend::db::ci_tokens::BOT)
        .bind(repositories)
        .bind(page.limit)
        .bind(page.offset)
        .fetch_all(pool)
        .await?)
}
pub async fn repository_counts(
    pool: &SqlitePool,
    viewer: Option<i64>,
    root: i64,
    kind: Kind,
) -> Result<Vec<(i64, i64)>> {
    let readable = super::repositories::access::readable_condition();
    let sql = format!(
        "SELECT i.repo_id,COUNT(*) {} GROUP BY i.repo_id ORDER BY i.repo_id",
        related_sql(kind, &readable, "1")
    )
    .replace('$', "?");
    Ok(sqlx::query_as(&sql)
        .bind(viewer)
        .bind(root)
        .bind(kind.storage())
        .bind(crate::backend::db::ci_tokens::BOT)
        .fetch_all(pool)
        .await?)
}
pub async fn summaries(pool: &SqlitePool, roots: &[i64]) -> Result<Vec<RelationCounts>> {
    anyhow::ensure!(roots.len() <= 100, Refusal::Limit);
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "WITH edges(root, side, repo_id, state) AS (
    SELECT e.source_id, 'child', i.repo_id, i.state FROM issue_relations e JOIN issues i ON i.id=e.target_id
        WHERE e.kind='parent' AND e.source_id IN (SELECT value FROM json_each(?1))
    UNION ALL SELECT e.target_id, 'blocked_by', i.repo_id, i.state FROM issue_relations e JOIN issues i ON i.id=e.source_id
        WHERE e.kind='blocks' AND e.target_id IN (SELECT value FROM json_each(?1))
    UNION ALL SELECT e.source_id, 'blocking', i.repo_id, i.state FROM issue_relations e JOIN issues i ON i.id=e.target_id
        WHERE e.kind='blocks' AND e.source_id IN (SELECT value FROM json_each(?1)))
SELECT root, repo_id,
    SUM(CASE WHEN side='child' THEN 1 ELSE 0 END) AS total,
    SUM(CASE WHEN side='child' AND state='closed' THEN 1 ELSE 0 END) AS completed,
    SUM(CASE WHEN side='blocked_by' AND state='open' THEN 1 ELSE 0 END) AS blocked_by,
    SUM(CASE WHEN side='blocking' AND state='open' THEN 1 ELSE 0 END) AS blocking,
    SUM(CASE WHEN side='blocked_by' THEN 1 ELSE 0 END) AS total_blocked_by,
    SUM(CASE WHEN side='blocking' THEN 1 ELSE 0 END) AS total_blocking
FROM edges GROUP BY root, repo_id",
    )
    .bind(serde_json::to_string(roots)?)
    .fetch_all(pool)
    .await?)
}
pub async fn change(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    root: i64,
    mutation: Mutation,
    token: Option<i64>,
) -> Result<()> {
    change_once(pool, actor, repo, root, mutation, token).await
}
async fn change_once(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    root: i64,
    mutation: Mutation,
    token: Option<i64>,
) -> Result<()> {
    let tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let triage = matches!(
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
    );
    let tx = change_in_transaction(tx, actor, repo, root, mutation, triage, token).await?;
    tx.commit().await?;
    Ok(())
}
async fn authority(
    tx: &mut Transaction<'_, Sqlite>,
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
    mut tx: Transaction<'a, Sqlite>,
    actor: i64,
    repo: i64,
    root: i64,
    mutation: Mutation,
    triage: bool,
    token: Option<i64>,
) -> Result<Transaction<'a, Sqlite>> {
    let other = mutation.target();
    anyhow::ensure!(other > 0 && root != other, Refusal::Invalid);
    anyhow::ensure!(
        authority(&mut tx, actor, repo, triage).await?,
        Refusal::Denied
    );
    let restricted: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND username=?2)")
            .bind(actor)
            .bind(crate::backend::db::ci_tokens::BOT)
            .fetch_one(&mut *tx)
            .await?;
    // BEGIN IMMEDIATE serializes graph validation and insertion, including disjoint edges.
    sqlx::query_scalar::<_, i64>("SELECT id FROM issue_relation_guard WHERE id=1")
        .fetch_one(&mut *tx)
        .await?;
    let target_repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM issues WHERE id=?1")
        .bind(other)
        .fetch_optional(&mut *tx)
        .await?;
    let target_repo = target_repo.ok_or(Refusal::Missing)?;
    anyhow::ensure!(!restricted || target_repo == repo, Refusal::Missing);
    if let Some(token) = token {
        for affected in [repo, target_repo] {
            anyhow::ensure!(
                super::users::tokens::repository_write_on(
                    &mut tx, token, actor, affected, "issues"
                )
                .await?,
                Refusal::Denied
            );
        }
    }
    let visible:bool=sqlx::query_scalar(&format!("SELECT EXISTS (SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=?1 WHERE r.id=?2 AND {})",super::repositories::access::readable_condition())).bind(actor).bind(target_repo).fetch_one(&mut *tx).await?;
    anyhow::ensure!(visible, Refusal::Missing);
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM issues WHERE (id=?1 AND repo_id=?3) OR (id=?2 AND repo_id=?4) ORDER BY id",
    )
    .bind(root)
    .bind(other)
    .bind(repo)
    .bind(target_repo)
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
    let mut former_parent = None;
    match mutation {
        Mutation::Add { kind, replace, .. } => {
            let (source, target) = kind.edge(root, other);
            let storage = kind.storage();
            let existing:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_relations WHERE kind=?1 AND source_id=?2 AND target_id=?3)").bind(storage).bind(source).bind(target).fetch_one(&mut *tx).await?;
            if existing {
                changed = false;
            } else {
                if storage == "parent" {
                    anyhow::ensure!(
                        authority(&mut tx, actor, target_repo, triage).await?,
                        Refusal::Denied
                    );
                    let a: Repository = sqlx::query_as("SELECT * FROM repositories WHERE id=?1")
                        .bind(repo)
                        .fetch_one(&mut *tx)
                        .await?;
                    let b: Repository = sqlx::query_as("SELECT * FROM repositories WHERE id=?1")
                        .bind(target_repo)
                        .fetch_one(&mut *tx)
                        .await?;
                    anyhow::ensure!(
                        a.owner_kind == b.owner_kind
                            && a.owner_user_id == b.owner_user_id
                            && a.owner_org_id == b.owner_org_id,
                        Refusal::Invalid
                    );
                    let old:Option<(i64,i64)>=sqlx::query_as("SELECT e.source_id,i.repo_id FROM issue_relations e JOIN issues i ON i.id=e.source_id WHERE e.kind='parent' AND e.target_id=?1").bind(target).fetch_optional(&mut *tx).await?;
                    if let Some((old_parent, old_repo)) = old {
                        anyhow::ensure!(!restricted || old_repo == repo, Refusal::Missing);
                        anyhow::ensure!(replace, Refusal::HasParent);
                        if let Some(token) = token {
                            anyhow::ensure!(
                                super::users::tokens::repository_write_on(
                                    &mut tx, token, actor, old_repo, "issues"
                                )
                                .await?,
                                Refusal::Denied
                            );
                        }
                        anyhow::ensure!(
                            authority(&mut tx, actor, old_repo, triage).await?,
                            Refusal::Denied
                        );
                        former_parent = Some(old_parent);
                    }
                }
                if storage != "related" {
                    let cycle:bool=sqlx::query_scalar("WITH RECURSIVE reach(id) AS (SELECT ?1 UNION SELECT e.target_id FROM issue_relations e JOIN reach r ON r.id=e.source_id WHERE e.kind=?3) SELECT EXISTS(SELECT 1 FROM reach WHERE id=?2)").bind(target).bind(source).bind(storage).fetch_one(&mut *tx).await?;
                    anyhow::ensure!(!cycle, Refusal::Cycle);
                }
                if storage == "parent" {
                    let depth:i64=sqlx::query_scalar("WITH RECURSIVE ancestors(id,depth) AS (SELECT ?1,1 UNION ALL SELECT e.source_id,a.depth+1 FROM issue_relations e JOIN ancestors a ON a.id=e.target_id WHERE e.kind='parent' AND a.depth<9), descendants(id,depth) AS (SELECT ?2,1 UNION ALL SELECT e.target_id,d.depth+1 FROM issue_relations e JOIN descendants d ON d.id=e.source_id WHERE e.kind='parent' AND d.depth<9) SELECT CAST((SELECT MAX(depth) FROM ancestors)+(SELECT MAX(depth) FROM descendants) AS BIGINT)").bind(source).bind(target).fetch_one(&mut *tx).await?;
                    anyhow::ensure!(depth <= 8, Refusal::Limit);
                    sqlx::query("DELETE FROM issue_relations WHERE kind='parent' AND target_id=?1")
                        .bind(target)
                        .execute(&mut *tx)
                        .await?;
                }
                let count: i64 = sqlx::query_scalar(
                    "SELECT MAX(n) FROM (SELECT COUNT(*) AS n FROM issue_relations WHERE kind=?1 AND (source_id=?2 OR (kind='related' AND target_id=?2)) UNION ALL SELECT COUNT(*) AS n FROM issue_relations WHERE kind=?1 AND kind='related' AND (source_id=?3 OR target_id=?3)) counts",
                )
                .bind(storage)
                .bind(source)
                .bind(target)
                .fetch_one(&mut *tx)
                .await?;
                anyhow::ensure!(count < 100, Refusal::Limit);
                sqlx::query("INSERT INTO issue_relations(kind,source_id,target_id,position) VALUES (?1,?2,?3,COALESCE((SELECT MAX(position)+1 FROM issue_relations WHERE kind=?1 AND source_id=?2),0))").bind(storage).bind(source).bind(target).execute(&mut *tx).await?;
            }
        }
        Mutation::Remove { kind, .. } => {
            let (source, target) = kind.edge(root, other);
            let deleted = sqlx::query(
                "DELETE FROM issue_relations WHERE kind=?1 AND source_id=?2 AND target_id=?3",
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
            let mut children:Vec<i64>=sqlx::query_scalar("SELECT target_id FROM issue_relations WHERE kind='parent' AND source_id=?1 ORDER BY position,target_id").bind(root).fetch_all(&mut *tx).await?;
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
                sqlx::query("UPDATE issue_relations SET position=?3 WHERE kind='parent' AND source_id=?1 AND target_id=?2").bind(root).bind(id).bind(position as i64).execute(&mut *tx).await?;
            }
        }
    }
    if changed {
        touch(&mut tx, root, other).await?;
        if let Some(parent) = former_parent {
            touch(&mut tx, parent, parent).await?;
        }
    }
    Ok(tx)
}
async fn touch(tx: &mut Transaction<'_, Sqlite>, a: i64, b: i64) -> Result<()> {
    let rows: Vec<(i64, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT id,updated_at FROM issues WHERE id=?1 OR id=?2")
            .bind(a)
            .bind(b)
            .fetch_all(&mut **tx)
            .await?;
    for (id, updated) in rows {
        let next = super::time::next_revision(updated)?;
        sqlx::query("UPDATE issues SET updated_at=?2 WHERE id=?1")
            .bind(id)
            .bind(next)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
