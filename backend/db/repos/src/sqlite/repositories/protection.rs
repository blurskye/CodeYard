use crate::repositories::protection::server_owned_status_context;
use crate::repositories::protection::BranchProtectionRuleUpsert;
use crate::repositories::protection::CommitStatusUpsert;
use crate::repositories::protection::ProtectionWriteRefusal;
use anyhow::Result;
use codeyard_db_core::models::BranchProtectionRule;
use codeyard_db_core::models::CommitStatus;
use sqlx::SqlitePool;

pub async fn read_protection_snapshot(
    pool: &SqlitePool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
) -> Result<crate::repositories::protection::ProtectionSnapshot> {
    let actor = actor.into();
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_settings_read_authority(
            &mut tx,
            actor.user_id,
            repo_id,
            true
        )
        .await?,
        ProtectionWriteRefusal::Denied
    );
    if let Some(credential) = actor.credential() {
        codeyard_db_identity::sqlite::users::transitions::require_read_account(
            &mut tx,
            actor.user_id,
            &credential,
        )
        .await?;
    }
    let rules: Vec<BranchProtectionRule> =
        sqlx::query_as("SELECT * FROM branch_protection_rules WHERE repo_id=?1 ORDER BY pattern")
            .bind(repo_id)
            .fetch_all(&mut *tx)
            .await?;
    let mut users = Vec::new();
    let mut teams = Vec::new();
    for rule in &rules {
        users.extend(
            rule.push_allowances
                .iter()
                .chain(&rule.force_push_allowances)
                .copied(),
        );
        teams.extend(rule.push_team_allowances.iter().copied());
    }
    users.sort_unstable();
    users.dedup();
    teams.sort_unstable();
    teams.dedup();
    let users: Vec<(i64, String)> = if users.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT id,username FROM users WHERE id IN (SELECT value FROM json_each(?1))",
        )
        .bind(serde_json::to_string(&users)?)
        .fetch_all(&mut *tx)
        .await?
    };
    let teams: Vec<(i64, String)> = if teams.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as("SELECT t.id,t.slug FROM teams t JOIN repositories r ON r.owner_org_id=t.org_id AND r.owner_kind='org' AND r.owner_user_id IS NULL WHERE r.id=?1 AND t.id IN (SELECT value FROM json_each(?2)) AND t.privacy IN ('visible','secret')")
            .bind(repo_id).bind(serde_json::to_string(&teams)?).fetch_all(&mut *tx).await?
    };
    tx.commit().await?;
    Ok(crate::repositories::protection::ProtectionSnapshot {
        repository_id: repo_id,
        rules,
        users: users.into_iter().collect(),
        teams: teams.into_iter().collect(),
    })
}

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
    mut input: BranchProtectionRuleUpsert<'_>,
) -> Result<BranchProtectionRule> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_admin_authority(
            &mut tx,
            input.actor_id,
            input.repo_id,
        )
        .await?,
        ProtectionWriteRefusal::Denied
    );
    if let Some(credential) = input.credential.as_ref() {
        let authority =
            codeyard_db_core::queries::credentials::Actor::from((input.actor_id, credential));
        anyhow::ensure!(
            codeyard_db_core::queries::sqlite::credentials::repository(
                &mut tx,
                &authority,
                input.repo_id,
                "administration",
                true
            )
            .await?,
            ProtectionWriteRefusal::Denied
        );
    }
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT github_settings FROM branch_protection_rules WHERE repo_id=?1 AND pattern=?2",
    )
    .bind(input.repo_id)
    .bind(input.pattern)
    .fetch_optional(&mut *tx)
    .await?;
    let previous: codeyard_db_core::models::BranchProtectionSettings = previous
        .map(|value| serde_json::from_str(&value))
        .transpose()?
        .unwrap_or_default();
    let validate_apps = input.github_settings.is_some();
    let mut settings = input
        .github_settings
        .take()
        .unwrap_or_else(|| previous.clone());
    // Signatures are changed only through their own subresource.
    settings.required_signatures = previous.required_signatures;
    crate::repositories::protection::normalize_check_bindings(&mut settings.checks)?;
    crate::repositories::protection::prune_check_bindings(
        &mut settings,
        input.require_status_checks,
        &input.required_status_checks,
    );
    if validate_apps {
        validate_push_apps_on(&mut tx, input.repo_id, &settings.push_apps).await?;
    }
    crate::repositories::protection::validate_branch_pattern(input.pattern)?;
    crate::repositories::protection::validate_required_checks(&input.required_status_checks)?;
    crate::repositories::push_teams::validate_count(
        input
            .push_team_allowances
            .len()
            .saturating_add(input.push_allowances.len())
            .saturating_add(settings.push_apps.len()),
    )?;
    input.push_team_allowances.sort_unstable();
    input.push_team_allowances.dedup();
    let allowed = crate::sqlite::repositories::push_teams::eligible_ids_on(
        &mut tx,
        input.repo_id,
        &input.push_team_allowances,
    )
    .await?;
    anyhow::ensure!(
        allowed.len() == input.push_team_allowances.len(),
        ProtectionWriteRefusal::InvalidPushTeams
    );
    let allowed = crate::sqlite::repositories::access::write_access_user_ids_on(
        &mut tx,
        input.repo_id,
        &input.push_allowances,
    )
    .await?;
    anyhow::ensure!(
        allowed.len() == input.push_allowances.len(),
        ProtectionWriteRefusal::InvalidPushAllowances
    );
    let full: bool = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM branch_protection_rules WHERE repo_id=?1) >= 100 AND NOT EXISTS(SELECT 1 FROM branch_protection_rules WHERE repo_id=?1 AND pattern=?2)")
        .bind(input.repo_id).bind(input.pattern).fetch_one(&mut *tx).await?;
    anyhow::ensure!(!full, ProtectionWriteRefusal::RuleLimit);
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
            force_push_allowances,
            require_code_owner_review,
            push_team_allowances,
            github_settings
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
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
            require_code_owner_review = EXCLUDED.require_code_owner_review,
            push_team_allowances = EXCLUDED.push_team_allowances,
            github_settings = EXCLUDED.github_settings,
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
    .bind(input.require_code_owner_review)
    .bind(serde_json::to_string(&input.push_team_allowances)?)
    .bind(serde_json::to_string(&settings)?)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rule)
}

pub async fn delete_branch_protection_rule(
    pool: &SqlitePool,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    pattern: &str,
) -> Result<bool> {
    let authority = actor_id.into();
    let actor_id = authority.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_admin_authority(
            &mut tx, actor_id, repo_id,
        )
        .await?,
        ProtectionWriteRefusal::Denied
    );
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::credentials::repository(
            &mut tx,
            &authority,
            repo_id,
            "administration",
            true
        )
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

pub async fn update_branch_protection_rule(
    pool: &SqlitePool,
    actor_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    pattern: &str,
    update: crate::repositories::protection::ProtectionUpdate,
) -> Result<BranchProtectionRule> {
    let authority = actor_id.into();
    let actor_id = authority.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_admin_authority(
            &mut tx, actor_id, repo_id
        )
        .await?,
        ProtectionWriteRefusal::Denied
    );
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::credentials::repository(
            &mut tx,
            &authority,
            repo_id,
            "administration",
            true
        )
        .await?,
        ProtectionWriteRefusal::Denied
    );
    let mut rule = sqlx::query_as::<_, BranchProtectionRule>(
        "SELECT * FROM branch_protection_rules WHERE repo_id=?1 AND pattern=?2",
    )
    .bind(repo_id)
    .bind(pattern)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ProtectionWriteRefusal::NotFound)?;
    if let Some(ids) = update.new_push_users()? {
        let allowed =
            crate::sqlite::repositories::access::write_access_user_ids_on(&mut tx, repo_id, &ids)
                .await?;
        anyhow::ensure!(
            allowed.len() == ids.len(),
            ProtectionWriteRefusal::InvalidPushAllowances
        );
    }
    if let Some(ids) = update.new_push_teams()? {
        let allowed =
            crate::sqlite::repositories::push_teams::eligible_ids_on(&mut tx, repo_id, &ids)
                .await?;
        anyhow::ensure!(
            allowed.len() == ids.len(),
            ProtectionWriteRefusal::InvalidPushTeams
        );
    }
    if let Some(ids) = update.new_push_apps()? {
        validate_push_apps_on(&mut tx, repo_id, &ids).await?;
    }
    crate::repositories::protection::apply_protection_update(&mut rule, update)?;
    let rule = sqlx::query_as::<_, BranchProtectionRule>("UPDATE branch_protection_rules SET enforce_admins=?3,require_status_checks=?4,required_status_checks=?5,require_up_to_date_branch=?6,restrict_pushes=?7,push_allowances=?8,require_pull_request=?9,required_approving_reviews=?10,dismiss_stale_reviews=?11,require_code_owner_review=?12,push_team_allowances=?13,github_settings=?14,updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE repo_id=?1 AND pattern=?2 RETURNING *")
        .bind(repo_id).bind(pattern).bind(rule.enforce_admins).bind(rule.require_status_checks).bind(serde_json::to_string(&rule.required_status_checks)?).bind(rule.require_up_to_date_branch).bind(rule.restrict_pushes).bind(serde_json::to_string(&rule.push_allowances)?).bind(rule.require_pull_request).bind(rule.required_approving_reviews).bind(rule.dismiss_stale_reviews).bind(rule.require_code_owner_review).bind(serde_json::to_string(&rule.push_team_allowances)?).bind(serde_json::to_string(&rule.github_settings)?).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(rule)
}

async fn validate_push_apps_on(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
    ids: &[i64],
) -> Result<()> {
    anyhow::ensure!(
        ids.len() <= crate::repositories::protection::MAX_ALLOWANCE_ACTORS,
        ProtectionWriteRefusal::InvalidPushAllowances
    );
    for id in ids {
        let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN github_app_installations i ON (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id) JOIN github_apps a ON a.id=i.app_id JOIN users u ON u.id=a.owner_id WHERE r.id=?1 AND i.app_id=?2 AND i.suspended_at IS NULL AND NOT u.is_banned AND NOT u.must_change_password AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar WHERE ar.installation_id=i.id AND ar.repo_id=r.id)))").bind(repo).bind(id).fetch_one(&mut *connection).await?;
        anyhow::ensure!(allowed, ProtectionWriteRefusal::InvalidPushAllowances);
    }
    Ok(())
}

pub async fn rename_branch_settings(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    old: &str,
    new: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_write_authority(&mut tx, actor, repo)
            .await?,
        ProtectionWriteRefusal::Denied
    );
    let admin: bool = sqlx::query_scalar("SELECT default_branch=?2 OR EXISTS(SELECT 1 FROM branch_protection_rules WHERE repo_id=?1 AND pattern=?2) FROM repositories WHERE id=?1").bind(repo).bind(old).fetch_one(&mut *tx).await?;
    if admin {
        anyhow::ensure!(
            codeyard_db_core::queries::sqlite::discussions::lock_admin_authority(
                &mut tx, actor, repo
            )
            .await?,
            ProtectionWriteRefusal::Denied
        );
    }
    sqlx::query("UPDATE branch_protection_rules SET pattern=?3 WHERE repo_id=?1 AND pattern=?2")
        .bind(repo)
        .bind(old)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE repositories SET default_branch=?3 WHERE id=?1 AND default_branch=?2")
        .bind(repo)
        .bind(old)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE pull_requests SET base_branch=CASE WHEN repo_id=?1 AND base_branch=?2 THEN ?3 ELSE base_branch END,head_branch=CASE WHEN head_repo_id=?1 AND head_branch=?2 THEN ?3 ELSE head_branch END,stack_version=stack_version+1,updated_at=MAX(strftime('%Y-%m-%dT%H:%M:%f000Z','now'),strftime('%Y-%m-%dT%H:%M:%f000Z',updated_at,'+0.001 seconds')) WHERE state='open' AND ((repo_id=?1 AND base_branch=?2) OR (head_repo_id=?1 AND head_branch=?2))")
        .bind(repo).bind(old).bind(new).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE pull_request_stacks SET base_branch=?3 WHERE repo_id=?1 AND base_branch=?2",
    )
    .bind(repo)
    .bind(old)
    .bind(new)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM issue_linked_branches WHERE repo_id=?1 AND name=?2 AND EXISTS(SELECT 1 FROM issue_linked_branches destination WHERE destination.issue_id=issue_linked_branches.issue_id AND destination.repo_id=?1 AND destination.name=?3)")
        .bind(repo).bind(old).bind(new).execute(&mut *tx).await?;
    sqlx::query("UPDATE issue_linked_branches SET name=?3 WHERE repo_id=?1 AND name=?2")
        .bind(repo)
        .bind(old)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM branch_redirects WHERE repo_id=?1 AND old_name=?2")
        .bind(repo)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE branch_redirects SET new_name=?3 WHERE repo_id=?1 AND new_name=?2")
        .bind(repo)
        .bind(old)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO branch_redirects(repo_id,old_name,new_name) VALUES(?1,?2,?3) ON CONFLICT(repo_id,old_name) DO UPDATE SET new_name=excluded.new_name")
        .bind(repo).bind(old).bind(new).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn branch_redirect(pool: &SqlitePool, repo: i64, old: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar(
            "SELECT new_name FROM branch_redirects WHERE repo_id=?1 AND old_name=?2",
        )
        .bind(repo)
        .bind(old)
        .fetch_optional(pool)
        .await?,
    )
}

pub async fn upsert_commit_status(
    pool: &SqlitePool,
    input: CommitStatusUpsert<'_>,
    token: Option<i64>,
) -> Result<CommitStatus> {
    anyhow::ensure!(
        !server_owned_status_context(input.context),
        "server-owned commit status context"
    );
    crate::repositories::protection::validate_status_metadata(&input)?;
    let actor_id = input.creator_id.ok_or(ProtectionWriteRefusal::Denied)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        codeyard_db_core::queries::sqlite::discussions::lock_write_authority(
            &mut tx,
            actor_id,
            input.repo_id,
        )
        .await?,
        ProtectionWriteRefusal::Denied
    );
    if let Some(token) = token {
        anyhow::ensure!(
            codeyard_db_identity::sqlite::users::tokens::repository_write_on(
                &mut tx,
                token,
                actor_id,
                input.repo_id,
                "statuses"
            )
            .await?,
            ProtectionWriteRefusal::Denied
        );
    }
    let sha = input.sha.to_ascii_lowercase();
    // GitHub compares contexts without case: a status updates the one differing only in case.
    let context: String = sqlx::query_scalar("SELECT context FROM commit_statuses WHERE repo_id=?1 AND sha=?2 AND lower(context)=lower(?3) ORDER BY id LIMIT 1")
        .bind(input.repo_id).bind(&sha).bind(input.context).fetch_optional(&mut *tx).await?
        .unwrap_or_else(|| input.context.to_owned());
    let full: bool = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM commit_statuses WHERE repo_id=?1 AND sha=?2 AND ci_run_id IS NULL) >= 1024 AND NOT EXISTS(SELECT 1 FROM commit_statuses WHERE repo_id=?1 AND sha=?2 AND context=?3)")
        .bind(input.repo_id).bind(&sha).bind(&context).fetch_one(&mut *tx).await?;
    anyhow::ensure!(!full, ProtectionWriteRefusal::StatusLimit);
    let full: bool = sqlx::query_scalar("SELECT COUNT(*) >= 1000 FROM commit_status_history WHERE repo_id=?1 AND sha=?2 AND context=?3")
        .bind(input.repo_id).bind(&sha).bind(&context).fetch_one(&mut *tx).await?;
    anyhow::ensure!(!full, ProtectionWriteRefusal::StatusHistoryLimit);
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
    .bind(&context)
    .bind(input.state)
    .bind(input.description)
    .bind(input.target_url)
    .bind(input.creator_id)
    .fetch_one(&mut *tx)
    .await?;
    let status = sqlx::query_as::<_, CommitStatus>("SELECT * FROM commit_statuses_latest WHERE id=(SELECT MAX(id) FROM commit_status_history WHERE status_id=?1)")
        .bind(status.id).fetch_one(&mut *tx).await?;
    tx.commit().await?;

    Ok(status)
}

pub async fn commit_status_page(
    pool: &SqlitePool,
    viewer: Option<i64>,
    repo: i64,
    sha: &str,
    offset: i64,
    limit: i64,
    history: bool,
) -> Result<Option<crate::repositories::protection::CommitStatusPage>> {
    // A zero limit reads only the counts, which the GraphQL rollup needs.
    anyhow::ensure!(
        (0..=100).contains(&limit) && offset >= 0,
        "Invalid status page"
    );
    let mut tx = pool.begin().await?;
    let visible: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=$2 WHERE r.id=$1 AND NOT r.is_gist AND (NOT r.is_private OR (r.owner_kind='user' AND (u.is_admin OR r.owner_user_id=u.id)) OR ({})>=1))", codeyard_db_core::queries::sqlite::repositories::access::USER_REPO_ACCESS_SQL))
        .bind(repo).bind(viewer).fetch_one(&mut *tx).await?;
    if !visible {
        return Ok(None);
    }
    let counts: Vec<(String, i64)> = if history {
        Vec::new()
    } else {
        sqlx::query_as("SELECT state,COUNT(*) FROM commit_statuses_latest WHERE repo_id=$1 AND sha=$2 GROUP BY state")
            .bind(repo).bind(sha).fetch_all(&mut *tx).await?
    };
    let total = if history {
        sqlx::query_scalar("SELECT COUNT(*) FROM commit_status_history WHERE repo_id=$1 AND sha=$2")
            .bind(repo)
            .bind(sha)
            .fetch_one(&mut *tx)
            .await?
    } else {
        counts.iter().map(|(_, count)| count).sum()
    };
    let table = if history {
        "commit_status_history"
    } else {
        "commit_statuses_latest"
    };
    let updated = if history { "created_at" } else { "updated_at" };
    let statuses = sqlx::query_as(&format!("SELECT id,repo_id,sha,context,state,substr(description,1,1024) AS description,substr(target_url,1,2048) AS target_url,creator_id,created_at,{updated} AS updated_at FROM {table} WHERE repo_id=$1 AND sha=$2 ORDER BY id DESC LIMIT $3 OFFSET $4"))
        .bind(repo).bind(sha).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(crate::repositories::protection::CommitStatusPage {
        total,
        counts,
        statuses,
    }))
}

pub async fn list_commit_statuses_page(
    pool: &SqlitePool,
    repo_id: i64,
    sha: &str,
    offset: i64,
    limit: i64,
    newest_first: bool,
) -> Result<Vec<CommitStatus>> {
    anyhow::ensure!(
        (1..=100).contains(&limit) && offset >= 0,
        "Invalid status page"
    );
    Ok(sqlx::query_as::<_, CommitStatus>("SELECT id,repo_id,sha,context,state,substr(description,1,1024) AS description,substr(target_url,1,2048) AS target_url,creator_id,created_at,updated_at FROM commit_statuses_latest WHERE repo_id=?1 AND sha=?2 ORDER BY CASE WHEN ?5 THEN id END DESC, context LIMIT ?3 OFFSET ?4")
        .bind(repo_id).bind(sha.to_ascii_lowercase()).bind(limit).bind(offset).bind(newest_first).fetch_all(pool).await?)
}

pub async fn list_commit_status_history_page(
    pool: &SqlitePool,
    repo_id: i64,
    sha: &str,
    offset: i64,
    limit: i64,
) -> Result<Vec<CommitStatus>> {
    anyhow::ensure!(
        (1..=100).contains(&limit) && offset >= 0,
        "Invalid status page"
    );
    Ok(sqlx::query_as::<_, CommitStatus>("SELECT id,repo_id,sha,context,state,substr(description,1,1024) AS description,substr(target_url,1,2048) AS target_url,creator_id,created_at,created_at AS updated_at FROM commit_status_history WHERE repo_id=?1 AND sha=?2 ORDER BY id DESC LIMIT ?3 OFFSET ?4")
        .bind(repo_id).bind(sha.to_ascii_lowercase()).bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn commit_status_history_count(
    pool: &SqlitePool,
    repo_id: i64,
    sha: &str,
) -> Result<i64> {
    Ok(
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM commit_status_history WHERE repo_id=?1 AND sha=?2",
        )
        .bind(repo_id)
        .bind(sha.to_ascii_lowercase())
        .fetch_one(pool)
        .await?,
    )
}

pub async fn commit_status_counts(
    pool: &SqlitePool,
    repo_id: i64,
    sha: &str,
) -> Result<Vec<(String, i64)>> {
    Ok(sqlx::query_as(
        "SELECT state,COUNT(*) FROM commit_statuses WHERE repo_id=?1 AND sha=?2 GROUP BY state",
    )
    .bind(repo_id)
    .bind(sha.to_ascii_lowercase())
    .fetch_all(pool)
    .await?)
}

pub async fn list_commit_statuses(
    pool: &SqlitePool,
    repo_id: i64,
    sha: &str,
) -> Result<Vec<CommitStatus>> {
    // Writers store lowercase SHAs, so a request for any case is lowercased to match them.
    let sha = sha.to_ascii_lowercase();
    let statuses = sqlx::query_as::<_, CommitStatus>(
        r#"SELECT id, repo_id, sha, context, state, substr(description,1,1024) AS description, substr(target_url,1,2048) AS target_url,
                  creator_id, created_at, updated_at
           FROM commit_statuses_latest
           WHERE repo_id = ?1 AND sha = ?2
           ORDER BY context"#,
    )
    .bind(repo_id)
    .bind(sha)
    .fetch_all(pool)
    .await?;

    Ok(statuses)
}
