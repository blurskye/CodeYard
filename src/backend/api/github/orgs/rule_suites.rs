use super::{outcome, target};
use crate::backend::{
    api::github::{
        auth,
        pagination::Pagination,
        repository,
        rulesets::{error, page},
        Error, Result,
    },
    app::AppState,
    db::{
        organizations::OrganizationOutcome,
        rulesets::{self, Suite, SuiteEnforcement, SuiteFilter, SuiteResult},
    },
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    repository_name: Option<String>,
    actor_name: Option<String>,
    time_period: Option<String>,
    rule_suite_result: Option<String>,
    evaluate_status: Option<String>,
}
impl List {
    fn filter(self) -> Result<(Pagination, SuiteFilter)> {
        let duration = match self.time_period.as_deref().unwrap_or("day") {
            "hour" => Duration::hours(1),
            "day" => Duration::days(1),
            "week" => Duration::weeks(1),
            "month" => Duration::days(30),
            _ => return Err(Error::validation("RuleSuite", "time_period", "invalid")),
        };
        let result = match self.rule_suite_result.as_deref().unwrap_or("all") {
            "all" => None,
            "pass" => Some(SuiteResult::Pass),
            "fail" => Some(SuiteResult::Fail),
            "bypass" => Some(SuiteResult::Bypass),
            _ => {
                return Err(Error::validation(
                    "RuleSuite",
                    "rule_suite_result",
                    "invalid",
                ))
            }
        };
        let enforcement = match self.evaluate_status.as_deref().unwrap_or("all") {
            "all" => None,
            "active" => Some(SuiteEnforcement::Active),
            "evaluate" => Some(SuiteEnforcement::Evaluate),
            _ => return Err(Error::validation("RuleSuite", "evaluate_status", "invalid")),
        };
        if self.reference.as_ref().is_some_and(|r| {
            r.is_empty()
                || r.len() > 1024
                || r.contains(['*', '?', '[', ']'])
                || r.chars().any(char::is_control)
        }) {
            return Err(Error::validation("RuleSuite", "ref", "invalid"));
        }
        if self
            .repository_name
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 255)
            || self
                .actor_name
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 255)
        {
            return Err(Error::validation("RuleSuite", "filter", "invalid"));
        }
        Ok((
            Pagination {
                page: self.page,
                per_page: self.per_page,
            },
            SuiteFilter {
                since: Utc::now() - duration,
                reference: self.reference,
                repository_name: self.repository_name,
                actor_name: self.actor_name,
                result,
                enforcement,
            },
        ))
    }
}
fn document(suite: &Suite, detail: bool, metadata: bool) -> Result<Value> {
    let mut value = json!({"id":suite.id,"before_sha":suite.before_sha,"after_sha":suite.after_sha,
        "ref":suite.reference,"repository_id":suite.repo_id,"repository_name":suite.repository_name,
        "pushed_at":suite.created_at,"result":suite.result});
    // GitHub's list schema omits missing principals and evaluate results rather than using null.
    if detail || suite.actor_id.is_some() {
        value["actor_id"] = json!(suite.actor_id);
    }
    if detail || suite.actor_name.is_some() {
        value["actor_name"] = json!(suite.actor_name);
    }
    if detail || suite.evaluation_result.is_some() {
        value["evaluation_result"] = json!(suite.evaluation_result);
    }
    if detail {
        let mut evaluations =
            serde_json::to_value(&suite.evaluations).map_err(Error::unavailable)?;
        let entries = evaluations.as_array_mut().unwrap();
        if metadata {
            entries.truncate(50);
        } else {
            for entry in entries {
                entry.as_object_mut().unwrap().remove("metadata");
            }
        }
        value["rule_evaluations"] = evaluations;
    }
    Ok(value)
}
pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let (pagination, filter) = query.filter()?;
    let rows = outcome(
        rulesets::organization_rule_suites(
            &state.pool,
            actor.user.id,
            org.id,
            &filter,
            page(&pagination)?,
        )
        .await
        .map_err(error)?,
    )?;
    let (_, limit) = pagination.window()?;
    let has_next = rows.len() > limit;
    pagination.response(
        &uri,
        rows.iter()
            .map(|s| document(s, false, false))
            .collect::<Result<Vec<_>>>()?,
        has_next,
    )
}
#[derive(Deserialize)]
pub(super) struct Detail {
    #[serde(default)]
    include_metadata: bool,
}
pub(super) async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
    Query(query): Query<Detail>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = target(&state, &name).await?;
    let row = outcome(
        rulesets::organization_rule_suite(&state.pool, actor.user.id, org.id, id)
            .await
            .map_err(error)?,
    )?;
    Ok(Json(document(&row, true, query.include_metadata)?))
}
/// Repository rule suites are for repository administrators.
fn repository_outcome<T>(value: OrganizationOutcome<T>) -> Result<T> {
    match value {
        OrganizationOutcome::Denied => Err(Error::new(
            StatusCode::FORBIDDEN,
            "Must have admin rights to Repository.",
        )),
        other => outcome(other),
    }
}
pub(super) async fn list_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let (pagination, filter) = query.filter()?;
    let rows = repository_outcome(
        rulesets::repository_rule_suites(
            &state.pool,
            actor.user.id,
            repo.id,
            &filter,
            page(&pagination)?,
        )
        .await
        .map_err(error)?,
    )?;
    let (_, limit) = pagination.window()?;
    let has_next = rows.len() > limit;
    pagination.response(
        &uri,
        rows.iter()
            .map(|s| document(s, false, false))
            .collect::<Result<Vec<_>>>()?,
        has_next,
    )
}
pub(super) async fn get_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(query): Query<Detail>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let row = repository_outcome(
        rulesets::repository_rule_suite(&state.pool, actor.user.id, repo.id, id)
            .await
            .map_err(error)?,
    )?;
    Ok(Json(document(&row, true, query.include_metadata)?))
}
