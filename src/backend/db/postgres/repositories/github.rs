use crate::backend::db::{
    models::Repository,
    repositories::github::{list_sql, RepositoryListQuery},
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn repository_teams(
    pool: &PgPool,
    actor: i64,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Option<Vec<crate::backend::db::repositories::github::RepositoryTeam>>> {
    use crate::backend::db::{
        models::OrgRole, organizations::OrganizationOutcome,
        repositories::github::REPOSITORY_TEAMS_SQL,
    };
    anyhow::ensure!(
        (1..=101).contains(&limit) && offset >= 0,
        "Invalid team page"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
    // Anyone who can read the repository may list the teams of its organization they can see.
    if !super::access::user_repo_access_in_transaction(&mut tx, actor, repo_id)
        .await?
        .can_read()
    {
        return Ok(None);
    }
    let Some(org) = sqlx::query_scalar::<_, i64>(
        "SELECT owner_org_id FROM repositories WHERE id=$1 AND owner_kind='org'",
    )
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Some(vec![]));
    };
    let OrganizationOutcome::Applied(role) =
        crate::backend::db::postgres::organizations::authority::read_authority(&mut tx, actor, org)
            .await?
    else {
        return Ok(None);
    };
    let privileged = matches!(role, OrgRole::Owner | OrgRole::Admin);
    let mut teams: Vec<crate::backend::db::repositories::github::RepositoryTeam> = sqlx::query_as(REPOSITORY_TEAMS_SQL)
        .bind(repo_id)
        .bind(org)
        .bind(privileged)
        .bind(actor)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *tx)
        .await?;
    let parents: Vec<_> = teams.iter().filter(|row| row.team.privacy == "visible").filter_map(|row| row.team.parent_team_id).collect();
    let parents: Vec<crate::backend::db::models::Team> = sqlx::query_as("SELECT * FROM teams WHERE id=ANY($1) AND org_id=$2 AND privacy='visible'").bind(parents).bind(org).fetch_all(&mut *tx).await?;
    let parents: std::collections::HashMap<_, _> = parents.into_iter().map(|team| (team.id, team)).collect();
    for row in &mut teams { if row.team.privacy == "visible" { row.parent = row.team.parent_team_id.and_then(|id| parents.get(&id).cloned()); } }
    tx.commit().await?;
    Ok(Some(teams))
}

pub async fn repository_pushed_at(
    pool: &PgPool,
    repo_id: i64,
) -> Result<chrono::DateTime<chrono::Utc>> {
    Ok(sqlx::query_scalar("SELECT COALESCE((SELECT MAX(p.created_at) FROM repository_pushes p WHERE p.repo_id=r.id),r.created_at) FROM repositories r WHERE r.id=$1")
        .bind(repo_id).fetch_one(pool).await?)
}

pub async fn list_repositories(
    pool: &PgPool,
    query: RepositoryListQuery<'_>,
) -> Result<Vec<Repository>> {
    let sql = list_sql(
        &query,
        super::access::USER_REPO_ACCESS_SQL,
        &super::access::readable_condition(),
    );
    Ok(sqlx::query_as(&sql)
        .bind(query.viewer)
        .bind(query.target_id())
        .bind(query.since)
        .bind(query.visibility)
        .bind(query.kind)
        .bind(query.limit)
        .bind(query.offset)
        .fetch_all(pool)
        .await?)
}

pub async fn count_repositories(pool: &PgPool, query: RepositoryListQuery<'_>) -> Result<i64> {
    let sql = list_sql(
        &query,
        super::access::USER_REPO_ACCESS_SQL,
        &super::access::readable_condition(),
    );
    let sql = format!("SELECT COUNT(*) FROM ({sql}) readable_repositories");
    Ok(sqlx::query_scalar(&sql)
        .bind(query.viewer)
        .bind(query.target_id())
        .bind(query.since)
        .bind(query.visibility)
        .bind(query.kind)
        .bind(i64::MAX)
        .bind(0_i64)
        .fetch_one(pool)
        .await?)
}
