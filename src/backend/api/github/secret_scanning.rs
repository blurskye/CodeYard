//! Secret alert adapters share the stored alerts and resolution rules with native forms.
mod patterns;
use super::{auth, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{self, secret_scanning as records},
    repositories::secret_scanning::jobs,
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![
        ("secret-scanning/list-alerts-for-repo", get(list)),
        ("secret-scanning/list-alerts-for-org", get(list_org)),
        ("secret-scanning/get-alert", get(get_alert)),
        ("secret-scanning/update-alert", axum::routing::patch(update)),
        ("secret-scanning/list-locations-for-alert", get(locations)),
        ("secret-scanning/get-scan-history", get(history)),
        (
            "secret-scanning/create-push-protection-bypass",
            axum::routing::post(create_bypass),
        ),
    ];
    handlers.extend(patterns::handlers());
    handlers
}

#[derive(Deserialize)]
struct CreateBypass {
    reason: String,
    placeholder_id: String,
}

async fn create_bypass(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<CreateBypass>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let bypass = records::bypass(
        &state.pool,
        actor.user.id,
        repo.id,
        &input.placeholder_id,
        Some(&input.reason),
    )
    .await
    .map_err(|refusal| {
        if matches!(
            refusal.downcast_ref::<records::Refusal>(),
            Some(records::Refusal::Invalid)
        ) {
            Error::validation("SecretScanningPushProtectionBypass", "reason", "invalid")
        } else {
            error(refusal)
        }
    })?;
    Ok(Json(
        serde_json::json!({"reason":bypass.reason,"expire_at":bypass.expires_at,"token_type":bypass.token_type}),
    ))
}

pub(super) fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<records::Refusal>() {
        Some(records::Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token")
        }
        Some(records::Refusal::Missing) => Error::missing(),
        Some(records::Refusal::Version) => Error::new(
            StatusCode::PRECONDITION_FAILED,
            "Custom pattern version changed",
        ),
        Some(records::Refusal::Conflict) => Error::new(
            StatusCode::CONFLICT,
            "Pattern configuration version changed",
        ),
        Some(records::Refusal::Invalid) => {
            Error::validation("SecretScanningAlert", "state", "invalid")
        }
        None => Error::unavailable(error),
    }
}

#[derive(Deserialize)]
struct Filters {
    state: Option<String>,
    secret_type: Option<String>,
    exclude_secret_types: Option<String>,
    resolution: Option<String>,
    validity: Option<String>,
    assignee: Option<String>,
    is_bypassed: Option<bool>,
    direction: Option<String>,
    sort: Option<String>,
    #[serde(default)]
    hide_secret: bool,
    page: Option<usize>,
    per_page: Option<usize>,
}
impl Filters {
    fn validate(&self) -> Result<()> {
        if self
            .state
            .as_deref()
            .is_some_and(|state| !matches!(state, "open" | "resolved"))
            || self
                .sort
                .as_deref()
                .is_some_and(|sort| !matches!(sort, "created" | "updated"))
            || self
                .direction
                .as_deref()
                .is_some_and(|direction| !matches!(direction, "asc" | "desc"))
            || self.validity.as_deref().is_some_and(|values| {
                values
                    .split(',')
                    .any(|value| !matches!(value, "active" | "inactive" | "unknown"))
            })
            || self.resolution.as_deref().is_some_and(|values| {
                values.split(',').any(|value| {
                    !matches!(
                        value,
                        "false_positive" | "wont_fix" | "revoked" | "used_in_tests" | "pattern_deleted" | "pattern_edited"
                    )
                })
            })
            || self.secret_type.is_some() && self.exclude_secret_types.is_some()
        {
            return Err(Error::validation(
                "SecretScanningAlert",
                "filter",
                "invalid",
            ));
        }
        Ok(())
    }
    fn filter(&self, offset: usize, limit: usize) -> records::Filter {
        records::Filter {
            state: self.state.clone(),
            secret_type: self.secret_type.clone(),
            exclude_secret_types: self.exclude_secret_types.clone(),
            resolution: self.resolution.clone(),
            validity: self.validity.clone(),
            assignee: self.assignee.clone(),
            is_bypassed: self.is_bypassed,
            ascending: self.direction.as_deref() == Some("asc"),
            updated: self.sort.as_deref() == Some("updated"),
            offset: offset as i64,
            limit: limit as i64 + 1,
            ..Default::default()
        }
    }
}

async fn list_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<Filters>,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = super::orgs::target(&state, &org).await?;
    query.validate()?;
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = pagination.window()?;
    let alerts = records::org_alerts(
        &state.pool,
        actor.user.id,
        org.id,
        &query.filter(offset, limit),
    )
    .await
    .map_err(error)?;
    let has_next = alerts.len() > limit;
    let mut items = Vec::new();
    for alert in alerts.into_iter().take(limit) {
        let repo = db::repositories::records::get_repository_by_id(&state.pool, alert.repo_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        if repo.owner_org_id != Some(org.id) {
            continue;
        }
        let mut document = jobs::document(&state, &alert, &org.name, &repo.name, query.hide_secret)
            .await
            .map_err(Error::unavailable)?;
        document["repository"] = payloads::repository(&repo, &org.name);
        items.push(document);
    }
    pagination.response(&uri, items, has_next)
}

fn number(params: &HashMap<String, String>) -> Result<i64> {
    params
        .get("alert_number")
        .and_then(|number| number.parse().ok())
        .filter(|number| *number > 0)
        .ok_or_else(Error::missing)
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<Filters>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor.ok_or_else(auth::required)?;
    query.validate()?;
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = pagination.window()?;
    let alerts = records::alerts(
        &state.pool,
        actor.user.id,
        repo.id,
        &query.filter(offset, limit),
    )
    .await
    .map_err(error)?;
    let has_next = alerts.len() > limit;
    let mut items = Vec::new();
    for alert in alerts.into_iter().take(limit) {
        items.push(
            jobs::document(&state, &alert, &owner, &name, query.hide_secret)
                .await
                .map_err(Error::unavailable)?,
        );
    }
    pagination.response(&uri, items, has_next)
}

async fn get_alert(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<Filters>,
) -> Result<Json<Value>> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, actor) = repository(&state, &headers, owner, name, false).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let alert = records::alerts(
        &state.pool,
        actor.user.id,
        repo.id,
        &records::Filter {
            number: Some(number(&params)?),
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .map_err(error)?
    .into_iter()
    .next()
    .ok_or_else(Error::missing)?;
    Ok(Json(
        jobs::document(&state, &alert, owner, name, query.hide_secret)
            .await
            .map_err(Error::unavailable)?,
    ))
}

#[derive(Deserialize)]
struct Update {
    state: Option<String>,
    resolution: Option<String>,
    resolution_comment: Option<String>,
    #[serde(flatten)]
    fields: HashMap<String, serde_json::Value>,
}
fn nullable<'a>(
    fields: &'a HashMap<String, serde_json::Value>,
    name: &'static str,
) -> Result<Option<Option<&'a str>>> {
    match fields.get(name) {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::String(value)) => Ok(Some(Some(value))),
        _ => Err(Error::validation("SecretScanningAlert", name, "invalid")),
    }
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Json(input): Json<Update>,
) -> Result<Json<Value>> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, actor) = repository(&state, &headers, owner, name, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let alert = records::update_alert(
        &state.pool,
        actor.user.id,
        repo.id,
        number(&params)?,
        &records::AlertUpdate {
            state: input.state.as_deref(),
            reason: input.resolution.as_deref(),
            comment: input.resolution_comment.as_deref(),
            assignee: nullable(&input.fields, "assignee")?,
            validity: nullable(&input.fields, "validity")?,
        },
    )
    .await
    .map_err(error)?;
    jobs::emit_alert(
        &state,
        &repo,
        &alert,
        match input.state.as_deref() {
            Some("open") => "reopened",
            Some(_) => "resolved",
            None if input.fields.contains_key("assignee") => {
                if input.fields["assignee"].is_null() {
                    "unassigned"
                } else {
                    "assigned"
                }
            }
            None => "validated",
        },
        actor.user.id,
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(Json(
        jobs::document(&state, &alert, owner, name, false)
            .await
            .map_err(Error::unavailable)?,
    ))
}

async fn locations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (owner, name) = (&params["owner"], &params["repo"]);
    let (repo, actor) = repository(&state, &headers, owner, name, false).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let (offset, limit) = page.window()?;
    let locations = records::locations(
        &state.pool,
        actor.user.id,
        repo.id,
        number(&params)?,
        offset as i64,
        limit as i64 + 1,
    )
    .await
    .map_err(error)?;
    let has_next = locations.len() > limit;
    page.response(
        &uri,
        locations
            .into_iter()
            .map(|location| payloads::secret_scanning_location(&location, owner, name))
            .collect(),
        has_next,
    )
}

async fn history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let history = records::history(&state.pool, actor.user.id, repo.id)
        .await
        .map_err(error)?;
    let scans = |kind| {
        history.iter().filter(|job| job.scan_type == kind).map(|job| {
            let mut scan=serde_json::json!({"type": job.scan_type,"status":if job.status=="queued" {"pending"} else {&job.status},"started_at":job.started_at,"completed_at":job.completed_at});
            if let Some(name)=&job.pattern_name {scan["pattern_name"]=serde_json::json!(name);scan["pattern_scope"]=serde_json::json!(job.pattern_scope);}
            scan
        }).collect::<Vec<_>>()
    };
    Ok(Json(
        serde_json::json!({"incremental_scans": scans("incremental"),"backfill_scans": scans("backfill"),"pattern_update_scans":scans("pattern_update"),"custom_pattern_backfill_scans":scans("custom_pattern_backfill"),"generic_secrets_backfill_scans":[]}),
    ))
}
