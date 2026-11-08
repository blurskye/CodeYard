use crate::assignees::AssignedItem;
use crate::issue_attributes::ValueInput;
use crate::labels::LabelTarget;
use crate::suggestions::Action;
use crate::suggestions::ListFilter;
use crate::suggestions::Proposal;
use crate::suggestions::Refusal;
use crate::suggestions::Resolution;
use crate::suggestions::Suggestion;
use anyhow::Result;
use codeyard_database::models::Issue;
use codeyard_database::pool::PgPool;
use serde_json::Value;
use sqlx::PgConnection;
use sqlx::Postgres;
use sqlx::Transaction;

async fn permission(c: &mut PgConnection, repo: i64, actor: i64, triage: bool) -> Result<bool> {
    let allowed:bool=sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$2 WHERE r.id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND ((NOT $3 AND ({})) OR (({})>=2) OR (r.owner_kind='user' AND u.is_admin)))",codeyard_database::queries::postgres::repositories::access::readable_condition(),codeyard_database::queries::postgres::repositories::access::USER_REPO_ACCESS_SQL)).bind(repo).bind(actor).bind(triage).fetch_one(&mut *c).await?;
    if !allowed {
        return Ok(false);
    }
    let (admin,private):(bool,bool)=sqlx::query_as("SELECT u.is_admin,r.is_private FROM users u CROSS JOIN repositories r WHERE u.id=$1 AND r.id=$2").bind(actor).bind(repo).fetch_one(&mut *c).await?;
    Ok(admin
        || !(triage || private)
        || !codeyard_database::queries::postgres::users::blocks::repository_interaction_blocked(
            c, actor, repo,
        )
        .await?)
}
pub async fn can_triage(pool: &PgPool, repo: i64, actor: i64) -> Result<bool> {
    permission(&mut *pool.acquire().await?, repo, actor, true).await
}
pub async fn enabled(pool: &PgPool, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT COALESCE((SELECT enabled FROM repository_issue_suggestions WHERE repo_id=$1),false)").bind(repo).fetch_one(pool).await?)
}
pub async fn set_enabled(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
    enabled: bool,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        codeyard_database::queries::postgres::discussions::lock_admin_authority(
            &mut tx, actor, repo
        )
        .await?,
        Refusal::Denied
    );
    anyhow::ensure!(
        codeyard_database::queries::postgres::users::transitions::credential_live(
            &mut tx, actor, credential, true
        )
        .await?,
        Refusal::Denied
    );
    crate::postgres::issue_attributes::lock_metadata(&mut tx).await?;
    sqlx::query("INSERT INTO repository_issue_suggestions(repo_id,enabled) VALUES ($1,$2) ON CONFLICT(repo_id) DO UPDATE SET enabled=excluded.enabled").bind(repo).bind(enabled).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
async fn issue_scope(c: &mut PgConnection, repo: i64, issue: i64) -> Result<()> {
    let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issues i JOIN repository_issue_suggestions s ON s.repo_id=i.repo_id WHERE i.id=$1 AND i.repo_id=$2 AND s.enabled)").bind(issue).bind(repo).fetch_one(c).await?;
    anyhow::ensure!(exists, Refusal::Missing);
    Ok(())
}
pub async fn list(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    issue: i64,
    filter: &ListFilter<'_>,
) -> Result<Vec<Suggestion>> {
    filter.validate()?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    issue_scope(&mut tx, repo, issue).await?;
    anyhow::ensure!(
        permission(&mut tx, repo, actor, !filter.own).await?,
        Refusal::Denied
    );
    let fields =
        crate::postgres::issue_attributes::catalog_connection(&mut tx, repo, Some(actor), "field")
            .await?;
    let ids = serde_json::to_string(&fields.iter().map(|f| f.id).collect::<Vec<_>>())?;
    let rows=sqlx::query_as(&format!("SELECT s.* FROM issue_suggestions s WHERE s.issue_id=$1 AND ($3='all' OR s.state=$3) AND ($4::TEXT IS NULL OR s.action=$4) AND (NOT $5 OR s.actor_id=$2) AND (s.action<>'add_field' OR s.target_id IN(SELECT CAST(value AS BIGINT) FROM jsonb_array_elements_text($6::jsonb))) AND (s.action<>'close_issue' OR s.target_id IS NULL OR EXISTS(SELECT 1 FROM issues target JOIN repositories r ON r.id=target.repo_id LEFT JOIN users u ON u.id=$2 WHERE target.id=s.target_id AND ({}) AND (COALESCE(u.username,'')<>$10 OR r.id=$9))) ORDER BY s.created_at {order},s.id {order} LIMIT $7 OFFSET $8",codeyard_database::queries::postgres::repositories::access::readable_condition(),order=if filter.descending{"DESC"}else{"ASC"})).bind(issue).bind(actor).bind(filter.state).bind(filter.action).bind(filter.own).bind(ids).bind(filter.limit).bind(filter.offset).bind(repo).bind(codeyard_database::queries::ci_tokens::BOT).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}
struct Prepared {
    value: Value,
    name: String,
    current: bool,
}
async fn prepare(
    tx: &mut Transaction<'_, Postgres>,
    repo: i64,
    actor: i64,
    issue: &Issue,
    p: &Proposal,
) -> Result<Prepared> {
    p.validate()?;
    let target = p.target_id;
    let (value, name, current) = match p.action {
        Action::SetType => {
            anyhow::ensure!(p.value.is_null() || p.value.is_string(), Refusal::Invalid);
            let old =
                crate::postgres::issue_attributes::assigned_connection(tx, repo, issue.id).await?;
            if let Some(id) = target {
                let a = crate::postgres::issue_attributes::catalog_connection(
                    tx,
                    repo,
                    Some(actor),
                    "type",
                )
                .await?
                .into_iter()
                .find(|a| a.id == id && a.is_enabled)
                .ok_or(Refusal::Invalid)?;
                (
                    Value::String(a.name.clone()),
                    a.name,
                    old.is_some_and(|a| a.id == id),
                )
            } else {
                (Value::Null, "None".into(), old.is_none())
            }
        }
        Action::AddLabel => {
            anyhow::ensure!(p.value.is_null() || p.value.is_string(), Refusal::Invalid);
            let id = target.ok_or(Refusal::Invalid)?;
            crate::postgres::labels::lock_item_labels(
                tx,
                repo,
                LabelTarget::Issue,
                issue.id,
                &[id],
            )
            .await?;
            let name: String =
                sqlx::query_scalar("SELECT name FROM labels WHERE id=$1 AND repo_id=$2")
                    .bind(id)
                    .bind(repo)
                    .fetch_one(&mut **tx)
                    .await?;
            let current: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM issue_labels WHERE issue_id=$1 AND label_id=$2)",
            )
            .bind(issue.id)
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
            (Value::String(name.clone()), name, current)
        }
        Action::AddField => {
            let id = target.ok_or(Refusal::Invalid)?;
            let a = crate::postgres::issue_attributes::catalog_connection(
                tx,
                repo,
                Some(actor),
                "field",
            )
            .await?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or(Refusal::Invalid)?;
            let canonical = a.canonical(&p.value)?;
            let old: Option<String> = sqlx::query_scalar(
                "SELECT value_json FROM issue_field_values WHERE issue_id=$1 AND attribute_id=$2",
            )
            .bind(issue.id)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
            let current = old
                .map(|v| serde_json::from_str::<Value>(&v))
                .transpose()?
                .as_ref()
                == Some(&canonical);
            (p.value.clone(), a.name, current)
        }
        Action::AddAssignee => {
            anyhow::ensure!(p.value.is_null() || p.value.is_string(), Refusal::Invalid);
            let id = target.ok_or(Refusal::Invalid)?;
            crate::postgres::assignees::candidate(tx, repo, id).await?;
            let name: String = sqlx::query_scalar("SELECT username FROM users WHERE id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
            let current: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM issue_assignees WHERE issue_id=$1 AND user_id=$2)",
            )
            .bind(issue.id)
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
            (Value::String(name.clone()), name, current)
        }
        Action::CloseIssue => {
            let reason = if p.value.is_null() || p.value == Value::Bool(true) {
                if target.is_some() {
                    "duplicate"
                } else {
                    "completed"
                }
            } else {
                p.value.as_str().ok_or(Refusal::Invalid)?
            };
            anyhow::ensure!(
                matches!(reason, "completed" | "not_planned" | "duplicate")
                    && (target.is_none() || reason == "duplicate"),
                Refusal::Invalid
            );
            let name = if let Some(id) = target {
                anyhow::ensure!(id != issue.id, Refusal::Invalid);
                let title:Option<String>=sqlx::query_scalar(&format!("SELECT i.title FROM issues i JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id=$1 WHERE i.id=$2 AND ({}) AND (COALESCE(u.username,'')<>$4 OR r.id=$3)",codeyard_database::queries::postgres::repositories::access::readable_condition())).bind(actor).bind(id).bind(repo).bind(codeyard_database::queries::ci_tokens::BOT).fetch_optional(&mut **tx).await?;
                title.ok_or(Refusal::Invalid)?
            } else {
                reason.into()
            };
            let duplicate: Option<i64> =
                sqlx::query_scalar("SELECT duplicate_id FROM issue_duplicates WHERE issue_id=$1")
                    .bind(issue.id)
                    .fetch_optional(&mut **tx)
                    .await?;
            let current = issue.state == "closed"
                && issue.state_reason.as_deref().unwrap_or("completed") == reason
                && duplicate == target;
            (Value::String(reason.into()), name, current)
        }
    };
    Ok(Prepared {
        value,
        name,
        current,
    })
}
async fn transition(
    tx: &mut Transaction<'_, Postgres>,
    s: &Suggestion,
    actor: Option<i64>,
    state: &str,
    event: Option<i64>,
) -> Result<Suggestion> {
    let updated = chrono::Utc::now().max(s.updated_at + chrono::Duration::microseconds(1));
    Ok(sqlx::query_as("UPDATE issue_suggestions SET state=$2,resolved_by=$3,issue_event_id=$4,updated_at=$5 WHERE id=$1 AND state='pending' RETURNING *").bind(s.id).bind(state).bind(actor).bind(event).bind(updated).fetch_optional(&mut **tx).await?.ok_or(Refusal::NotPending)?)
}
pub async fn propose(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
    issue: i64,
    p: &Proposal,
    target_repository: Option<i64>,
) -> Result<Suggestion> {
    p.validate()?;
    let mut tx = pool.begin().await?;
    crate::postgres::discussions::require_contribution_authority(&mut tx, actor, repo).await?;
    crate::postgres::issue_attributes::lock_metadata(&mut tx).await?;
    crate::postgres::discussions::lock_issue_contribution(&mut tx, actor, repo, issue).await?;
    anyhow::ensure!(
        codeyard_database::queries::postgres::users::transitions::credential_live(
            &mut tx, actor, credential, true
        )
        .await?,
        Refusal::Denied
    );
    issue_scope(&mut tx, repo, issue).await?;
    if let Some(expected) = target_repository {
        anyhow::ensure!(p.action == Action::CloseIssue, Refusal::Invalid);
        let current: Option<i64> =
            sqlx::query_scalar("SELECT repo_id FROM issues WHERE id=$1 FOR SHARE")
                .bind(p.target_id)
                .fetch_optional(&mut *tx)
                .await?;
        anyhow::ensure!(current == Some(expected), Refusal::Missing);
    }
    let record: Issue =
        sqlx::query_as("SELECT * FROM issues WHERE id=$1 AND repo_id=$2 FOR UPDATE")
            .bind(issue)
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    let prepared = prepare(&mut tx, repo, actor, &record, p).await?;
    let slot = p.action.slot(p.target_id);
    let previous:Vec<Suggestion>=sqlx::query_as("SELECT * FROM issue_suggestions WHERE issue_id=$1 AND actor_id=$2 AND slot=$3 AND state='pending' ORDER BY id FOR UPDATE").bind(issue).bind(actor).bind(&slot).fetch_all(&mut *tx).await?;
    for s in previous {
        transition(&mut tx, &s, None, "replaced", None).await?;
    }
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM issue_suggestions WHERE issue_id=$1 AND state='pending'",
    )
    .bind(issue)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(pending < 100 || prepared.current, Refusal::Full);
    let suggestion=sqlx::query_as("INSERT INTO issue_suggestions(issue_id,action,slot,target_id,target_name,value_json,rationale,confidence,actor_id,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *").bind(issue).bind(p.action.name()).bind(slot).bind(p.target_id).bind(prepared.name).bind(serde_json::to_string(&prepared.value)?).bind(&p.rationale).bind(&p.confidence).bind(actor).bind(if prepared.current{"applied"}else{"pending"}).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(suggestion)
}
fn invalid_target(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Refusal>()
        .is_some_and(|e| matches!(e, Refusal::Invalid | Refusal::Missing))
        || error
            .downcast_ref::<crate::issue_attributes::Refusal>()
            .is_some_and(|e| !matches!(e, crate::issue_attributes::Refusal::Denied))
        || error
            .downcast_ref::<crate::labels::LabelWriteRefusal>()
            .is_some_and(|e| matches!(e, crate::labels::LabelWriteRefusal::InvalidLabels))
        || error
            .downcast_ref::<crate::assignees::AssignmentRefusal>()
            .is_some_and(|e| !matches!(e, crate::assignees::AssignmentRefusal::Denied))
}
async fn effect(
    tx: &mut Transaction<'_, Postgres>,
    (repo, actor): (i64, i64),
    issue: &Issue,
    p: &Proposal,
    prepared: &Prepared,
) -> Result<(Option<i64>, Option<i64>, Option<Issue>)> {
    if prepared.current {
        return Ok((None, None, None));
    }
    let id = issue.id;
    match p.action {
        Action::SetType => Ok((
            crate::postgres::issue_attributes::type_in_transaction(
                tx,
                repo,
                actor,
                id,
                p.target_id,
            )
            .await?,
            None,
            None,
        )),
        Action::AddField => Ok((
            crate::postgres::issue_attributes::fields_in_transaction(
                tx,
                repo,
                actor,
                id,
                &[ValueInput {
                    field_id: p.target_id.ok_or(Refusal::Invalid)?,
                    value: p.value.clone(),
                }],
                false,
            )
            .await?,
            None,
            None,
        )),
        Action::AddLabel => {
            crate::postgres::labels::write_item_labels_transaction(
                tx,
                None,
                repo,
                LabelTarget::Issue,
                id,
                &[p.target_id.ok_or(Refusal::Invalid)?],
                false,
            )
            .await?;
            crate::postgres::issue_attributes::touch(tx, id).await?;
            let (name, color): (String, String) =
                sqlx::query_as("SELECT name,color FROM labels WHERE id=$1")
                    .bind(p.target_id)
                    .fetch_one(&mut **tx)
                    .await?;
            Ok((
                Some(
                    crate::postgres::issue_attributes::event(
                        tx,
                        (repo, id, actor),
                        "labeled",
                        &name,
                        &color,
                    )
                    .await?,
                ),
                None,
                None,
            ))
        }
        Action::AddAssignee => {
            let assignee = p.target_id.ok_or(Refusal::Invalid)?;
            let changed = crate::postgres::assignees::assign_in_transaction(
                tx,
                actor,
                repo,
                AssignedItem::Issue(id),
                assignee,
            )
            .await?;
            if !changed {
                return Ok((None, None, None));
            }
            crate::postgres::issue_attributes::touch(tx, id).await?;
            Ok((
                Some(
                    sqlx::query_scalar("SELECT id FROM timeline_events WHERE repo_id=$1 AND subject_kind='issue' AND subject_id=$2 AND actor_id=$3 AND kind='assigned' ORDER BY id DESC LIMIT 1")
                        .bind(repo).bind(id).bind(actor).fetch_one(&mut **tx).await?,
                ),
                Some(assignee),
                None,
            ))
        }
        Action::CloseIssue => {
            let closed = crate::postgres::issues::update_issue_in_transaction(
                tx,
                repo,
                id,
                crate::discussions::DiscussionEdit {
                    actor_id: actor,
                    expected_state: &issue.state,
                    expected_updated: issue.updated_at,
                    title: None,
                    body: None,
                    state: Some("closed"),
                },
                prepared.value.as_str(),
            )
            .await?
            .ok_or(Refusal::Invalid)?;
            // Apply the exact normalized target, clearing an obsolete duplicate link too.
            if let Some(duplicate) = p.target_id {
                sqlx::query("INSERT INTO issue_duplicates(issue_id,duplicate_id) VALUES($1,$2) ON CONFLICT(issue_id) DO UPDATE SET duplicate_id=excluded.duplicate_id")
                    .bind(id)
                    .bind(duplicate)
                    .execute(&mut **tx)
                    .await?;
            } else {
                sqlx::query("DELETE FROM issue_duplicates WHERE issue_id=$1")
                    .bind(id)
                    .execute(&mut **tx)
                    .await?;
            }
            let event = if issue.state == "closed" {
                // A reason/duplicate correction is a new action, never a historical close.
                crate::postgres::timeline::append(
                    tx,
                    repo,
                    "issue",
                    id,
                    actor,
                    "closed",
                    prepared.value.as_str().unwrap_or("completed"),
                    issue.state_reason.as_deref().unwrap_or("completed"),
                )
                .await?
            } else {
                sqlx::query_scalar("SELECT id FROM timeline_events WHERE repo_id=$1 AND subject_kind='issue' AND subject_id=$2 AND actor_id=$3 AND kind='closed' ORDER BY id DESC LIMIT 1").bind(repo).bind(id).bind(actor).fetch_one(&mut **tx).await?
            };
            Ok((Some(event), None, Some(closed)))
        }
    }
}
pub async fn resolve(
    pool: &PgPool,
    repo: i64,
    actor: i64,
    credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>,
    issue: i64,
    id: i64,
    approve: bool,
) -> Result<Resolution> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        codeyard_database::queries::postgres::discussions::lock_triage_authority(
            &mut tx, actor, repo
        )
        .await?,
        Refusal::Denied
    );
    anyhow::ensure!(
        codeyard_database::queries::postgres::users::transitions::credential_live(
            &mut tx, actor, credential, true
        )
        .await?,
        Refusal::Denied
    );
    crate::postgres::issue_attributes::lock_metadata(&mut tx).await?;
    issue_scope(&mut tx, repo, issue).await?;
    let record: Issue =
        sqlx::query_as("SELECT * FROM issues WHERE id=$1 AND repo_id=$2 FOR UPDATE")
            .bind(issue)
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    let s: Suggestion =
        sqlx::query_as("SELECT * FROM issue_suggestions WHERE id=$1 AND issue_id=$2 FOR UPDATE")
            .bind(id)
            .bind(issue)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
    if s.action == "add_field" {
        let fields = crate::postgres::issue_attributes::catalog_connection(
            &mut tx,
            repo,
            Some(actor),
            "field",
        )
        .await?;
        anyhow::ensure!(
            fields.iter().any(|f| Some(f.id) == s.target_id),
            Refusal::Missing
        );
    }
    if s.action == "close_issue" && s.target_id.is_some() {
        let visible:bool=sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM issues i JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id=$1 WHERE i.id=$2 AND ({}) AND (COALESCE(u.username,'')<>$4 OR r.id=$3))",codeyard_database::queries::postgres::repositories::access::readable_condition())).bind(actor).bind(s.target_id).bind(repo).bind(codeyard_database::queries::ci_tokens::BOT).fetch_one(&mut *tx).await?;
        anyhow::ensure!(visible, Refusal::Missing);
    }
    anyhow::ensure!(s.state == "pending", Refusal::NotPending);
    let mut event = None;
    let mut assignment = None;
    let mut closed_issue = None;
    let mut invalidated = false;
    if approve {
        if s.action == "add_field" {
            anyhow::ensure!(
                codeyard_database::queries::postgres::discussions::lock_write_authority(
                    &mut tx, actor, repo
                )
                .await?,
                Refusal::Denied
            );
        }
        let action = Action::parse(&s.action).ok_or(Refusal::Invalid)?;
        let p = Proposal {
            action,
            target_id: s.target_id,
            value: serde_json::from_str(&s.value_json)?,
            rationale: s.rationale.clone(),
            confidence: s.confidence.clone(),
        };
        sqlx::query("SAVEPOINT suggestion_apply")
            .execute(&mut *tx)
            .await?;
        let result = async {
            let prepared = prepare(&mut tx, repo, actor, &record, &p).await?;
            effect(&mut tx, (repo, actor), &record, &p, &prepared).await
        }
        .await;
        match result {
            Ok((created, assigned, closed)) => {
                event = created;
                assignment = assigned;
                closed_issue = closed;
            }
            Err(error) if invalid_target(&error) => {
                sqlx::query("ROLLBACK TO SAVEPOINT suggestion_apply")
                    .execute(&mut *tx)
                    .await?;
                invalidated = true;
            }
            Err(error) => return Err(error),
        }
        sqlx::query("RELEASE SAVEPOINT suggestion_apply")
            .execute(&mut *tx)
            .await?;
    }
    let suggestion = transition(
        &mut tx,
        &s,
        Some(actor),
        if invalidated {
            "invalidated"
        } else if approve {
            "approved"
        } else {
            "dismissed"
        },
        event,
    )
    .await?;
    if approve && !invalidated {
        let competing:Vec<Suggestion>=sqlx::query_as("SELECT * FROM issue_suggestions WHERE issue_id=$1 AND slot=$2 AND state='pending' ORDER BY id FOR UPDATE").bind(issue).bind(&s.slot).fetch_all(&mut *tx).await?;
        for other in competing {
            transition(&mut tx, &other, Some(actor), "dismissed", None).await?;
        }
    }
    tx.commit().await?;
    Ok(Resolution {
        issue: record,
        suggestion,
        assignment,
        closed_issue,
        invalidated,
    })
}
