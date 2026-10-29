use crate::backend::db::{
    action_settings::{organization_allows, Setting},
    models::{CiRun, ClusterNode},
    organizations::{administers, OrganizationOutcome},
    runners::{
        normalize_labels, workflow_allowed, Candidate, Group, GroupInput, Labels, Refusal,
        Registration, RegistrationBinding, Runner, Scope, Selection,
    },
};
use anyhow::Result;
use sqlx::{Sqlite, SqliteConnection as Connection, SqlitePool as Pool, Transaction};

async fn authorize(c: &mut Transaction<'_, Sqlite>, actor: i64, scope: Scope) -> Result<()> {
    match scope {
        Scope::Organization(org) => {
            let role = super::organizations::authority::authority(c, actor, org).await?;
            anyhow::ensure!(
                matches!(role,OrganizationOutcome::Applied(role) if administers(role)),
                Refusal::Denied
            );
        }
        Scope::Repository(repo) => anyhow::ensure!(
            super::discussions::lock_admin_authority(c, actor, repo).await?,
            Refusal::Denied
        ),
    }
    Ok(())
}
async fn default_group(c: &mut Connection, org: i64) -> Result<i64> {
    sqlx::query("INSERT INTO action_runner_groups(org_id,name,is_default) VALUES(?1,'Default',true) ON CONFLICT DO NOTHING").bind(org).execute(&mut *c).await?;
    Ok(
        sqlx::query_scalar("SELECT id FROM action_runner_groups WHERE org_id=?1 AND is_default")
            .bind(org)
            .fetch_one(c)
            .await?,
    )
}
async fn group(c: &mut Connection, org: i64, id: i64) -> Result<Group> {
    let mut row: Group =
        sqlx::query_as("SELECT * FROM action_runner_groups WHERE org_id=?1 AND id=?2")
            .bind(org)
            .bind(id)
            .fetch_optional(&mut *c)
            .await?
            .ok_or(Refusal::Missing)?;
    row.repositories = sqlx::query_scalar(
        "SELECT a.repo_id FROM action_runner_group_repositories a JOIN action_runner_groups g ON g.id=a.group_id JOIN repositories r ON r.id=a.repo_id AND r.owner_kind='org' AND r.owner_org_id=g.org_id AND r.owner_user_id IS NULL WHERE a.group_id=?1 ORDER BY a.repo_id",
    )
    .bind(id)
    .fetch_all(c)
    .await?;
    Ok(row)
}
/// Cancels the running workflows on one runner, or on every runner in one group. Other runs
/// keep going: an edit changes which jobs the runners claim next.
async fn stop_runners(
    tx: &mut Transaction<'_, Sqlite>,
    runner: Option<i64>,
    group: Option<i64>,
) -> Result<()> {
    let ids:Vec<i64>=sqlx::query_scalar("SELECT c.id FROM ci_runs c WHERE c.status='running' AND EXISTS(SELECT 1 FROM action_runners a JOIN nodes n ON n.id=a.node_id WHERE c.worker LIKE n.public_key || '/%' AND (a.id=?1 OR a.group_id=?2))").bind(runner).bind(group).fetch_all(&mut **tx).await?;
    super::ci::stop_ci_runs(tx, &ids).await
}
pub async fn list_groups(pool: &Pool, actor: i64, org: i64) -> Result<Vec<Group>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    default_group(&mut tx, org).await?;
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM action_runner_groups WHERE org_id=?1 ORDER BY id")
            .bind(org)
            .fetch_all(&mut *tx)
            .await?;
    let mut rows = Vec::new();
    for id in ids {
        rows.push(group(&mut tx, org, id).await?);
    }
    tx.commit().await?;
    Ok(rows)
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
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1 AND owner_kind='org' AND owner_org_id=?2 AND owner_user_id IS NULL AND (NOT ?3 OR NOT is_archived))").bind(repo).bind(org).bind(joining).fetch_one(&mut *c).await?;
        anyhow::ensure!(valid, Refusal::Invalid("selected_repository_ids"));
    }
    sqlx::query("DELETE FROM action_runner_group_repositories WHERE group_id=?1")
        .bind(id)
        .execute(&mut *c)
        .await?;
    for repo in ids {
        sqlx::query("INSERT INTO action_runner_group_repositories(group_id,repo_id) VALUES(?1,?2)")
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
        "SELECT id FROM action_runners WHERE org_id=?1 AND group_id=?2 ORDER BY id",
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
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM action_runners WHERE org_id=?1 AND repo_id IS NULL AND id=?2)").bind(org).bind(runner).fetch_one(&mut *c).await?;
        anyhow::ensure!(valid, Refusal::Invalid("runners"));
    }
    let default = default_group(c, org).await?;
    sqlx::query("UPDATE action_runners SET group_id=?3 WHERE org_id=?1 AND group_id=?2")
        .bind(org)
        .bind(id)
        .bind(default)
        .execute(&mut *c)
        .await?;
    for runner in &desired {
        sqlx::query("UPDATE action_runners SET group_id=?3 WHERE org_id=?1 AND id=?2")
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
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
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
    let duplicate:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM action_runner_groups WHERE org_id=?1 AND lower(name)=lower(?2) AND (?3 IS NULL OR id<>?3))").bind(org).bind(name).bind(id).fetch_one(&mut *tx).await?;
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
        Some(id)=> { sqlx::query("UPDATE action_runner_groups SET name=?2,visibility=?3,allows_public_repositories=?4,restricted_to_workflows=?5,selected_workflows=?6 WHERE id=?1").bind(id).bind(name).bind(visibility).bind(public).bind(restricted).bind(workflows).execute(&mut *tx).await?;id },
        None=>sqlx::query_scalar("INSERT INTO action_runner_groups(org_id,name,visibility,allows_public_repositories,restricted_to_workflows,selected_workflows) VALUES(?1,?2,?3,?4,?5,?6) RETURNING id").bind(org).bind(name).bind(visibility).bind(public).bind(restricted).bind(workflows).fetch_one(&mut *tx).await?,
    };
    if let Some(ids) = &input.selected_repository_ids {
        if visibility == "selected" {
            repositories_in(&mut tx, org, id, &Selection::Replace(ids.clone())).await?;
        } else {
            anyhow::ensure!(ids.is_empty(), Refusal::Invalid("selected_repository_ids"));
        }
    }
    if visibility != "selected" {
        sqlx::query("DELETE FROM action_runner_group_repositories WHERE group_id=?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if existing.is_some() && input.changes_access() {
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
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    let row = group(&mut tx, org, id).await?;
    anyhow::ensure!(
        !row.is_default,
        Refusal::Conflict("The default runner group cannot be deleted")
    );
    let default = default_group(&mut tx, org).await?;
    stop_runners(&mut tx, None, Some(id)).await?;
    sqlx::query("UPDATE action_runners SET group_id=?2 WHERE group_id=?1 AND org_id=?3")
        .bind(id)
        .bind(default)
        .bind(org)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM action_runner_groups WHERE id=?1 AND org_id=?2")
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
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    repositories_in(&mut tx, org, id, selection).await?;
    stop_runners(&mut tx, None, Some(id)).await?;
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
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, Scope::Organization(org)).await?;
    // Runners leaving the group and runners joining it both change what their runs may use.
    for runner in runners_in(&mut tx, org, id, selection).await? {
        stop_runners(&mut tx, Some(runner), None).await?;
    }
    tx.commit().await?;
    Ok(())
}
const RUNNERS:&str="SELECT a.*,false AS online,n.status AS node_status,n.last_seen AS node_last_seen,strftime('%Y-%m-%dT%H:%M:%f000Z','now') AS observed_at,EXISTS(SELECT 1 FROM ci_runs c WHERE c.status='running' AND c.worker LIKE n.public_key || '/%') AS busy,n.version FROM action_runners a LEFT JOIN nodes n ON n.id=a.node_id WHERE a.org_id IS NOT DISTINCT FROM ?1 AND a.repo_id IS NOT DISTINCT FROM ?2 ORDER BY a.id";
async fn runner_rows(c: &mut Connection, scope: Scope) -> Result<Vec<Runner>> {
    let (org, repo) = scope.ids();
    Ok(sqlx::query_as::<_, crate::backend::db::runners::RunnerProjection>(RUNNERS)
        .bind(org)
        .bind(repo)
        .fetch_all(c)
        .await?.into_iter().map(|row| row.into_runner()).collect())
}
pub async fn list_runners(pool: &Pool, actor: i64, scope: Scope) -> Result<Vec<Runner>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, scope).await?;
    let rows = runner_rows(&mut tx, scope).await?;
    tx.commit().await?;
    Ok(rows)
}
async fn runner(c: &mut Connection, scope: Scope, id: i64) -> Result<Runner> {
    runner_rows(c, scope)
        .await?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| Refusal::Missing.into())
}
async fn delete_in(tx: &mut Transaction<'_, Sqlite>, scope: Scope, id: i64) -> Result<()> {
    let row = runner(tx, scope, id).await?;
    stop_runners(tx, Some(row.id), None).await?;
    if let Some(node) = row.node_id {
        sqlx::query("UPDATE nodes SET status='revoked' WHERE id=?1 AND runner_managed")
            .bind(node)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("DELETE FROM action_runners WHERE id=?1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Removes a repository's own runners and their unused registrations. A transfer calls this:
/// the runners were registered under the former owner's authority.
pub(crate) async fn remove_repository_runners(c: &mut Connection, repo: i64) -> Result<()> {
    sqlx::query("UPDATE nodes SET status='revoked' WHERE runner_managed AND id IN (SELECT node_id FROM action_runners WHERE repo_id=?1)")
        .bind(repo)
        .execute(&mut *c)
        .await?;
    sqlx::query("DELETE FROM action_runners WHERE repo_id=?1")
        .bind(repo)
        .execute(&mut *c)
        .await?;
    sqlx::query("DELETE FROM join_tokens WHERE token_hash IN (SELECT token_hash FROM action_runner_tokens WHERE repo_id=?1)")
        .bind(repo)
        .execute(&mut *c)
        .await?;
    sqlx::query("DELETE FROM action_runner_remove_tokens WHERE repo_id=?1")
        .bind(repo)
        .execute(c)
        .await?;
    Ok(())
}
pub async fn delete_runner(pool: &Pool, actor: i64, scope: Scope, id: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
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
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
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
    sqlx::query("UPDATE action_runners SET labels=?2 WHERE id=?1")
        .bind(id)
        .bind(serde_json::to_string(&current)?)
        .execute(&mut *tx)
        .await?;
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
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, scope).await?;
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
    let runner_id = if let Some(name) = input.name {
        anyhow::ensure!(
            !name.trim().is_empty() && name.len() <= 100 && !name.chars().any(char::is_control),
            Refusal::Invalid("name")
        );
        let duplicate:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM action_runners WHERE org_id IS NOT DISTINCT FROM ?1 AND repo_id IS NOT DISTINCT FROM ?2 AND lower(name)=lower(?3))").bind(org).bind(repo).bind(name).fetch_one(&mut *tx).await?;
        anyhow::ensure!(!duplicate, Refusal::Invalid("name"));
        Some(sqlx::query_scalar::<_,i64>("INSERT INTO action_runners(org_id,repo_id,group_id,name,labels,ephemeral) VALUES(?1,?2,?3,?4,?5,true) RETURNING id").bind(org).bind(repo).bind(group_id).bind(name).bind(serde_json::to_string(&labels)?).fetch_one(&mut *tx).await?)
    } else {
        None
    };
    sqlx::query("INSERT INTO join_tokens(token_hash,roles,reusable,requires_approval,expires_at,runner_scoped) VALUES(?1,'[\"runner\"]',?2,false,?3,true)").bind(input.hash).bind(runner_id.is_none()).bind(super::time::encode(input.expires)).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO action_runner_tokens(token_hash,actor_id,org_id,repo_id,group_id,runner_id) VALUES(?1,?2,?3,?4,?5,?6)").bind(input.hash).bind(actor).bind(org).bind(repo).bind(group_id).bind(runner_id).execute(&mut *tx).await?;
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
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, scope).await?;
    let (org, repo) = scope.ids();
    sqlx::query("INSERT INTO action_runner_remove_tokens(token_hash,actor_id,org_id,repo_id,expires_at) VALUES(?1,?2,?3,?4,?5)").bind(hash).bind(actor).bind(org).bind(repo).bind(super::time::encode(expires)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn remove_with_token(pool: &Pool, id: i64, hash: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let token:Option<(i64,Option<i64>,Option<i64>)>=sqlx::query_as("SELECT actor_id,org_id,repo_id FROM action_runner_remove_tokens WHERE token_hash=?1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')").bind(hash).fetch_optional(&mut *tx).await?;
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
pub(super) async fn bind_registration(
    tx: &mut Transaction<'_, Sqlite>,
    hash: &str,
    node: &ClusterNode,
) -> Result<()> {
    let scoped: bool =
        sqlx::query_scalar("SELECT runner_scoped FROM join_tokens WHERE token_hash=?1")
            .bind(hash)
            .fetch_one(&mut **tx)
            .await?;
    if !scoped {
        return Ok(());
    }
    let binding:Option<RegistrationBinding>=sqlx::query_as("SELECT actor_id,org_id,repo_id,group_id,runner_id FROM action_runner_tokens WHERE token_hash=?1").bind(hash).fetch_optional(&mut **tx).await?;
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
            row.node_id.is_none()
                && node.name == crate::backend::db::runners::jit_node_name(id, hash),
            Refusal::Denied
        );
        sqlx::query("UPDATE action_runners SET node_id=?2 WHERE id=?1")
            .bind(id)
            .bind(node.id)
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("INSERT INTO action_runners(org_id,repo_id,group_id,node_id,name) VALUES(?1,?2,?3,?4,?5)").bind(org).bind(repo).bind(group_id).bind(node.id).bind(&node.name).execute(&mut **tx).await?;
    }
    sqlx::query("UPDATE nodes SET runner_managed=true WHERE id=?1")
        .bind(node.id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Claim holds repository authority and then node membership. Group edits take the same
/// organization authority lock, so eligibility cannot race a narrower policy into a claim.
pub(super) async fn eligible(c: &mut Connection, key: Option<&str>, run: &CiRun) -> Result<bool> {
    let Some(key) = key else {
        return Ok(run.runner_group.is_none());
    };
    let managed: bool = sqlx::query_scalar("SELECT runner_managed FROM nodes WHERE public_key=?1")
        .bind(key)
        .fetch_one(&mut *c)
        .await?;
    if !managed {
        return Ok(run.runner_group.is_none());
    }
    let row:Option<Candidate>=sqlx::query_as("SELECT a.org_id,a.repo_id,a.group_id,a.labels,a.used FROM action_runners a JOIN nodes n ON n.id=a.node_id WHERE n.public_key=?1").bind(key).fetch_optional(&mut *c).await?;
    let Some(Candidate {
        org_id: org,
        repo_id: repo,
        group_id,
        labels,
        used,
    }) = row
    else {
        return Ok(false);
    };
    if used {
        return Ok(false);
    }
    // The owning organization may keep its repositories off self-hosted runners.
    let policy: Option<String> = sqlx::query_scalar("SELECT o.rest_settings FROM repositories r JOIN org_actions_settings o ON o.org_id=r.owner_org_id AND r.owner_kind='org' WHERE r.id=?1")
        .bind(run.repo_id)
        .fetch_optional(&mut *c)
        .await?;
    if let Some(policy) = policy {
        let policy: serde_json::Value = serde_json::from_str(&policy)?;
        if !organization_allows(&policy, Setting::Runners, run.repo_id) {
            return Ok(false);
        }
    }
    let labels: Vec<String> = serde_json::from_str(&labels)?;
    let wanted: Vec<String> = serde_json::from_str(&run.runner_labels)?;
    if !wanted
        .iter()
        .all(|label| label == "self-hosted" || labels.contains(label))
    {
        return Ok(false);
    }
    if repo == Some(run.repo_id) {
        return Ok(run.runner_group.is_none());
    }
    let Some(org) = org else {
        return Ok(false);
    };
    let owner:Option<(String,Option<i64>,bool,String)>=sqlx::query_as("SELECT r.owner_kind,r.owner_org_id,r.is_private,o.name || '/' || r.name FROM repositories r JOIN organizations o ON o.id=r.owner_org_id WHERE r.id=?1").bind(run.repo_id).fetch_optional(&mut *c).await?;
    let Some((kind, owner, private, full_name)) = owner else {
        return Ok(false);
    };
    if kind != "org" || owner != Some(org) {
        return Ok(false);
    }
    let Some(group_id) = group_id else {
        return Ok(false);
    };
    let g = group(c, org, group_id).await?;
    if run
        .runner_group
        .as_ref()
        .is_some_and(|name| !g.name.eq_ignore_ascii_case(name))
    {
        return Ok(false);
    }
    if !g.allows_repository(run.repo_id, private) {
        return Ok(false);
    }
    if g.restricted_to_workflows {
        let allowed: Vec<String> = serde_json::from_str(&g.selected_workflows)?;
        let Some(blobs) = run.workflow_blobs.as_object() else {
            return Ok(false);
        };
        let reference = if run.event == "pull_request" {
            let number: Option<i64> =
                sqlx::query_scalar("SELECT number FROM pull_requests WHERE id=?1 AND repo_id=?2")
                    .bind(run.pull_request_id)
                    .bind(run.repo_id)
                    .fetch_optional(&mut *c)
                    .await?;
            let Some(number) = number else {
                return Ok(false);
            };
            format!("refs/pull/{number}/merge")
        } else if run.branch.starts_with("refs/") {
            run.branch.clone()
        } else {
            format!("refs/heads/{}", run.branch)
        };
        if blobs.is_empty() {
            return Ok(false);
        }
        for (path, blob) in blobs {
            // A called workflow is checked under the repository, path and ref it came from,
            // not under the file name admission stored it as.
            let allowed_here = match blob.get("source_path").and_then(|source| source.as_str()) {
                Some(source) => {
                    let source_repo = blob["repository_id"].as_i64().unwrap_or(run.repo_id);
                    let name: Option<String> = if source_repo == run.repo_id {
                        Some(full_name.clone())
                    } else {
                        sqlx::query_scalar("SELECT COALESCE(u.username, o.name) || '/' || r.name FROM repositories r LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations o ON o.id=r.owner_org_id WHERE r.id=?1")
                            .bind(source_repo)
                            .fetch_optional(&mut *c)
                            .await?
                    };
                    name.is_some_and(|name| {
                        workflow_allowed(
                            &allowed,
                            &format!("{name}/{source}"),
                            blob["ref"].as_str().unwrap_or(&reference),
                            blob["sha"].as_str().or(run.head_sha.as_deref()),
                        )
                    })
                }
                None => workflow_allowed(
                    &allowed,
                    &format!("{full_name}/{path}"),
                    &reference,
                    run.head_sha.as_deref(),
                ),
            };
            if !allowed_here {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
pub(super) async fn mark_claimed(c: &mut Connection, key: Option<&str>) -> Result<()> {
    if let Some(key) = key {
        sqlx::query("UPDATE action_runners SET used=true WHERE ephemeral AND node_id=(SELECT id FROM nodes WHERE public_key=?1)").bind(key).execute(c).await?;
    }
    Ok(())
}
pub(super) async fn retire(c: &mut Connection, worker: Option<&str>) -> Result<()> {
    if let Some(key) = worker.and_then(|w| w.split_once('/').map(|(key, _)| key)) {
        sqlx::query("UPDATE nodes SET status='revoked' WHERE public_key=?1 AND runner_managed AND EXISTS(SELECT 1 FROM action_runners a WHERE a.node_id=nodes.id AND a.ephemeral AND a.used)").bind(key).execute(&mut *c).await?;
        // A used ephemeral runner is gone, as on GitHub, so its name can be registered again.
        sqlx::query("DELETE FROM action_runners WHERE ephemeral AND used AND node_id=(SELECT id FROM nodes WHERE public_key=?1 AND runner_managed)").bind(key).execute(c).await?;
    }
    Ok(())
}
