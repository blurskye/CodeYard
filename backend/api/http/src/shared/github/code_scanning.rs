//! Code scanning accepts open scanners rather than depending on GitHub's CodeQL service.
use axum::response::IntoResponse;

use crate::shared::github::pagination::Pagination;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;
pub(crate) const SARIF_ROUTE: &str = "/api/v3/repos/:owner/:repo/code-scanning/sarifs";

pub const SARIF_ACTION_ROUTE: &str = "/api/v3/repos/:owner/:repo/code-scanning/analysis";

pub async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
    admin: bool,
) -> Result<(
    codeyard_db_core::models::Repository,
    codeyard_app::api_shared::github::auth::Authentication,
)> {
    let (repo, auth) = repository(state, headers, owner, name, write).await?;
    let auth = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let allowed = if admin {
        codeyard_identity::repository_access::can_admin(&state.pool, &auth.user, &repo).await
    } else if write {
        codeyard_identity::repository_access::can_write(&state.pool, &auth.user, &repo).await
    } else {
        codeyard_identity::repository_access::user_access(&state.pool, &auth.user, &repo)
            .await
            .map(|level| level.can_write())
    }
    .map_err(Error::unavailable)?;
    if !allowed {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Resource not accessible by token",
        ));
    }
    Ok((repo, auth))
}

pub fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<codeyard_db_security::code_scanning::Refusal>() {
        Some(codeyard_db_security::code_scanning::Refusal::Missing) => Error::missing(),
        Some(codeyard_db_security::code_scanning::Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Repository write denied")
        }
        Some(codeyard_db_security::code_scanning::Refusal::RateLimited) => {
            Error::new(StatusCode::FORBIDDEN, error.to_string())
        }
        Some(codeyard_db_security::code_scanning::Refusal::DeleteOrder) => {
            Error::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        None => Error::unavailable(error),
    }
}

#[derive(Deserialize)]
pub struct Filters {
    page: Option<usize>,
    per_page: Option<usize>,
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    tool_name: Option<String>,
    tool_guid: Option<String>,
    sarif_id: Option<String>,
    pr: Option<i32>,
    state: Option<String>,
    severity: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    confirm_delete: Option<String>,
}

pub fn normalize_ref(reference: &str) -> String {
    if reference.starts_with("refs/") {
        reference.to_owned()
    } else {
        format!("refs/heads/{reference}")
    }
}

impl Filters {
    pub fn pull_request(&self, resource: &str) -> Result<Option<i32>> {
        if self.pr.is_some_and(|number| number <= 0) {
            return Err(Error::validation(resource, "pr", "invalid"));
        }
        Ok(self.pr)
    }
    pub fn pagination(&self) -> Pagination {
        Pagination {
            page: self.page,
            per_page: self.per_page,
        }
    }
    pub fn analysis_filters(
        &self,
    ) -> Result<codeyard_db_security::code_scanning::AnalysisFilters<'_>> {
        if self.sort.as_deref().is_some_and(|sort| sort != "created") {
            return Err(Error::validation("Analysis", "sort", "invalid"));
        }
        if self
            .direction
            .as_deref()
            .is_some_and(|direction| !["asc", "desc"].contains(&direction))
        {
            return Err(Error::validation("Analysis", "direction", "invalid"));
        }
        Ok(codeyard_db_security::code_scanning::AnalysisFilters {
            reference: None,
            tool_name: self.tool_name.as_deref(),
            tool_guid: self.tool_guid.as_deref(),
            sarif_id: self.sarif_id.as_deref(),
            pull_request: self.pull_request("Analysis")?,
            ascending: self.direction.as_deref() == Some("asc"),
        })
    }
    pub fn alert_filters(&self) -> Result<codeyard_db_security::code_scanning::AlertFilters<'_>> {
        if self
            .state
            .as_deref()
            .is_some_and(|state| !["open", "closed", "fixed", "dismissed"].contains(&state))
        {
            return Err(Error::validation("Alert", "state", "invalid"));
        }
        if self.severity.as_deref().is_some_and(|severity| {
            ![
                "critical", "high", "medium", "low", "warning", "note", "error",
            ]
            .contains(&severity)
        }) {
            return Err(Error::validation("Alert", "severity", "invalid"));
        }
        if self
            .sort
            .as_deref()
            .is_some_and(|sort| !["created", "updated"].contains(&sort))
        {
            return Err(Error::validation("Alert", "sort", "invalid"));
        }
        if self
            .direction
            .as_deref()
            .is_some_and(|direction| !["asc", "desc"].contains(&direction))
        {
            return Err(Error::validation("Alert", "direction", "invalid"));
        }
        let sort = match (
            self.sort.as_deref() == Some("updated"),
            self.direction.as_deref() == Some("asc"),
        ) {
            (false, false) => codeyard_db_security::code_scanning::AlertSort::CreatedDesc,
            (false, true) => codeyard_db_security::code_scanning::AlertSort::CreatedAsc,
            (true, false) => codeyard_db_security::code_scanning::AlertSort::UpdatedDesc,
            (true, true) => codeyard_db_security::code_scanning::AlertSort::UpdatedAsc,
        };
        Ok(codeyard_db_security::code_scanning::AlertFilters {
            reference: None,
            tool_name: self.tool_name.as_deref(),
            tool_guid: self.tool_guid.as_deref(),
            state: self.state.as_deref(),
            severity: self.severity.as_deref(),
            pull_request: self.pull_request("Alert")?,
            sort,
        })
    }
}

fn analysis_document(
    row: &codeyard_db_security::code_scanning::Analysis,
    owner: &str,
    name: &str,
    deletable: bool,
) -> Result<Value> {
    Ok(
        json!({"id":row.id,"ref":row.ref_name,"commit_sha":row.commit_sha,"analysis_key":row.category,"environment":"{}","category":row.category,"error":"","warning":if row.results_count>5000 {"Only the highest priority 5000 results are included"} else {""},"created_at":row.created_at,"results_count":row.results_count,"rules_count":row.rules_count,"url":format!("{}/code-scanning/analyses/{}",repo_url(owner,name),row.id),"sarif_id":row.upload_id,"tool":serde_json::from_str::<Value>(&row.tool).map_err(Error::unavailable)?,"deletable":deletable}),
    )
}

pub async fn analyses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Filters>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = scope(&state, &headers, &owner, &name, false, false).await?;
    let (offset, limit) = query.pagination().window()?;
    let reference = query.ref_name.as_deref().map(normalize_ref);
    let mut filters = query.analysis_filters()?;
    filters.reference = reference.as_deref();
    let rows = codeyard_db_security::code_scanning::analyses(
        &state.pool,
        repo.id,
        &filters,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(failure)?;
    let next = rows.len() > limit;
    let values = rows
        .iter()
        .take(limit)
        .map(|entry| analysis_document(&entry.analysis, &owner, &name, entry.deletable))
        .collect::<Result<Vec<_>>>()?;
    query.pagination().response(&uri, values, next)
}

pub async fn analysis(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = scope(&state, &headers, &owner, &name, false, false).await?;
    let entry = codeyard_db_security::code_scanning::analysis(&state.pool, repo.id, id)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    let row = &entry.analysis;
    if headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/sarif+json"))
    {
        // An analysis keeps up to 100 MiB of SARIF. The shared download slots bound how many
        // are loaded and rewritten at once, the rewrite runs off the async workers, and the
        // slot is held until the client has the last byte.
        let permit = crate::middleware::wait_for_slot(&state.repository_downloads)
            .await
            .ok_or_else(|| Error::unavailable(()))?;
        let raw = codeyard_db_security::code_scanning::analysis_sarif(&state.pool, repo.id, id)
            .await
            .map_err(failure)?
            .ok_or_else(Error::missing)?;
        let alerts = codeyard_db_security::code_scanning::analysis_alerts(&state.pool, row.id)
            .await
            .map_err(failure)?;
        let alerts_url = format!("{}/code-scanning/alerts", repo_url(&owner, &name));
        let bytes = tokio::task::spawn_blocking(move || sarif_download(raw, alerts, &alerts_url))
            .await
            .map_err(Error::unavailable)?
            .map_err(Error::unavailable)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/sarif+json")],
            crate::middleware::buffered_download_body(bytes, permit),
        )
            .into_response());
    }
    Ok(Json(analysis_document(row, &owner, &name, entry.deletable)?).into_response())
}

/// The stored SARIF with only the results that became alerts, each tagged with its alert
/// number and URL as GitHub returns it.
fn sarif_download(
    raw: String,
    alerts: Vec<(i64, i64)>,
    alerts_url: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut sarif: Value = serde_json::from_str(&raw)?;
    drop(raw);
    let mut source = match sarif["runs"][0]["results"].take() {
        Value::Array(results) => results,
        _ => Vec::new(),
    };
    let mut results = Vec::with_capacity(alerts.len());
    for (index, number) in alerts {
        let mut result = usize::try_from(index)
            .ok()
            .and_then(|index| source.get_mut(index))
            .map(Value::take)
            .ok_or_else(|| anyhow::anyhow!("alert {number} names no stored result"))?;
        if !result["properties"].is_object() {
            result["properties"] = json!({});
        }
        result["properties"]["github/alertNumber"] = json!(number);
        result["properties"]["github/alertUrl"] = json!(format!("{alerts_url}/{number}"));
        results.push(result);
    }
    drop(source);
    sarif["runs"][0]["results"] = json!(results);
    Ok(serde_json::to_vec(&sarif)?)
}

pub async fn alert_repository(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<codeyard_db_core::models::Repository> {
    let (repo, _) = repository(state, headers, owner, name, false).await?;
    if repo.is_private {
        scope(state, headers, owner, name, false, false).await?;
    }
    Ok(repo)
}

pub async fn find_alert(
    state: &AppState,
    repo: i64,
    number: i64,
) -> Result<codeyard_db_security::code_scanning::Alert> {
    codeyard_db_security::code_scanning::alert(&state.pool, repo, number)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)
}

pub async fn alert_document(
    state: &AppState,
    row: &codeyard_db_security::code_scanning::Alert,
    owner: &str,
    name: &str,
    reference: Option<&str>,
    pull_request: Option<i32>,
) -> Result<Value> {
    let instance = codeyard_db_security::code_scanning::instance_filtered(
        &state.pool,
        row.id,
        reference,
        pull_request,
    )
    .await
    .map_err(failure)?
    .ok_or_else(Error::missing)?;
    let dismissed = match row.dismissed_by {
        Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .as_ref()
            .map(codeyard_app::webhooks::payloads::user),
        None => None,
    };
    codeyard_app::webhooks::payloads::code_scanning::code_scanning_alert(
        row,
        std::slice::from_ref(&instance),
        owner,
        name,
        dismissed,
    )
    .map_err(Error::unavailable)
}

pub async fn alerts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Filters>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let repo = alert_repository(&state, &headers, &owner, &name).await?;
    let (offset, limit) = query.pagination().window()?;
    let reference = query.ref_name.as_deref().map(normalize_ref);
    let mut filters = query.alert_filters()?;
    filters.reference = reference.as_deref();
    let rows = codeyard_db_security::code_scanning::alert_page(
        &state.pool,
        codeyard_db_security::code_scanning::AlertScope::Repository(repo.id),
        &filters,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(failure)?;
    let next = rows.len() > limit;
    let mut values = Vec::new();
    for row in rows.iter().take(limit) {
        values.push(
            listed_alert(
                &state,
                row,
                &repo,
                &owner,
                reference.as_deref(),
                filters.pull_request,
            )
            .await?,
        );
    }
    query.pagination().response(&uri, values, next)
}

async fn listed_alert(
    state: &AppState,
    row: &codeyard_db_security::code_scanning::Alert,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    reference: Option<&str>,
    pull_request: Option<i32>,
) -> Result<Value> {
    let default = pull_request
        .is_none()
        .then(|| format!("refs/heads/{}", repo.default_branch));
    alert_document(
        state,
        row,
        owner,
        &repo.name,
        reference.or(default.as_deref()),
        pull_request,
    )
    .await
}

pub async fn alert(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let repo = alert_repository(&state, &headers, &owner, &name).await?;
    let row = find_alert(&state, repo.id, number).await?;
    Ok(Json(
        alert_document(&state, &row, &owner, &name, None, None).await?,
    ))
}

pub mod databases;
