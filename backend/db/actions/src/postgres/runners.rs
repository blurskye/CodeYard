use crate::runners::normalize_labels;
use crate::runners::Candidate;
use crate::runners::Eligibility;
use crate::runners::GroupInput;
use crate::runners::GroupQuery;
use crate::runners::Labels;
use crate::runners::Listing;
use crate::runners::Registration;
use crate::runners::RegistrationBinding;
use crate::runners::Selection;
use anyhow::Result;
use codeyard_database::models::CiRun;
use codeyard_database::models::ClusterNode;
use codeyard_database::pool::PgPool as Pool;
use codeyard_database::queries::organizations::administers;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::queries::runners::Group;
use codeyard_database::queries::runners::Refusal;
use codeyard_database::queries::runners::Runner;
use codeyard_database::queries::runners::RunnerQuery;
use codeyard_database::queries::runners::Scope;
use sqlx::PgConnection as Connection;
use sqlx::Postgres;
use sqlx::Transaction;

async fn authorize(c: &mut Transaction<'_, Postgres>, actor: i64, scope: Scope) -> Result<()> {
    match scope {
        Scope::Organization(org) => {
            let role = codeyard_database::queries::postgres::organizations::authority::authority(
                c, actor, org,
            )
            .await?;
            anyhow::ensure!(
                matches!(role,OrganizationOutcome::Applied(role) if administers(role)),
                Refusal::Denied
            );
        }
        Scope::Repository(repo) => anyhow::ensure!(
            codeyard_database::queries::postgres::discussions::lock_admin_authority(c, actor, repo)
                .await?,
            Refusal::Denied
        ),
    }
    Ok(())
}
async fn default_group(c: &mut Connection, org: i64) -> Result<i64> {
    sqlx::query("INSERT INTO action_runner_groups(org_id,name,is_default) VALUES($1,'Default',true) ON CONFLICT DO NOTHING").bind(org).execute(&mut *c).await?;
    Ok(
        sqlx::query_scalar("SELECT id FROM action_runner_groups WHERE org_id=$1 AND is_default")
            .bind(org)
            .fetch_one(c)
            .await?,
    )
}
async fn group(c: &mut Connection, org: i64, id: i64) -> Result<Group> {
    let mut row: Group =
        sqlx::query_as("SELECT * FROM action_runner_groups WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(id)
            .fetch_optional(&mut *c)
            .await?
            .ok_or(Refusal::Missing)?;
    row.repositories = sqlx::query_scalar(
        "SELECT a.repo_id FROM action_runner_group_repositories a JOIN action_runner_groups g ON g.id=a.group_id JOIN repositories r ON r.id=a.repo_id AND r.owner_kind='org' AND r.owner_org_id=g.org_id AND r.owner_user_id IS NULL WHERE a.group_id=$1 ORDER BY a.repo_id",
    )
    .bind(id)
    .fetch_all(c)
    .await?;
    Ok(row)
}
/// Cancels the running workflows on one runner, or on every runner in one group. Other runs
/// keep going: an edit changes which jobs the runners claim next.
async fn stop_runners(
    tx: &mut Transaction<'_, Postgres>,
    runner: Option<i64>,
    group: Option<i64>,
) -> Result<()> {
    let ids:Vec<i64>=sqlx::query_scalar("SELECT c.id FROM ci_runs c WHERE c.status='running' AND EXISTS(SELECT 1 FROM action_runners a JOIN nodes n ON n.id=a.node_id WHERE c.worker LIKE n.public_key || '/%' AND (a.id=$1 OR a.group_id=$2))").bind(runner).bind(group).fetch_all(&mut **tx).await?;
    codeyard_database::queries::postgres::ci::stop_ci_runs(tx, &ids).await
}
pub async fn authorize_scope(pool: &Pool, actor: i64, scope: Scope) -> Result<()> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, scope).await?;
    tx.commit().await?;
    Ok(())
}
const GROUP_FILTER: &str = "g.org_id=$1 AND ($2::bigint IS NULL OR g.id=$2) AND ($3::bigint IS NULL OR EXISTS(SELECT 1 FROM repositories r WHERE r.id=$3 AND r.owner_kind='org' AND r.owner_org_id=g.org_id AND r.owner_user_id IS NULL AND (r.is_private OR g.allows_public_repositories) AND (g.visibility='all' OR (g.visibility='private' AND r.is_private) OR (g.visibility='selected' AND EXISTS(SELECT 1 FROM action_runner_group_repositories s WHERE s.group_id=g.id AND s.repo_id=r.id)))))";
pub async fn list_groups(
    pool: &Pool,
    actor: i64,
    org: i64,
    query: &GroupQuery,
) -> Result<Listing<Group>> {
    query.page.validate()?;
    // Initialization is the only write. Ordinary reads neither lock the organization nor
    // reserve a writer, and authorization and projection share one snapshot.
    let initialized: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM action_runner_groups WHERE org_id=$1 AND is_default)",
    )
    .bind(org)
    .fetch_one(pool)
    .await?;
    if !initialized {
        let mut tx = pool.begin().await?;
        authorize(&mut tx, actor, Scope::Organization(org)).await?;
        default_group(&mut tx, org).await?;
        tx.commit().await?;
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let role = codeyard_database::queries::postgres::organizations::authority::read_authority(
        &mut tx, actor, org,
    )
    .await?;
    anyhow::ensure!(
        matches!(role, OrganizationOutcome::Applied(role) if administers(role)),
        Refusal::Denied
    );
    let total = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM action_runner_groups g WHERE {GROUP_FILTER}"
    )))
    .bind(org)
    .bind(query.id)
    .bind(query.repository)
    .fetch_one(&mut *tx)
    .await?;
    let mut rows: Vec<Group> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT g.* FROM action_runner_groups g WHERE {GROUP_FILTER} ORDER BY g.id LIMIT $4 OFFSET $5")))
        .bind(org).bind(query.id).bind(query.repository).bind(query.page.limit).bind(query.page.offset).fetch_all(&mut *tx).await?;
    let ids: Vec<_> = rows.iter().map(|g| g.id).collect();
    let selections: Vec<(i64,i64)> = sqlx::query_as("SELECT s.group_id,s.repo_id FROM action_runner_group_repositories s JOIN repositories r ON r.id=s.repo_id JOIN action_runner_groups g ON g.id=s.group_id WHERE s.group_id=ANY($1) AND r.owner_kind='org' AND r.owner_org_id=g.org_id AND r.owner_user_id IS NULL ORDER BY s.group_id,s.repo_id")
        .bind(&ids).fetch_all(&mut *tx).await?;
    let mut grants = std::collections::BTreeMap::<i64, Vec<i64>>::new();
    for (group, repo) in selections {
        grants.entry(group).or_default().push(repo);
    }
    for row in &mut rows {
        row.repositories = grants.remove(&row.id).unwrap_or_default();
    }
    tx.commit().await?;
    Ok(Listing { total, rows })
}
async fn repositories_in(
    c: &mut Connection,
    org: i64,
    id: i64,
    selection: &Selection,
) -> Result<()> {
    let row = group(c, org, id).await?;
    anyhow::ensure!(
        row.visibility == "selected",
        Refusal::Conflict("Runner group visibility is not selected")
    );
    let current = row.repositories.clone();
    let ids = selection.apply(row.repositories)?;
    let removed = match selection {
        Selection::Remove(id) => std::slice::from_ref(id),
        _ => &[],
    };
    for repo in ids.iter().chain(removed) {
        // Archival only stops a repository joining; one already selected may stay or leave.
        let joining = !current.contains(repo);
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND owner_kind='org' AND owner_org_id=$2 AND owner_user_id IS NULL AND (NOT $3 OR NOT is_archived))").bind(repo).bind(org).bind(joining).fetch_one(&mut *c).await?;
        anyhow::ensure!(valid, Refusal::Invalid("selected_repository_ids"));
    }
    sqlx::query("DELETE FROM action_runner_group_repositories WHERE group_id=$1")
        .bind(id)
        .execute(&mut *c)
        .await?;
    for repo in ids {
        sqlx::query("INSERT INTO action_runner_group_repositories(group_id,repo_id) VALUES($1,$2)")
            .bind(id)
            .bind(repo)
            .execute(&mut *c)
            .await?;
    }
    Ok(())
}
/// Sets the group's runners and returns the runners that joined or left it.
async fn runners_in(
    c: &mut Connection,
    org: i64,
    id: i64,
    selection: &Selection,
) -> Result<Vec<i64>> {
    let row = group(c, org, id).await?;
    let current: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM action_runners WHERE org_id=$1 AND group_id=$2 ORDER BY id",
    )
    .bind(org)
    .bind(id)
    .fetch_all(&mut *c)
    .await?;
    let desired = selection.apply(current.clone())?;
    anyhow::ensure!(
        !row.is_default || current.iter().all(|id| desired.contains(id)),
        Refusal::Conflict("Runners cannot be removed from the default group")
    );
    let removed = match selection {
        Selection::Remove(id) => std::slice::from_ref(id),
        _ => &[],
    };
    for runner in desired.iter().chain(removed) {
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM action_runners WHERE org_id=$1 AND repo_id IS NULL AND id=$2)").bind(org).bind(runner).fetch_one(&mut *c).await?;
        anyhow::ensure!(valid, Refusal::Invalid("runners"));
    }
    let default = default_group(c, org).await?;
    sqlx::query("UPDATE action_runners SET group_id=$3 WHERE org_id=$1 AND group_id=$2")
        .bind(org)
        .bind(id)
        .bind(default)
        .execute(&mut *c)
        .await?;
    for runner in &desired {
        sqlx::query("UPDATE action_runners SET group_id=$3 WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(runner)
            .bind(id)
            .execute(&mut *c)
            .await?;
    }
    let left = current.iter().filter(|runner| !desired.contains(runner));
    let joined = desired.iter().filter(|runner| !current.contains(runner));
    Ok(left.chain(joined).copied().collect())
}
pub async fn save_group(
    pool: &Pool,
    actor: i64,
    org: i64,
    id: Option<i64>,
    input: &GroupInput,
) -> Result<Group> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    input.validate()?;
    default_group(&mut tx, org).await?;
    let existing = match id {
        Some(id) => Some(group(&mut tx, org, id).await?),
        None => None,
    };
    let name = input
        .name
        .as_deref()
        .or_else(|| existing.as_ref().map(|g| g.name.as_str()))
        .ok_or(Refusal::Invalid("name"))?
        .trim();
    let duplicate:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM action_runner_groups WHERE org_id=$1 AND lower(name)=lower($2) AND ($3 IS NULL OR id<>$3))").bind(org).bind(name).bind(id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(!duplicate, Refusal::Invalid("name"));
    let visibility = input
        .visibility
        .as_deref()
        .or(existing.as_ref().map(|g| g.visibility.as_str()))
        .unwrap_or("all");
    let public = input.allows_public_repositories.unwrap_or(
        existing
            .as_ref()
            .is_some_and(|g| g.allows_public_repositories),
    );
    let restricted = input
        .restricted_to_workflows
        .unwrap_or(existing.as_ref().is_some_and(|g| g.restricted_to_workflows));
    let workflows = match &input.selected_workflows {
        Some(workflows) => serde_json::to_string(workflows)?,
        None => existing
            .as_ref()
            .map(|g| g.selected_workflows.clone())
            .unwrap_or_else(|| "[]".into()),
    };
    let id=match id {
        Some(id)=> { sqlx::query("UPDATE action_runner_groups SET name=$2,visibility=$3,allows_public_repositories=$4,restricted_to_workflows=$5,selected_workflows=$6 WHERE id=$1").bind(id).bind(name).bind(visibility).bind(public).bind(restricted).bind(workflows).execute(&mut *tx).await?;id },
        None=>sqlx::query_scalar("INSERT INTO action_runner_groups(org_id,name,visibility,allows_public_repositories,restricted_to_workflows,selected_workflows) VALUES($1,$2,$3,$4,$5,$6) RETURNING id").bind(org).bind(name).bind(visibility).bind(public).bind(restricted).bind(workflows).fetch_one(&mut *tx).await?,
    };
    if let Some(old) = &existing {
        if old.name != name {
            sqlx::query("UPDATE ci_runs SET runner_group=$3 WHERE status='queued' AND lower(runner_group)=lower($2) AND repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=$1)")
                .bind(org).bind(&old.name).bind(name).execute(&mut *tx).await?;
        }
    }
    if let Some(ids) = &input.selected_repository_ids {
        if visibility == "selected" {
            repositories_in(&mut tx, org, id, &Selection::Replace(ids.clone())).await?;
        } else {
            anyhow::ensure!(ids.is_empty(), Refusal::Invalid("selected_repository_ids"));
        }
    }
    if visibility != "selected" {
        sqlx::query("DELETE FROM action_runner_group_repositories WHERE group_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if existing
        .as_ref()
        .is_some_and(|existing| input.changes_access(existing))
    {
        stop_runners(&mut tx, None, Some(id)).await?;
    }
    if let Some(ids) = &input.runners {
        for runner in runners_in(&mut tx, org, id, &Selection::Replace(ids.clone())).await? {
            stop_runners(&mut tx, Some(runner), None).await?;
        }
    }
    let row = group(&mut tx, org, id).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn delete_group(pool: &Pool, actor: i64, org: i64, id: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    let row = group(&mut tx, org, id).await?;
    anyhow::ensure!(
        !row.is_default,
        Refusal::Conflict("The default runner group cannot be deleted")
    );
    let default = default_group(&mut tx, org).await?;
    let queued: Vec<i64> = sqlx::query_scalar("SELECT c.id FROM ci_runs c JOIN repositories r ON r.id=c.repo_id WHERE c.status='queued' AND lower(c.runner_group)=lower($2) AND r.owner_kind='org' AND r.owner_org_id=$1")
        .bind(org).bind(&row.name).fetch_all(&mut *tx).await?;
    codeyard_database::queries::postgres::ci::stop_ci_runs(&mut tx, &queued).await?;
    stop_runners(&mut tx, None, Some(id)).await?;
    sqlx::query("UPDATE action_runners SET group_id=$2 WHERE group_id=$1 AND org_id=$3")
        .bind(id)
        .bind(default)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM action_runner_groups WHERE id=$1 AND org_id=$2")
        .bind(id)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn group_repositories(
    pool: &Pool,
    actor: i64,
    org: i64,
    id: i64,
    selection: &Selection,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    let previous = group(&mut tx, org, id).await?.repositories;
    repositories_in(&mut tx, org, id, selection).await?;
    let current = group(&mut tx, org, id).await?.repositories;
    let removed: Vec<i64> = previous
        .into_iter()
        .filter(|repo| !current.contains(repo))
        .collect();
    if !removed.is_empty() {
        let runs: Vec<i64> = sqlx::query_scalar("SELECT c.id FROM ci_runs c WHERE c.status='running' AND c.repo_id=ANY($2) AND EXISTS(SELECT 1 FROM action_runners a JOIN nodes n ON n.id=a.node_id WHERE a.group_id=$1 AND c.worker LIKE n.public_key || '/%')")
            .bind(id).bind(&removed).fetch_all(&mut *tx).await?;
        codeyard_database::queries::postgres::ci::stop_ci_runs(&mut tx, &runs).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn group_runners(
    pool: &Pool,
    actor: i64,
    org: i64,
    id: i64,
    selection: &Selection,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    // Runners leaving the group and runners joining it both change what their runs may use.
    for runner in runners_in(&mut tx, org, id, selection).await? {
        stop_runners(&mut tx, Some(runner), None).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn runner_rows(c: &mut Connection, scope: Scope, query: &RunnerQuery) -> Result<Vec<Runner>> {
    let (org, repo) = scope.ids();
    Ok(
        sqlx::query_as::<_, codeyard_database::queries::runners::RunnerProjection>(
            sqlx::AssertSqlSafe(format!(
                "{RUNNERS} WHERE {RUNNER_FILTER} ORDER BY a.id LIMIT $6 OFFSET $7"
            )),
        )
        .bind(org)
        .bind(repo)
        .bind(query.id)
        .bind(query.group)
        .bind(&query.name)
        .bind(query.page.limit)
        .bind(query.page.offset)
        .fetch_all(c)
        .await?
        .into_iter()
        .map(|row| row.into_runner())
        .collect(),
    )
}
pub async fn list_runners(
    pool: &Pool,
    actor: i64,
    scope: Scope,
    query: &RunnerQuery,
) -> Result<Listing<Runner>> {
    query.page.validate()?;
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, scope).await?;
    let (org, repo) = scope.ids();
    let total = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM action_runners a WHERE {RUNNER_FILTER}"
    )))
    .bind(org)
    .bind(repo)
    .bind(query.id)
    .bind(query.group)
    .bind(&query.name)
    .fetch_one(&mut *tx)
    .await?;
    let rows = runner_rows(&mut tx, scope, query).await?;
    tx.commit().await?;
    Ok(Listing { total, rows })
}

async fn delete_in(tx: &mut Transaction<'_, Postgres>, scope: Scope, id: i64) -> Result<()> {
    let row = runner(tx, scope, id).await?;
    stop_runners(tx, Some(row.id), None).await?;
    if let Some(node) = row.node_id {
        sqlx::query("UPDATE nodes SET status='revoked' WHERE id=$1 AND runner_managed")
            .bind(node)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("DELETE FROM action_runners WHERE id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn delete_runner(pool: &Pool, actor: i64, scope: Scope, id: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, scope).await?;
    delete_in(&mut tx, scope, id).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn labels(
    pool: &Pool,
    actor: i64,
    scope: Scope,
    id: i64,
    change: &Labels,
) -> Result<Runner> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, scope).await?;
    let row = runner(&mut tx, scope, id).await?;
    let mut current: Vec<String> = serde_json::from_str(&row.labels)?;
    match change {
        Labels::Add(labels) => current.extend(normalize_labels(labels)?),
        Labels::Replace(labels) => current = normalize_labels(labels)?,
        Labels::Clear => current.clear(),
        Labels::Remove(label) => {
            let label = label.to_ascii_lowercase();
            anyhow::ensure!(current.contains(&label), Refusal::Missing);
            current.retain(|v| v != &label);
        }
    }
    current.sort();
    current.dedup();
    let current = normalize_labels(&current)?;
    sqlx::query("UPDATE action_runners SET labels=$2 WHERE id=$1")
        .bind(id)
        .bind(serde_json::to_string(&current)?)
        .execute(&mut *tx)
        .await?;
    if serde_json::from_str::<Vec<String>>(&row.labels)? != current {
        stop_runners(&mut tx, Some(id), None).await?;
    }
    let row = runner(&mut tx, scope, id).await?;
    tx.commit().await?;
    Ok(row)
}
pub async fn registration(
    pool: &Pool,
    actor: i64,
    scope: Scope,
    input: Registration<'_>,
) -> Result<Option<Runner>> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, scope).await?;
    if let Some(session) = input.session {
        anyhow::ensure!(
            codeyard_database::queries::postgres::users::transitions::live_session(
                &mut tx, actor, session
            )
            .await?,
            Refusal::Denied
        );
    }
    let labels = normalize_labels(input.labels)?;
    let (org, repo) = scope.ids();
    let group_id = if let Some(org) = org {
        let id = match input.group {
            Some(id) => id,
            None => default_group(&mut tx, org).await?,
        };
        group(&mut tx, org, id).await?;
        Some(id)
    } else {
        anyhow::ensure!(
            input.group.is_none_or(|id| id == 1),
            Refusal::Invalid("runner_group_id")
        );
        None
    };
    sqlx::query("DELETE FROM join_tokens WHERE expires_at<=clock_timestamp() AND token_hash IN (SELECT t.token_hash FROM action_runner_tokens t JOIN action_runners a ON a.id=t.runner_id WHERE a.org_id IS NOT DISTINCT FROM $1 AND a.repo_id IS NOT DISTINCT FROM $2 AND a.ephemeral AND a.node_id IS NULL AND NOT a.used)")
        .bind(org).bind(repo).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM action_runners a WHERE a.org_id IS NOT DISTINCT FROM $1 AND a.repo_id IS NOT DISTINCT FROM $2 AND a.ephemeral AND a.node_id IS NULL AND NOT a.used AND NOT EXISTS(SELECT 1 FROM action_runner_tokens t WHERE t.runner_id=a.id)")
        .bind(org).bind(repo).execute(&mut *tx).await?;
    let runner_id = if let Some(name) = input.name {
        anyhow::ensure!(
            !name.trim().is_empty() && name.len() <= 100 && !name.chars().any(char::is_control),
            Refusal::Invalid("name")
        );
        let duplicate:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM action_runners WHERE org_id IS NOT DISTINCT FROM $1 AND repo_id IS NOT DISTINCT FROM $2 AND lower(name)=lower($3))").bind(org).bind(repo).bind(name).fetch_one(&mut *tx).await?;
        anyhow::ensure!(!duplicate, Refusal::Invalid("name"));
        Some(sqlx::query_scalar::<_,i64>("INSERT INTO action_runners(org_id,repo_id,group_id,name,labels,ephemeral) VALUES($1,$2,$3,$4,$5,true) RETURNING id").bind(org).bind(repo).bind(group_id).bind(name).bind(serde_json::to_string(&labels)?).fetch_one(&mut *tx).await?)
    } else {
        None
    };
    sqlx::query("INSERT INTO join_tokens(token_hash,roles,reusable,requires_approval,expires_at,runner_scoped) VALUES($1,ARRAY['runner'],$2,false,$3,true)").bind(input.hash).bind(runner_id.is_none()).bind(input.expires).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO action_runner_tokens(token_hash,actor_id,org_id,repo_id,group_id,runner_id) VALUES($1,$2,$3,$4,$5,$6)").bind(input.hash).bind(actor).bind(org).bind(repo).bind(group_id).bind(runner_id).execute(&mut *tx).await?;
    let row = match runner_id {
        Some(id) => Some(runner(&mut tx, scope, id).await?),
        None => None,
    };
    tx.commit().await?;
    Ok(row)
}
pub async fn remove_token(
    pool: &Pool,
    actor: i64,
    scope: Scope,
    hash: &str,
    expires: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    authorize(&mut tx, actor, scope).await?;
    let (org, repo) = scope.ids();
    sqlx::query("INSERT INTO action_runner_remove_tokens(token_hash,actor_id,org_id,repo_id,expires_at) VALUES($1,$2,$3,$4,$5)").bind(hash).bind(actor).bind(org).bind(repo).bind(expires).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn remove_with_token(pool: &Pool, id: i64, hash: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    let token:Option<(i64,Option<i64>,Option<i64>)>=sqlx::query_as("SELECT actor_id,org_id,repo_id FROM action_runner_remove_tokens WHERE token_hash=$1 AND expires_at>CURRENT_TIMESTAMP").bind(hash).fetch_optional(&mut *tx).await?;
    let (actor, org, repo) = token.ok_or(Refusal::Denied)?;
    let scope = scope_ids(org, repo)?;
    authorize(&mut tx, actor, scope).await?;
    delete_in(&mut tx, scope, id).await?;
    tx.commit().await?;
    Ok(())
}
fn scope_ids(org: Option<i64>, repo: Option<i64>) -> Result<Scope> {
    match (org, repo) {
        (Some(org), None) => Ok(Scope::Organization(org)),
        (None, Some(repo)) => Ok(Scope::Repository(repo)),
        _ => Err(Refusal::Denied.into()),
    }
}
/// Called inside the existing pairing transaction, before a joined node becomes visible.
/// A missing binding never turns a formerly scoped credential into a cluster-wide invite.
pub async fn bind_registration(
    tx: &mut Transaction<'_, Postgres>,
    hash: &str,
    node: &ClusterNode,
    display_name: &str,
) -> Result<()> {
    let scoped: bool =
        sqlx::query_scalar("SELECT runner_scoped FROM join_tokens WHERE token_hash=$1")
            .bind(hash)
            .fetch_one(&mut **tx)
            .await?;
    if !scoped {
        return Ok(());
    }
    let binding:Option<RegistrationBinding>=sqlx::query_as("SELECT actor_id,org_id,repo_id,group_id,runner_id FROM action_runner_tokens WHERE token_hash=$1").bind(hash).fetch_optional(&mut **tx).await?;
    let RegistrationBinding {
        actor_id: actor,
        org_id: org,
        repo_id: repo,
        group_id,
        runner_id: id,
    } = binding.ok_or(Refusal::Denied)?;
    let scope = scope_ids(org, repo)?;
    authorize(tx, actor, scope).await?;
    if let Some(org) = org {
        group(tx, org, group_id.ok_or(Refusal::Denied)?).await?;
    }
    anyhow::ensure!(node.roles == ["runner"], Refusal::Denied);
    if let Some(id) = id {
        let row = runner(tx, scope, id).await?;
        anyhow::ensure!(
            row.node_id.is_none() && node.name == crate::runners::jit_node_name(id, hash),
            Refusal::Denied
        );
        sqlx::query("UPDATE action_runners SET node_id=$2 WHERE id=$1")
            .bind(id)
            .bind(node.id)
            .execute(&mut **tx)
            .await?;
    } else {
        let duplicate: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM action_runners WHERE org_id IS NOT DISTINCT FROM $1 AND repo_id IS NOT DISTINCT FROM $2 AND lower(name)=lower($3))")
            .bind(org).bind(repo).bind(display_name).fetch_one(&mut **tx).await?;
        anyhow::ensure!(!duplicate, Refusal::Invalid("name"));
        sqlx::query("INSERT INTO action_runners(org_id,repo_id,group_id,node_id,name) VALUES($1,$2,$3,$4,$5)").bind(org).bind(repo).bind(group_id).bind(node.id).bind(display_name).execute(&mut **tx).await?;
    }
    sqlx::query("UPDATE nodes SET runner_managed=true WHERE id=$1")
        .bind(node.id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Claim holds repository authority and then node membership. Group edits take the same
/// organization authority lock, so eligibility cannot race a narrower policy into a claim.
pub(super) async fn eligible(
    c: &mut Connection,
    key: Option<&str>,
    run: &CiRun,
    mode: &str,
    platforms: &[String],
) -> Result<bool> {
    let Some(key) = key else {
        return Eligibility::default().allows(run, mode, platforms);
    };
    let managed: bool = sqlx::query_scalar("SELECT runner_managed FROM nodes WHERE public_key=$1")
        .bind(key)
        .fetch_one(&mut *c)
        .await?;
    if !managed {
        return Eligibility::default().allows(run, mode, platforms);
    }
    let row:Option<Candidate>=sqlx::query_as("SELECT a.org_id,a.repo_id,a.group_id,a.labels,a.used FROM action_runners a JOIN nodes n ON n.id=a.node_id WHERE n.public_key=$1 FOR UPDATE OF a").bind(key).fetch_optional(&mut *c).await?;
    let mut facts = Eligibility {
        managed,
        runner: row,
        ..Default::default()
    };
    let policy: Option<String> = sqlx::query_scalar("SELECT o.rest_settings FROM repositories r JOIN org_actions_settings o ON o.org_id=r.owner_org_id AND r.owner_kind='org' WHERE r.id=$1")
        .bind(run.repo_id)
        .fetch_optional(&mut *c)
        .await?;
    facts.policy = policy;
    let owner:Option<(String,Option<i64>,bool,String)>=sqlx::query_as("SELECT r.owner_kind,r.owner_org_id,r.is_private,o.name || '/' || r.name FROM repositories r JOIN organizations o ON o.id=r.owner_org_id WHERE r.id=$1").bind(run.repo_id).fetch_optional(&mut *c).await?;
    facts.owner = owner;
    if let Some((org, id)) = facts.runner.as_ref().and_then(|r| r.org_id.zip(r.group_id)) {
        facts.group = Some(group(c, org, id).await?);
    }
    if facts
        .group
        .as_ref()
        .is_some_and(|g| g.restricted_to_workflows)
    {
        if run.event == "pull_request" {
            let number: Option<i32> =
                sqlx::query_scalar("SELECT number FROM pull_requests WHERE id=$1 AND repo_id=$2")
                    .bind(run.pull_request_id)
                    .bind(run.repo_id)
                    .fetch_optional(&mut *c)
                    .await?;
            facts.pull_number = number.map(i64::from);
        }
        if let Some(blobs) = run.workflow_blobs.as_object() {
            for repo in blobs
                .values()
                .filter_map(|blob| blob["repository_id"].as_i64())
            {
                if repo != run.repo_id && !facts.source_names.contains_key(&repo) {
                    let name: Option<String> = sqlx::query_scalar("SELECT COALESCE(u.username, o.name) || '/' || r.name FROM repositories r LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations o ON o.id=r.owner_org_id WHERE r.id=$1")
                        .bind(repo).fetch_optional(&mut *c).await?;
                    if let Some(name) = name {
                        facts.source_names.insert(repo, name);
                    }
                }
            }
        }
    }
    facts.allows(run, mode, platforms)
}
pub(super) async fn mark_claimed(c: &mut Connection, key: Option<&str>) -> Result<()> {
    if let Some(key) = key {
        sqlx::query("UPDATE action_runners SET used=true WHERE ephemeral AND node_id=(SELECT id FROM nodes WHERE public_key=$1)").bind(key).execute(c).await?;
    }
    Ok(())
}

use codeyard_database::queries::postgres::runners::runner;

use codeyard_database::queries::postgres::runners::RUNNER_FILTER;

use codeyard_database::queries::postgres::runners::RUNNERS;
