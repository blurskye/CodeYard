use crate::backend::db::models::{BranchProtectionRule, CommitStatus};
use crate::backend::db::repositories::protection::{
    server_owned_status_context, BranchProtectionRuleUpsert, CommitStatusUpsert,
    ProtectionWriteRefusal,
};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn list_branch_protection_rules(
    pool: &SqlitePool,
    repo_id: i64,
) -> Result<Vec<BranchProtectionRule>> {
    let rules = sqlx::query_as::<_, BranchProtectionRule>(
        "SELECT * FROM branch_protection_rules WHERE repo_id = ?1 ORDER BY pattern",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;

    Ok(rules)
}

pub async fn upsert_branch_protection_rule(
    pool: &SqlitePool,
    input: BranchProtectionRuleUpsert<'_>,
) -> Result<BranchProtectionRule> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_admin_authority(
            &mut tx,
            input.actor_id,
            input.repo_id,
        )
        .await?,
        ProtectionWriteRefusal::Denied
    );
    crate::backend::db::repositories::protection::validate_branch_pattern(input.pattern)?;
    let rule = sqlx::query_as::<_, BranchProtectionRule>(
        r#"
        INSERT INTO branch_protection_rules (
            repo_id,
            pattern,
            require_pull_request,
            required_approving_reviews,
            dismiss_stale_reviews,
            require_status_checks,
            required_status_checks,
            require_up_to_date_branch,
            require_linear_history,
            block_force_pushes,
            block_deletions,
            restrict_pushes,
            push_allowances,
            enforce_admins,
            require_conversation_resolution,
            force_push_allowances
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
        ON CONFLICT (repo_id, pattern)
        DO UPDATE SET
            require_pull_request = EXCLUDED.require_pull_request,
            required_approving_reviews = EXCLUDED.required_approving_reviews,
            dismiss_stale_reviews = EXCLUDED.dismiss_stale_reviews,
            require_status_checks = EXCLUDED.require_status_checks,
            required_status_checks = EXCLUDED.required_status_checks,
            require_up_to_date_branch = EXCLUDED.require_up_to_date_branch,
            require_linear_history = EXCLUDED.require_linear_history,
            block_force_pushes = EXCLUDED.block_force_pushes,
            block_deletions = EXCLUDED.block_deletions,
            restrict_pushes = EXCLUDED.restrict_pushes,
            push_allowances = EXCLUDED.push_allowances,
            enforce_admins = EXCLUDED.enforce_admins,
            require_conversation_resolution = EXCLUDED.require_conversation_resolution,
            force_push_allowances = EXCLUDED.force_push_allowances,
            updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
        RETURNING *
        "#,
    )
    .bind(input.repo_id)
    .bind(input.pattern)
    .bind(input.require_pull_request)
    .bind(input.required_approving_reviews)
    .bind(input.dismiss_stale_reviews)
    .bind(input.require_status_checks)
    .bind(serde_json::to_string(&input.required_status_checks)?)
    .bind(input.require_up_to_date_branch)
    .bind(input.require_linear_history)
    .bind(input.block_force_pushes)
    .bind(input.block_deletions)
    .bind(input.restrict_pushes)
    .bind(serde_json::to_string(&input.push_allowances)?)
    .bind(input.enforce_admins)
    .bind(input.require_conversation_resolution)
    .bind(serde_json::to_string(&input.force_push_allowances)?)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rule)
}

pub async fn delete_branch_protection_rule(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    pattern: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_admin_authority(&mut tx, actor_id, repo_id,)
            .await?,
        ProtectionWriteRefusal::Denied
    );
    let result =
        sqlx::query("DELETE FROM branch_protection_rules WHERE repo_id = ?1 AND pattern = ?2")
            .bind(repo_id)
            .bind(pattern)
            .execute(&mut *tx)
            .await?;
    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

pub async fn upsert_commit_status(
    pool: &SqlitePool,
    input: CommitStatusUpsert<'_>,
) -> Result<CommitStatus> {
    anyhow::ensure!(
        !server_owned_status_context(input.context),
        "server-owned commit status context"
    );
    let actor_id = input.creator_id.ok_or(ProtectionWriteRefusal::Denied)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        crate::backend::db::sqlite::discussions::lock_write_authority(
            &mut tx,
            actor_id,
            input.repo_id,
        )
        .await?,
        ProtectionWriteRefusal::Denied
    );
    let sha = input.sha.to_ascii_lowercase();
    let status = sqlx::query_as::<_, CommitStatus>(
        r#"
        INSERT INTO commit_statuses (repo_id, sha, context, state, description, target_url, creator_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT (repo_id, sha, context)
        DO UPDATE SET
            state = EXCLUDED.state,
            description = EXCLUDED.description,
            target_url = EXCLUDED.target_url,
            creator_id = EXCLUDED.creator_id,
            updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
        WHERE commit_statuses.ci_run_id IS NULL
        RETURNING *
        "#,
    )
    .bind(input.repo_id)
    .bind(sha)
    .bind(input.context)
    .bind(input.state)
    .bind(input.description)
    .bind(input.target_url)
    .bind(input.creator_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(status)
}

pub async fn list_commit_statuses(
    pool: &SqlitePool,
    repo_id: i64,
    sha: &str,
) -> Result<Vec<CommitStatus>> {
    // Writers store lowercase SHAs, so a request for any case is lowercased to match them.
    let sha = sha.to_ascii_lowercase();
    let statuses = sqlx::query_as::<_, CommitStatus>(
        r#"SELECT id, repo_id, sha, context, state, description, target_url,
                  creator_id, created_at, updated_at
           FROM commit_statuses
           WHERE repo_id = ?1 AND sha = ?2
           ORDER BY context"#,
    )
    .bind(repo_id)
    .bind(sha)
    .fetch_all(pool)
    .await?;

    Ok(statuses)
}
