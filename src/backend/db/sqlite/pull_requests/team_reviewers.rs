use crate::backend::db::sqlite::discussions;
use crate::backend::db::{
    models::{PullRequest, Team},
    pull_requests::team_reviewers::{InvalidReviewTeam, ReviewTeamAction},
};
use anyhow::{ensure, Result};
use sqlx::SqlitePool;

const SCOPED: &str = "SELECT t.*,o.name AS org_slug,COALESCE(a.permission,0) AS permission FROM teams t JOIN repositories r ON r.owner_kind='org' AND r.owner_org_id=t.org_id AND r.owner_user_id IS NULL JOIN organizations o ON o.id=t.org_id LEFT JOIN effective_team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id WHERE r.id=?1 AND (t.privacy='visible' OR EXISTS(SELECT 1 FROM org_memberships om WHERE om.org_id=t.org_id AND om.user_id=?2 AND om.role IN ('owner','admin')) OR EXISTS(SELECT 1 FROM effective_team_memberships tm JOIN org_memberships om ON om.org_id=t.org_id AND om.user_id=tm.user_id WHERE tm.team_id=t.id AND tm.user_id=?2 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member')))";

pub async fn list(
    pool: &SqlitePool,
    repo: i64,
    pull: Option<i64>,
    viewer: Option<i64>,
) -> Result<Vec<crate::backend::db::repositories::push_teams::TeamAccess>> {
    Ok(sqlx::query_as(&format!("{SCOPED} AND (?3 IS NOT NULL OR a.permission BETWEEN 3 AND 5) AND (?3 IS NULL OR EXISTS(SELECT 1 FROM pr_team_reviewers p WHERE p.pull_request_id=?3 AND p.team_id=t.id)) ORDER BY t.slug,t.id"))
        .bind(repo).bind(viewer).bind(pull).fetch_all(pool).await?)
}

pub async fn members(pool: &SqlitePool, repo: i64, teams: &[i64], after: i64) -> Result<Vec<crate::backend::db::models::User>> {
    let access = super::super::repositories::access::USER_REPO_ACCESS_SQL;
    Ok(sqlx::query_as(&format!("SELECT DISTINCT u.* FROM effective_team_memberships m JOIN teams t ON t.id=m.team_id JOIN org_memberships o ON o.org_id=t.org_id AND o.user_id=m.user_id JOIN users u ON u.id=m.user_id JOIN repositories r ON r.owner_kind='org' AND r.owner_org_id=t.org_id AND r.owner_user_id IS NULL WHERE r.id=?1 AND t.id IN (SELECT value FROM json_each(?2)) AND u.id>?3 AND t.notifications_enabled AND m.role IN ('member','maintainer') AND o.role IN ('owner','admin','member') AND NOT r.is_archived AND NOT EXISTS(SELECT 1 FROM repository_migration_locks WHERE repo_id=r.id) AND ({access})>=3 AND EXISTS(SELECT 1 FROM effective_team_repo_access a WHERE a.team_id=t.id AND a.repo_id=r.id AND a.permission BETWEEN 3 AND 5) ORDER BY u.id LIMIT 100"))
        .bind(repo).bind(serde_json::to_string(teams)?).bind(after).fetch_all(pool).await?)
}

pub async fn change(
    pool: &SqlitePool,
    actor: i64,
    pull: i64,
    slugs: &[String],
    action: ReviewTeamAction,
    expected: Option<&PullRequest>,
) -> Result<Vec<Team>> {
    ensure!(slugs.len() <= 100, InvalidReviewTeam);
    let remove = action == ReviewTeamAction::Remove;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let repo: i64 = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=?1")
        .bind(pull)
        .fetch_one(&mut *tx)
        .await?;
    if expected.is_some() {
        discussions::require_contribution_authority(&mut tx, actor, repo).await?;
    } else {
        discussions::lock_pull_request_review_authority(&mut tx, actor, pull).await?;
    }
    let current: PullRequest = sqlx::query_as("SELECT * FROM pull_requests WHERE id=?1")
        .bind(pull)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(remove || current.state == "open", InvalidReviewTeam);
    if let Some(expected) = expected {
        if expected.id != current.id
            || expected.head_sha != current.head_sha
            || expected.base_sha != current.base_sha
            || current.is_draft
        {
            return Ok(Vec::new());
        }
    }
    let mut changed = Vec::new();
    for slug in slugs {
        let team: Team = sqlx::query_as(&format!(
            "{SCOPED} AND LOWER(t.slug)=LOWER(?3) AND (?4 OR a.permission BETWEEN 3 AND 5)"
        ))
        .bind(repo)
        .bind(actor)
        .bind(slug)
        .bind(remove)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(InvalidReviewTeam)?;
        if changed.iter().any(|previous: &Team| previous.id == team.id) {
            continue;
        }
        if expected.is_some() {
            // A member's decision on the current head stands in for the team; an older or
            // dismissed one does not.
            let reviewed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_team_memberships m WHERE m.team_id=?2 AND COALESCE((SELECT v.state IN ('approved','changes_requested') AND v.commit_sha=?3 FROM pr_reviews v WHERE v.pull_request_id=?1 AND v.author_id=m.user_id AND v.state NOT IN ('commented','pending') ORDER BY v.created_at DESC,v.id DESC LIMIT 1),FALSE))")
                .bind(pull).bind(team.id).bind(&current.head_sha).fetch_one(&mut *tx).await?;
            if reviewed {
                continue;
            }
        }
        if action == ReviewTeamAction::Rerequest {
            sqlx::query("DELETE FROM pr_team_reviewers WHERE pull_request_id=?1 AND team_id=?2")
                .bind(pull)
                .bind(team.id)
                .execute(&mut *tx)
                .await?;
        }
        let result = if remove {
            sqlx::query("DELETE FROM pr_team_reviewers WHERE pull_request_id=?1 AND team_id=?2")
                .bind(pull)
                .bind(team.id)
                .execute(&mut *tx)
                .await?
        } else {
            sqlx::query("INSERT INTO pr_team_reviewers(pull_request_id,team_id,as_code_owner) VALUES(?1,?2,?3) ON CONFLICT DO NOTHING").bind(pull).bind(team.id).bind(expected.is_some()).execute(&mut *tx).await?
        };
        if result.rows_affected() > 0 {
            changed.push(team);
        }
    }
    tx.commit().await?;
    Ok(changed)
}
