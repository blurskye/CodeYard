use crate::pull_requests::team_reviewers::InvalidReviewTeam;
use crate::pull_requests::team_reviewers::ReviewTeamAction;
use anyhow::ensure;
use anyhow::Result;
use codeyard_database::models::PullRequest;
use codeyard_database::models::Team;
use codeyard_database::pool::PgPool;

const SCOPED: &str = "SELECT t.*,o.name AS org_slug,COALESCE(a.permission,0)::SMALLINT AS permission FROM teams t JOIN repositories r ON r.owner_kind='org' AND r.owner_org_id=t.org_id AND r.owner_user_id IS NULL JOIN organizations o ON o.id=t.org_id LEFT JOIN effective_team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id WHERE r.id=$1 AND (t.privacy='visible' OR EXISTS(SELECT 1 FROM org_memberships om WHERE om.org_id=t.org_id AND om.user_id=$2 AND om.role IN ('owner','admin')) OR EXISTS(SELECT 1 FROM effective_team_memberships tm JOIN org_memberships om ON om.org_id=t.org_id AND om.user_id=tm.user_id WHERE tm.team_id=t.id AND tm.user_id=$2 AND tm.role IN ('member','maintainer') AND om.role IN ('owner','admin','member')))";

pub async fn request_page(
    pool: &PgPool,
    window: &crate::pull_requests::team_reviewers::RequestWindow,
) -> Result<(i64, Vec<crate::pull_requests::reviews::ReviewRequestOrigin>)> {
    ensure!(
        window.offset >= 0 && (0..=100).contains(&window.limit),
        InvalidReviewTeam
    );
    let sql = crate::pull_requests::team_reviewers::request_roster_sql(
        "$",
        SCOPED,
        &codeyard_database::queries::postgres::repositories::access::readable_condition(),
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let total = if window.count {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({sql}) visible"))
            .bind(window.repo)
            .bind(window.viewer)
            .bind(window.pull)
            .bind(window.target.map(|target| target.0))
            .bind(window.target.map(|target| target.1))
            .fetch_one(&mut *tx)
            .await?
    } else {
        0
    };
    let rows = if window.limit == 0 {
        Vec::new()
    } else {
        sqlx::query_as(&format!(
            "{sql} ORDER BY kind,sort_name,COALESCE(user_id,team_id) LIMIT $6 OFFSET $7"
        ))
        .bind(window.repo)
        .bind(window.viewer)
        .bind(window.pull)
        .bind(window.target.map(|target| target.0))
        .bind(window.target.map(|target| target.1))
        .bind(window.limit)
        .bind(window.offset)
        .fetch_all(&mut *tx)
        .await?
    };
    tx.commit().await?;
    Ok((total, rows))
}

pub async fn by_ids(
    pool: &PgPool,
    repo: i64,
    viewer: Option<i64>,
    ids: &[i64],
) -> Result<Vec<Team>> {
    ensure!(ids.len() <= 100, InvalidReviewTeam);
    let readable = codeyard_database::queries::postgres::repositories::access::readable_condition();
    Ok(sqlx::query_as(&format!("{SCOPED} AND t.id=ANY($3) AND EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=$2 WHERE r.id=$1 AND {readable})")).bind(repo).bind(viewer).bind(ids).fetch_all(pool).await?)
}

pub async fn list(
    pool: &PgPool,
    repo: i64,
    pull: Option<i64>,
    viewer: Option<i64>,
) -> Result<Vec<crate::repositories::push_teams::TeamAccess>> {
    Ok(sqlx::query_as(&format!("{SCOPED} AND ($3::bigint IS NOT NULL OR a.permission BETWEEN 3 AND 5) AND ($3::bigint IS NULL OR EXISTS(SELECT 1 FROM pr_team_reviewers p WHERE p.pull_request_id=$3 AND p.team_id=t.id)) ORDER BY t.slug,t.id"))
        .bind(repo).bind(viewer).bind(pull).fetch_all(pool).await?)
}

pub async fn members(
    pool: &PgPool,
    repo: i64,
    teams: &[i64],
    after: i64,
) -> Result<Vec<codeyard_database::models::User>> {
    let access = codeyard_database::queries::postgres::repositories::access::USER_REPO_ACCESS_SQL;
    Ok(sqlx::query_as(&format!("SELECT DISTINCT u.* FROM effective_team_memberships m JOIN teams t ON t.id=m.team_id JOIN org_memberships o ON o.org_id=t.org_id AND o.user_id=m.user_id JOIN users u ON u.id=m.user_id JOIN repositories r ON r.owner_kind='org' AND r.owner_org_id=t.org_id AND r.owner_user_id IS NULL WHERE r.id=$1 AND t.id=ANY($2) AND u.id>$3 AND t.notifications_enabled AND m.role IN ('member','maintainer') AND o.role IN ('owner','admin','member') AND NOT r.is_archived AND NOT EXISTS(SELECT 1 FROM repository_migration_locks WHERE repo_id=r.id) AND ({access})>=3 AND EXISTS(SELECT 1 FROM effective_team_repo_access a WHERE a.team_id=t.id AND a.repo_id=r.id AND a.permission BETWEEN 3 AND 5) ORDER BY u.id LIMIT 100"))
        .bind(repo).bind(teams).bind(after).fetch_all(pool).await?)
}

pub async fn set_requests(
    pool: &PgPool,
    actor: i64,
    pull: i64,
    input: &crate::pull_requests::team_reviewers::ReviewRequests<'_>,
) -> Result<crate::pull_requests::team_reviewers::ReviewRequestChanges> {
    use crate::pull_requests::team_reviewers::ReviewRequestChanges;
    use codeyard_database::models::User;
    ensure!(
        input.users.len() + input.teams.len() <= 100,
        InvalidReviewTeam
    );
    let mut tx = pool.begin().await?;
    let (author, state, _) =
        crate::postgres::discussions::lock_pull_request_review_authority(&mut tx, actor, pull)
            .await?;
    ensure!(state == "open", InvalidReviewTeam);
    let repo: i64 = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=$1")
        .bind(pull)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(token) = input.token {
        ensure!(
            codeyard_db_identity::postgres::users::tokens::repository_write_on(
                &mut tx,
                token,
                actor,
                repo,
                "pull-requests"
            )
            .await?,
            crate::postgres::discussions::AUTHORITY_DENIED
        );
    }
    let old: Vec<i64> =
        sqlx::query_scalar("SELECT user_id FROM pr_reviewers WHERE pull_request_id=$1")
            .bind(pull)
            .fetch_all(&mut *tx)
            .await?;
    let old_ids: std::collections::HashSet<_> = old.into_iter().collect();
    let old_teams: Vec<i64> =
        sqlx::query_scalar("SELECT team_id FROM pr_team_reviewers WHERE pull_request_id=$1")
            .bind(pull)
            .fetch_all(&mut *tx)
            .await?;
    let mut users: Vec<User> = Vec::new();
    for id in input.users {
        if users.iter().any(|user| user.id == *id) {
            continue;
        }
        ensure!(
            *id != author
                && codeyard_database::queries::postgres::discussions::lock_write_authority(
                    &mut tx, *id, repo
                )
                .await?,
            crate::postgres::discussions::AUTHORITY_DENIED
        );
        users.push(
            sqlx::query_as("SELECT * FROM users WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?,
        );
    }
    let mut teams: Vec<Team> = Vec::new();
    for id in input.teams {
        if teams.iter().any(|team| team.id == *id) {
            continue;
        }
        teams.push(
            sqlx::query_as(&format!(
                "{SCOPED} AND t.id=$3 AND a.permission BETWEEN 3 AND 5 FOR SHARE OF t"
            ))
            .bind(repo)
            .bind(actor)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(InvalidReviewTeam)?,
        );
    }
    let replace = !input.union && !input.rerequest;
    if replace {
        sqlx::query("DELETE FROM pr_reviewers WHERE pull_request_id=$1 AND NOT user_id=ANY($2)")
            .bind(pull)
            .bind(input.users)
            .execute(&mut *tx)
            .await?;
        // Replacement changes only the roster the actor can see, including stale visible grants.
        sqlx::query(&format!("DELETE FROM pr_team_reviewers WHERE pull_request_id=$3 AND team_id IN (SELECT id FROM ({SCOPED}) visible) AND NOT team_id=ANY($4)")).bind(repo).bind(actor).bind(pull).bind(input.teams).execute(&mut *tx).await?;
    }
    let mut added_users = Vec::new();
    for user in users {
        if input.rerequest {
            sqlx::query("DELETE FROM pr_reviewers WHERE pull_request_id=$1 AND user_id=$2")
                .bind(pull)
                .bind(user.id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("INSERT INTO pr_reviewers(pull_request_id,user_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(pull).bind(user.id).execute(&mut *tx).await?;
        if input.rerequest || !old_ids.contains(&user.id) {
            added_users.push(user);
        }
    }
    let mut added_teams = Vec::new();
    for team in teams {
        if input.rerequest {
            sqlx::query("DELETE FROM pr_team_reviewers WHERE pull_request_id=$1 AND team_id=$2")
                .bind(pull)
                .bind(team.id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("INSERT INTO pr_team_reviewers(pull_request_id,team_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(pull).bind(team.id).execute(&mut *tx).await?;
        if input.rerequest || !old_teams.contains(&team.id) {
            added_teams.push(team);
        }
    }
    tx.commit().await?;
    Ok(ReviewRequestChanges {
        added_users,
        added_teams,
    })
}

pub async fn change(
    pool: &PgPool,
    actor: i64,
    pull: i64,
    slugs: &[String],
    action: ReviewTeamAction,
    expected: Option<&PullRequest>,
) -> Result<Vec<Team>> {
    ensure!(slugs.len() <= 100, InvalidReviewTeam);
    let remove = action == ReviewTeamAction::Remove;
    let mut tx = pool.begin().await?;
    let repo: i64 = sqlx::query_scalar("SELECT repo_id FROM pull_requests WHERE id=$1")
        .bind(pull)
        .fetch_one(&mut *tx)
        .await?;
    if expected.is_some() {
        crate::postgres::discussions::require_contribution_authority(&mut tx, actor, repo).await?;
    } else {
        crate::postgres::discussions::lock_pull_request_review_authority(&mut tx, actor, pull)
            .await?;
    }
    let current: PullRequest = sqlx::query_as("SELECT * FROM pull_requests WHERE id=$1 FOR UPDATE")
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
        let team: Team = sqlx::query_as(&format!("{SCOPED} AND LOWER(t.slug)=LOWER($3) AND ($4 OR a.permission BETWEEN 3 AND 5) FOR SHARE OF t"))
            .bind(repo).bind(actor).bind(slug).bind(remove).fetch_optional(&mut *tx).await?.ok_or(InvalidReviewTeam)?;
        if changed.iter().any(|previous: &Team| previous.id == team.id) {
            continue;
        }
        if expected.is_some() {
            // A member's decision on the current head stands in for the team; an older or
            // dismissed one, or the author's own, does not.
            let reviewed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_team_memberships m WHERE m.team_id=$2 AND m.user_id<>(SELECT author_id FROM pull_requests WHERE id=$1) AND COALESCE((SELECT v.state IN ('approved','changes_requested') AND v.commit_sha=$3 FROM pr_reviews v WHERE v.pull_request_id=$1 AND v.author_id=m.user_id AND v.state NOT IN ('commented','pending') ORDER BY v.created_at DESC,v.id DESC LIMIT 1),FALSE))")
                .bind(pull).bind(team.id).bind(&current.head_sha).fetch_one(&mut *tx).await?;
            if reviewed {
                continue;
            }
        }
        if action == ReviewTeamAction::Rerequest {
            sqlx::query("DELETE FROM pr_team_reviewers WHERE pull_request_id=$1 AND team_id=$2")
                .bind(pull)
                .bind(team.id)
                .execute(&mut *tx)
                .await?;
        }
        let result = if remove {
            sqlx::query("DELETE FROM pr_team_reviewers WHERE pull_request_id=$1 AND team_id=$2")
                .bind(pull)
                .bind(team.id)
                .execute(&mut *tx)
                .await?
        } else {
            sqlx::query("INSERT INTO pr_team_reviewers(pull_request_id,team_id,as_code_owner) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(pull).bind(team.id).bind(expected.is_some()).execute(&mut *tx).await?
        };
        if result.rows_affected() > 0 {
            changed.push(team);
        }
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn request_key(pool: &PgPool, team: bool, id: i64) -> Result<Option<(i64, i64)>> {
    let sql = if team {
        "SELECT pull_request_id,team_id FROM pr_team_reviewers WHERE id=$1"
    } else {
        "SELECT pull_request_id,user_id FROM pr_reviewers WHERE id=$1"
    };
    Ok(sqlx::query_as(sql).bind(id).fetch_optional(pool).await?)
}
