//! Code scanning accepts open scanners rather than depending on GitHub's CodeQL service.
use super::{auth, pagination::Pagination, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    code_scanning,
    db::{self, code_scanning as records},
    repositories::{access, store::repo_store},
    webhooks::payloads,
};
use axum::{
    extract::{DefaultBodyLimit, FromRequest, OriginalUri, Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) mod databases;

pub(crate) const SARIF_ROUTE: &str = "/api/v3/repos/:owner/:repo/code-scanning/sarifs";
pub(crate) const SARIF_ACTION_ROUTE: &str = "/api/v3/repos/:owner/:repo/code-scanning/analysis";
const BODY_LIMIT: usize = code_scanning::MAX_COMPRESSED.div_ceil(3) * 4 + 64 * 1024;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "code-scanning/upload-sarif",
            post(upload).layer(DefaultBodyLimit::max(BODY_LIMIT)),
        ),
        ("code-scanning/list-codeql-databases", get(databases::list)),
        ("code-scanning/get-codeql-database", get(databases::get)),
        (
            "code-scanning/delete-codeql-database",
            delete(databases::delete),
        ),
        ("code-scanning/get-sarif", get(upload_status)),
        ("code-scanning/list-recent-analyses", get(analyses)),
        ("code-scanning/get-analysis", get(analysis)),
        ("code-scanning/delete-analysis", delete(delete_analysis)),
        ("code-scanning/list-alerts-for-repo", get(alerts)),
        ("code-scanning/list-alerts-for-org", get(org_alerts)),
        ("code-scanning/get-alert", get(alert)),
        ("code-scanning/update-alert", patch(update_alert)),
        ("code-scanning/list-alert-instances", get(instances)),
        ("code-scanning/get-default-setup", get(default_setup)),
        (
            "code-scanning/update-default-setup",
            patch(update_default_setup),
        ),
        ("code-scanning/get-autofix", get(absent)),
        ("code-scanning/create-autofix", post(absent)),
        ("code-scanning/commit-autofix", post(absent)),
    ]
}

// The open-source upload-sarif action uses this private GitHub spelling and commit_oid.
// Both transports persist exactly the same upload and expose the documented polling response.
pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            SARIF_ACTION_ROUTE.trim_start_matches("/api/v3"),
            put(upload).layer(DefaultBodyLimit::max(BODY_LIMIT)),
        )
        .route(
            databases::API_UPLOAD_ROUTE.trim_start_matches("/api/v3"),
            post(databases::upload).layer(DefaultBodyLimit::disable()),
        )
}

async fn absent() -> Error {
    Error::missing()
}

async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
    admin: bool,
) -> Result<(db::models::Repository, auth::Authentication)> {
    let (repo, auth) = repository(state, headers, owner, name, write).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let allowed = if admin {
        access::can_admin(&state.pool, &auth.user, &repo).await
    } else if write {
        access::can_write(&state.pool, &auth.user, &repo).await
    } else {
        access::user_access(&state.pool, &auth.user, &repo)
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

fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<records::Refusal>() {
        Some(records::Refusal::Missing) => Error::missing(),
        Some(records::Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Repository write denied")
        }
        Some(records::Refusal::RateLimited) => Error::new(StatusCode::FORBIDDEN, error.to_string()),
        Some(records::Refusal::DeleteOrder) => {
            Error::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        None => Error::unavailable(error),
    }
}

#[derive(Deserialize)]
struct UploadInput {
    #[serde(alias = "commit_oid")]
    commit_sha: String,
    #[serde(rename = "ref")]
    ref_name: String,
    sarif: String,
    checkout_uri: Option<String>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    tool_name: Option<String>,
    validate: Option<bool>,
}
async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    request: Request,
) -> Result<Response> {
    let (repo, auth) = scope(&state, &headers, &owner, &name, true, false).await?;
    let _permit = state
        .upload_reads
        .clone()
        .try_acquire_owned()
        .map_err(Error::unavailable)?;
    let Json(input) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Json::<UploadInput>::from_request(request, &state),
    )
    .await
    .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "Upload timed out"))?
    .map_err(|e| Error::new(e.status(), "Invalid SARIF request"))?;
    if !matches!(input.commit_sha.len(), 40 | 64)
        || !input.commit_sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error::validation("Sarif", "commit_sha", "invalid"));
    }
    if input.ref_name.len() > code_scanning::MAX_REFERENCE_BYTES
        || !["refs/heads/", "refs/tags/", "refs/pull/"]
            .iter()
            .any(|p| input.ref_name.starts_with(p))
        || input.ref_name.ends_with('/')
        || !git2::Reference::is_valid_name(&input.ref_name)
    {
        return Err(Error::validation("Sarif", "ref", "invalid"));
    }
    if input.sarif.len() > code_scanning::MAX_COMPRESSED.div_ceil(3) * 4
        || STANDARD.decode(&input.sarif).is_err()
    {
        return Err(Error::validation("Sarif", "sarif", "invalid"));
    }
    if input
        .checkout_uri
        .as_deref()
        .is_some_and(|uri| url::Url::parse(uri).is_err())
    {
        return Err(Error::validation("Sarif", "checkout_uri", "invalid"));
    }
    // These hints do not override the tool metadata and automation ID in the SARIF runs.
    let _ = (input.started_at, input.tool_name, input.validate);
    let commit = repo_store(&repo, state.cluster.as_ref())
        .commit_from_primary(&input.commit_sha)
        .await
        .map_err(|error| match error.kind {
            crate::backend::repositories::store::ErrorKind::NotFound
            | crate::backend::repositories::store::ErrorKind::MissingPath
            | crate::backend::repositories::store::ErrorKind::Invalid => {
                Error::validation("Sarif", "commit_sha", "invalid")
            }
            _ => super::contents::store_error(error),
        })?;
    let upload = records::enqueue(
        &state.pool,
        &records::NewUpload {
            actor: auth.database_actor(),
            repo: repo.id,
            ref_name: &input.ref_name,
            commit_sha: &commit.sha,
            sarif: &input.sarif,
            checkout_uri: input.checkout_uri.as_deref(),
        },
    )
    .await
    .map_err(failure)?;
    Ok((StatusCode::ACCEPTED,Json(json!({"id":upload.id,"url":format!("{}/code-scanning/sarifs/{}",repo_url(&owner,&name),upload.id)}))).into_response())
}

async fn upload_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = scope(&state, &headers, &owner, &name, false, false).await?;
    let upload = records::upload_status(&state.pool, repo.id, &id)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    Ok(Json(
        json!({"processing_status":upload.processing_status,"analyses_url":(upload.processing_status=="complete").then(||format!("{}/code-scanning/analyses?sarif_id={id}",repo_url(&owner,&name))),"errors":upload.error.map(|e|vec![e])}),
    ))
}

#[derive(Deserialize)]
struct Filters {
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
fn normalize_ref(reference: &str) -> String {
    if reference.starts_with("refs/") {
        reference.to_owned()
    } else {
        format!("refs/heads/{reference}")
    }
}
impl Filters {
    fn pull_request(&self, resource: &str) -> Result<Option<i32>> {
        if self.pr.is_some_and(|number| number <= 0) {
            return Err(Error::validation(resource, "pr", "invalid"));
        }
        Ok(self.pr)
    }
    fn pagination(&self) -> Pagination {
        Pagination {
            page: self.page,
            per_page: self.per_page,
        }
    }
    fn analysis_filters(&self) -> Result<records::AnalysisFilters<'_>> {
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
        Ok(records::AnalysisFilters {
            reference: None,
            tool_name: self.tool_name.as_deref(),
            tool_guid: self.tool_guid.as_deref(),
            sarif_id: self.sarif_id.as_deref(),
            pull_request: self.pull_request("Analysis")?,
            ascending: self.direction.as_deref() == Some("asc"),
        })
    }
    fn alert_filters(&self) -> Result<records::AlertFilters<'_>> {
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
            (false, false) => records::AlertSort::CreatedDesc,
            (false, true) => records::AlertSort::CreatedAsc,
            (true, false) => records::AlertSort::UpdatedDesc,
            (true, true) => records::AlertSort::UpdatedAsc,
        };
        Ok(records::AlertFilters {
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
    row: &records::Analysis,
    owner: &str,
    name: &str,
    deletable: bool,
) -> Result<Value> {
    Ok(
        json!({"id":row.id,"ref":row.ref_name,"commit_sha":row.commit_sha,"analysis_key":row.category,"environment":"{}","category":row.category,"error":"","warning":if row.results_count>5000 {"Only the highest priority 5000 results are included"} else {""},"created_at":row.created_at,"results_count":row.results_count,"rules_count":row.rules_count,"url":format!("{}/code-scanning/analyses/{}",repo_url(owner,name),row.id),"sarif_id":row.upload_id,"tool":serde_json::from_str::<Value>(&row.tool).map_err(Error::unavailable)?,"deletable":deletable}),
    )
}
async fn analyses(
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
    let rows = records::analyses(
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
async fn analysis(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let (repo, _) = scope(&state, &headers, &owner, &name, false, false).await?;
    let entry = records::analysis(&state.pool, repo.id, id)
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
        let permit =
            crate::backend::api::http::middleware::wait_for_slot(&state.repository_downloads)
                .await
                .ok_or_else(|| Error::unavailable(()))?;
        let raw = records::analysis_sarif(&state.pool, repo.id, id)
            .await
            .map_err(failure)?
            .ok_or_else(Error::missing)?;
        let alerts = records::analysis_alerts(&state.pool, row.id)
            .await
            .map_err(failure)?;
        let alerts_url = format!("{}/code-scanning/alerts", repo_url(&owner, &name));
        let bytes = tokio::task::spawn_blocking(move || sarif_download(raw, alerts, &alerts_url))
            .await
            .map_err(Error::unavailable)?
            .map_err(Error::unavailable)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/sarif+json")],
            crate::backend::api::http::middleware::buffered_download_body(bytes, permit),
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
async fn delete_analysis(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(query): Query<Filters>,
) -> Result<Json<Value>> {
    let (repo, auth) = scope(&state, &headers, &owner, &name, true, true).await?;
    let next = records::delete_analysis(
        &state.pool,
        auth.database_actor(),
        repo.id,
        id,
        query.confirm_delete.as_deref() == Some("true"),
    )
    .await
    .map_err(failure)?;
    let url = next.map(|id| format!("{}/code-scanning/analyses/{id}", repo_url(&owner, &name)));
    Ok(Json(
        json!({"next_analysis_url":url,"confirm_delete_url":url.map(|url|format!("{url}?confirm_delete=true"))}),
    ))
}
async fn alert_repository(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<db::models::Repository> {
    let (repo, _) = repository(state, headers, owner, name, false).await?;
    if repo.is_private {
        scope(state, headers, owner, name, false, false).await?;
    }
    Ok(repo)
}

async fn find_alert(state: &AppState, repo: i64, number: i64) -> Result<records::Alert> {
    records::alert(&state.pool, repo, number)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)
}
async fn alert_document(
    state: &AppState,
    row: &records::Alert,
    owner: &str,
    name: &str,
    reference: Option<&str>,
    pull_request: Option<i32>,
) -> Result<Value> {
    let instance = records::instance_filtered(&state.pool, row.id, reference, pull_request)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    let dismissed = match row.dismissed_by {
        Some(id) => db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .as_ref()
            .map(payloads::user),
        None => None,
    };
    payloads::code_scanning_alert(row, std::slice::from_ref(&instance), owner, name, dismissed)
        .map_err(Error::unavailable)
}
async fn alerts(
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
    let rows = records::alert_page(
        &state.pool,
        records::AlertScope::Repository(repo.id),
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
    row: &records::Alert,
    repo: &db::models::Repository,
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
async fn org_alerts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<Filters>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let auth = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let org = super::orgs::target(&state, &name).await?;
    let (offset, limit) = query.pagination().window()?;
    let reference = query.ref_name.as_deref().map(normalize_ref);
    let mut filters = query.alert_filters()?;
    filters.reference = reference.as_deref();
    let rows = records::organization_alert_page(
        &state.pool,
        auth.user.id,
        org.id,
        &filters,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(failure)?;
    let next = rows.len() > limit;
    let mut values = Vec::new();
    for row in rows.iter().take(limit) {
        let repo = &row.repository;
        let mut alert = payloads::code_scanning_alert(
            &row.alert,
            std::slice::from_ref(&row.instance),
            &name,
            &repo.name,
            row.dismissed_by.as_ref().map(payloads::user),
        )
        .map_err(Error::unavailable)?;
        alert["repository"] = payloads::repository(repo, &name);
        values.push(alert);
    }
    query.pagination().response(&uri, values, next)
}
async fn alert(
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
#[derive(Deserialize)]
struct UpdateAlert {
    state: String,
    dismissed_reason: Option<String>,
    dismissed_comment: Option<String>,
}
async fn update_alert(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Json(input): Json<UpdateAlert>,
) -> Result<Json<Value>> {
    let (repo, auth) = scope(&state, &headers, &owner, &name, true, false).await?;
    let input = records::Dismissal {
        state: &input.state,
        reason: input.dismissed_reason.as_deref(),
        comment: input.dismissed_comment.as_deref(),
    };
    input
        .validate()
        .map_err(|e| Error::invalid(e.to_string()))?;
    let row = code_scanning::dismiss(&state.pool, auth.database_actor(), repo.id, number, &input)
        .await
        .map_err(failure)?;
    Ok(Json(
        alert_document(&state, &row, &owner, &name, None, None).await?,
    ))
}
async fn instances(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<Filters>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let repo = alert_repository(&state, &headers, &owner, &name).await?;
    let row = find_alert(&state, repo.id, number).await?;
    let (offset, limit) = query.pagination().window()?;
    let reference = query.ref_name.as_deref().map(normalize_ref);
    let rows = records::instances_filtered(
        &state.pool,
        row.id,
        reference.as_deref(),
        query.pull_request("AlertInstance")?,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(failure)?;
    let next = rows.len() > limit;
    let values = rows
        .iter()
        .take(limit)
        .map(|instance| {
            payloads::code_scanning_instance(instance, &owner, &name, number)
                .map_err(Error::unavailable)
        })
        .collect::<Result<Vec<_>>>()?;
    query.pagination().response(&uri, values, next)
}
async fn default_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    scope(&state, &headers, &owner, &name, false, true).await?;
    Ok(Json(
        json!({"state":"not-configured","languages":[],"query_suite":"default","threat_model":"remote","updated_at":null,"schedule":null,"runner_type":null,"runner_label":null}),
    ))
}
async fn update_default_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Value>,
) -> Result<Json<Value>> {
    scope(&state, &headers, &owner, &name, true, true).await?;
    if input["state"] != "not-configured" {
        return Err(Error::invalid(
            "Default setup requires GitHub's CodeQL service; upload SARIF from a workflow instead",
        ));
    }
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests;
