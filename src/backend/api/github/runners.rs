//! GitHub runner administration projects Codeyard's scoped cluster workers.
use super::{auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    cluster::JoinTicket,
    db::{
        self,
        runners::{
            self as native, Group, GroupInput, Labels, Refusal, Registration, Runner, Scope,
            Selection,
        },
    },
};
use axum::{
    extract::{rejection::JsonRejection, OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "actions/list-self-hosted-runner-groups-for-org",
            get(groups),
        ),
        (
            "actions/create-self-hosted-runner-group-for-org",
            post(create_group),
        ),
        (
            "actions/get-self-hosted-runner-group-for-org",
            get(get_group),
        ),
        (
            "actions/update-self-hosted-runner-group-for-org",
            patch(update_group),
        ),
        (
            "actions/delete-self-hosted-runner-group-from-org",
            delete(delete_group),
        ),
        (
            "actions/list-github-hosted-runners-in-group-for-org",
            get(hosted),
        ),
        (
            "actions/list-repo-access-to-self-hosted-runner-group-in-org",
            get(repositories),
        ),
        (
            "actions/set-repo-access-to-self-hosted-runner-group-in-org",
            put(set_repositories),
        ),
        (
            "actions/add-repo-access-to-self-hosted-runner-group-in-org",
            put(add_repository),
        ),
        (
            "actions/remove-repo-access-to-self-hosted-runner-group-in-org",
            delete(remove_repository),
        ),
        (
            "actions/list-self-hosted-runners-in-group-for-org",
            get(list),
        ),
        (
            "actions/set-self-hosted-runners-in-group-for-org",
            put(set_runners),
        ),
        (
            "actions/add-self-hosted-runner-to-group-for-org",
            put(add_runner),
        ),
        (
            "actions/remove-self-hosted-runner-from-group-for-org",
            delete(remove_runner_from_group),
        ),
        ("actions/list-self-hosted-runners-for-org", get(list)),
        (
            "actions/get-runner-version-deprecation-for-org",
            get(deprecation),
        ),
        ("actions/list-runner-applications-for-org", get(downloads)),
        ("actions/generate-runner-jitconfig-for-org", post(jit)),
        (
            "actions/create-registration-token-for-org",
            post(registration_token),
        ),
        ("actions/create-remove-token-for-org", post(remove_token)),
        ("actions/get-self-hosted-runner-for-org", get(get_runner)),
        (
            "actions/delete-self-hosted-runner-from-org",
            delete(delete_runner),
        ),
        (
            "actions/list-labels-for-self-hosted-runner-for-org",
            get(labels),
        ),
        (
            "actions/add-custom-labels-to-self-hosted-runner-for-org",
            post(add_labels),
        ),
        (
            "actions/set-custom-labels-for-self-hosted-runner-for-org",
            put(set_labels),
        ),
        (
            "actions/remove-all-custom-labels-from-self-hosted-runner-for-org",
            delete(clear_labels),
        ),
        (
            "actions/remove-custom-label-from-self-hosted-runner-for-org",
            delete(remove_label),
        ),
        ("actions/list-self-hosted-runners-for-repo", get(list)),
        (
            "actions/get-runner-version-deprecation-for-repo",
            get(deprecation),
        ),
        ("actions/list-runner-applications-for-repo", get(downloads)),
        ("actions/generate-runner-jitconfig-for-repo", post(jit)),
        (
            "actions/create-registration-token-for-repo",
            post(registration_token),
        ),
        ("actions/create-remove-token-for-repo", post(remove_token)),
        ("actions/get-self-hosted-runner-for-repo", get(get_runner)),
        (
            "actions/delete-self-hosted-runner-from-repo",
            delete(delete_runner),
        ),
        (
            "actions/list-labels-for-self-hosted-runner-for-repo",
            get(labels),
        ),
        (
            "actions/add-custom-labels-to-self-hosted-runner-for-repo",
            post(add_labels),
        ),
        (
            "actions/set-custom-labels-for-self-hosted-runner-for-repo",
            put(set_labels),
        ),
        (
            "actions/remove-all-custom-labels-from-self-hosted-runner-for-repo",
            delete(clear_labels),
        ),
        (
            "actions/remove-custom-label-from-self-hosted-runner-for-repo",
            delete(remove_label),
        ),
    ]
}

type Params = HashMap<String, String>;
fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token")
        }
        Some(Refusal::Invalid(field)) => Error::validation("Runner", field, "invalid"),
        Some(Refusal::Conflict(message)) => Error::new(StatusCode::CONFLICT, *message),
        None => Error::unavailable(error),
    }
}
// These runner endpoints specify GitHub's string-array validation errors.
fn runner_error(cause: anyhow::Error) -> Error {
    match cause.downcast_ref::<Refusal>() {
        Some(Refusal::Invalid(field)) => Error::invalid_simple(format!("Invalid runner {field}")),
        _ => error(cause),
    }
}
fn id(params: &Params, key: &str) -> Result<i64> {
    params
        .get(key)
        .and_then(|s| s.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)
}
async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    p: &Params,
    write: bool,
) -> Result<(i64, Scope)> {
    let auth = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    if write {
        auth.require_write()?;
    }
    let scope = if let Some(org) = p.get("org") {
        let org = db::organizations::get_public_organization(&state.pool, org)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        Scope::Organization(org.id)
    } else {
        let owner = p.get("owner").ok_or_else(Error::missing)?;
        let repo = p.get("repo").ok_or_else(Error::missing)?;
        Scope::Repository(
            super::repository(state, headers, owner, repo, write)
                .await?
                .0
                .id,
        )
    };
    Ok((auth.user.id, scope))
}
fn org(scope: Scope) -> Result<i64> {
    match scope {
        Scope::Organization(org) => Ok(org),
        _ => Err(Error::missing()),
    }
}
fn group_document(name: &str, g: &Group) -> Result<Value> {
    let base = format!(
        "/orgs/{}/actions/runner-groups/{}",
        crate::webui::encode_component(name),
        g.id
    );
    let workflows: Value =
        serde_json::from_str(&g.selected_workflows).map_err(Error::unavailable)?;
    let mut value = json!({"id":g.id,"name":g.name,"visibility":g.visibility,"default":g.is_default,"runners_url":super::api_url(&format!("{base}/runners")),"hosted_runners_url":super::api_url(&format!("{base}/hosted-runners")),"inherited":false,"allows_public_repositories":g.allows_public_repositories,"workflow_restrictions_read_only":false,"restricted_to_workflows":g.restricted_to_workflows,"selected_workflows":workflows});
    if g.visibility == "selected" {
        value["selected_repositories_url"] = json!(super::api_url(&format!("{base}/repositories")));
    }
    Ok(value)
}
fn label_documents(row: &Runner) -> std::result::Result<Vec<Value>, serde_json::Error> {
    let labels: Vec<String> = serde_json::from_str(&row.labels)?;
    let mut output = vec![json!({"name":"self-hosted","type":"read-only"})];
    output.extend(
        labels
            .into_iter()
            .map(|name| json!({"name":name,"type":"custom"})),
    );
    Ok(output)
}
fn document(row: &Runner) -> Result<Value> {
    let mut value = json!({"id":row.id,"name":row.name,"os":row.os,"status":if row.online {"online"} else {"offline"},"busy":row.busy,"labels":label_documents(row).map_err(Error::unavailable)?,"ephemeral":row.ephemeral,"version":row.version});
    if let Some(group) = row.group_id {
        value["runner_group_id"] = json!(group);
    }
    Ok(value)
}
async fn selected_group(state: &AppState, actor: i64, scope: Scope, p: &Params) -> Result<Group> {
    native::list_groups(&state.pool, actor, org(scope)?)
        .await
        .map_err(error)?
        .into_iter()
        .find(|g| g.id == id(p, "runner_group_id").unwrap_or(0))
        .ok_or_else(Error::missing)
}
async fn selected_runner(state: &AppState, actor: i64, scope: Scope, p: &Params) -> Result<Runner> {
    native::list_runners_page(&state.pool, actor, scope, &native::RunnerFilter { id: Some(id(p, "runner_id")?), limit: 1, ..Default::default() })
        .await
        .map_err(error)?
        .1.into_iter()
        .next()
        .ok_or_else(Error::missing)
}
#[derive(Deserialize)]
struct Listing {
    page: Option<usize>,
    per_page: Option<usize>,
    name: Option<String>,
    visible_to_repository: Option<String>,
}
impl Listing {
    fn pagination(&self) -> Pagination {
        Pagination {
            page: self.page,
            per_page: self.per_page,
        }
    }
}

async fn groups(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<Listing>,
) -> Result<Response> {
    let page = query.pagination();
    let visible_to_repository = query.visible_to_repository;
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    let org_id = org(scope)?;
    let mut rows = native::list_groups(&s.pool, actor, org_id)
        .await
        .map_err(error)?;
    if let Some(name) = visible_to_repository {
        let name = name
            .strip_prefix(&format!("{}/", p["org"]))
            .unwrap_or(&name);
        let repo = db::repositories::records::get_repository(&s.pool, &p["org"], name)
            .await
            .map_err(Error::unavailable)?
            .filter(|r| {
                r.owner_kind == "org" && r.owner_org_id == Some(org_id) && r.owner_user_id.is_none()
            })
            .ok_or_else(Error::missing)?;
        rows.retain(|g| g.allows_repository(repo.id, repo.is_private));
    }
    let total = rows.len();
    let (offset, limit) = page.window()?;
    let rows = rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(|g| group_document(&p["org"], g))
        .collect::<Result<Vec<_>>>()?;
    Ok((
        page.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(json!({"total_count":total,"runner_groups":rows})),
    )
        .into_response())
}
async fn get_group(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    Ok(Json(group_document(
        &p["org"],
        &selected_group(&s, actor, scope, &p).await?,
    )?))
}
async fn create_group(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    input: std::result::Result<Json<GroupInput>, JsonRejection>,
) -> Result<Response> {
    let (actor, scope) = scope(&s, &headers, &p, true).await?;
    let Json(input) = input.map_err(|_| Error::invalid("Invalid runner group"))?;
    let row = native::save_group(&s.pool, actor, org(scope)?, None, &input)
        .await
        .map_err(error)?;
    Ok((StatusCode::CREATED, Json(group_document(&p["org"], &row)?)).into_response())
}
async fn update_group(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    input: std::result::Result<Json<GroupInput>, JsonRejection>,
) -> Result<Json<Value>> {
    let (actor, scope) = scope(&s, &headers, &p, true).await?;
    let Json(input) = input.map_err(|_| Error::invalid("Invalid runner group"))?;
    let row = native::save_group(
        &s.pool,
        actor,
        org(scope)?,
        Some(id(&p, "runner_group_id")?),
        &input,
    )
    .await
    .map_err(error)?;
    Ok(Json(group_document(&p["org"], &row)?))
}
async fn delete_group(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (actor, scope) = scope(&s, &headers, &p, true).await?;
    native::delete_group(&s.pool, actor, org(scope)?, id(&p, "runner_group_id")?)
        .await
        .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn hosted(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
) -> Result<Json<Value>> {
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    selected_group(&s, actor, scope, &p).await?;
    page.window()?;
    // Codeyard runs its own workers; none are GitHub-hosted machines.
    Ok(Json(json!({"total_count":0,"runners":[]})))
}
async fn repositories(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    let group = selected_group(&s, actor, scope, &p).await?;
    if group.visibility != "selected" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "Runner group visibility is not selected",
        ));
    }
    let total = group.repositories.len();
    let (offset, limit) = page.window()?;
    let ids: Vec<_> = group.repositories.into_iter().skip(offset).take(limit).collect();
    let rows = db::repositories::records::get_repositories_by_ids(&s.pool, &ids).await.map_err(Error::unavailable)?;
    let organization = org(scope)?;
    if rows.iter().any(|repo| repo.owner_org_id != Some(organization) || repo.owner_kind != "org") { return Err(Error::missing()); }
    let credential = auth::authenticate(&s, &headers, true).await?.ok_or_else(auth::required)?;
    let repos = super::repos::documents(&s, &rows, Some(&credential)).await?;
    // No document loaded after the first group snapshot escapes a later loss of group authority.
    let current = selected_group(&s, actor, scope, &p).await?;
    if current.visibility != "selected" || ids.iter().any(|id| !current.repositories.contains(id)) { return Err(Error::missing()); }
    Ok((
        page.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(json!({"total_count":total,"repositories":repos})),
    )
        .into_response())
}
#[derive(Deserialize)]
struct Repositories {
    selected_repository_ids: Vec<i64>,
}
#[derive(Deserialize)]
struct Runners {
    runners: Vec<i64>,
}
async fn selection(
    s: &AppState,
    headers: &HeaderMap,
    p: &Params,
    selection: Selection,
    repositories: bool,
) -> Result<StatusCode> {
    let (actor, scope) = scope(s, headers, p, true).await?;
    let org = org(scope)?;
    let group = id(p, "runner_group_id")?;
    if repositories {
        native::group_repositories(&s.pool, actor, org, group, &selection).await
    } else {
        native::group_runners(&s.pool, actor, org, group, &selection).await
    }
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn set_repositories(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    input: std::result::Result<Json<Repositories>, JsonRejection>,
) -> Result<StatusCode> {
    let Json(input) = input.map_err(|_| Error::invalid("Invalid repository IDs"))?;
    selection(
        &s,
        &headers,
        &p,
        Selection::Replace(input.selected_repository_ids),
        true,
    )
    .await
}
async fn add_repository(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    selection(
        &s,
        &headers,
        &p,
        Selection::Add(id(&p, "repository_id")?),
        true,
    )
    .await
}
async fn remove_repository(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    selection(
        &s,
        &headers,
        &p,
        Selection::Remove(id(&p, "repository_id")?),
        true,
    )
    .await
}
async fn set_runners(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    input: std::result::Result<Json<Runners>, JsonRejection>,
) -> Result<StatusCode> {
    let Json(input) = input.map_err(|_| Error::invalid("Invalid runner IDs"))?;
    selection(&s, &headers, &p, Selection::Replace(input.runners), false).await
}
async fn add_runner(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    selection(
        &s,
        &headers,
        &p,
        Selection::Add(id(&p, "runner_id")?),
        false,
    )
    .await
}
async fn remove_runner_from_group(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    selection(
        &s,
        &headers,
        &p,
        Selection::Remove(id(&p, "runner_id")?),
        false,
    )
    .await
}
async fn list(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<Listing>,
) -> Result<Response> {
    let page = query.pagination();
    let name = query.name;
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    let group = if p.contains_key("runner_group_id") {
        Some(selected_group(&s, actor, scope, &p).await?.id)
    } else {
        None
    };
    let (offset, limit) = page.window()?;
    let (total, rows) = native::list_runners_page(&s.pool, actor, scope, &native::RunnerFilter {
        name: name.as_deref(), group, offset: offset as i64, limit: limit as i64, ..Default::default()
    }).await.map_err(error)?;
    let total = total as usize;
    let rows = rows.iter().map(document).collect::<Result<Vec<_>>>()?;
    Ok((
        page.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(json!({"total_count":total,"runners":rows})),
    )
        .into_response())
}
async fn get_runner(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    Ok(Json(document(
        &selected_runner(&s, actor, scope, &p).await?,
    )?))
}
async fn delete_runner(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (actor, scope) = scope(&s, &headers, &p, true).await?;
    native::delete_runner(&s.pool, actor, scope, id(&p, "runner_id")?)
        .await
        .map_err(runner_error)?;
    Ok(StatusCode::NO_CONTENT)
}
fn labels_body(row: &Runner) -> Result<Json<Value>> {
    let labels = label_documents(row).map_err(Error::unavailable)?;
    Ok(Json(json!({"total_count":labels.len(),"labels":labels})))
}
async fn labels(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    labels_body(&selected_runner(&s, actor, scope, &p).await?)
}
#[derive(Deserialize)]
struct LabelInput {
    labels: Vec<String>,
}
async fn change_labels(
    s: &AppState,
    headers: &HeaderMap,
    p: &Params,
    change: Labels,
) -> Result<Json<Value>> {
    let (actor, scope) = scope(s, headers, p, true).await?;
    labels_body(
        &native::labels(&s.pool, actor, scope, id(p, "runner_id")?, &change)
            .await
            .map_err(runner_error)?,
    )
}
async fn add_labels(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    input: std::result::Result<Json<LabelInput>, JsonRejection>,
) -> Result<Json<Value>> {
    let Json(input) = input.map_err(|_| Error::invalid_simple("Invalid labels"))?;
    change_labels(&s, &headers, &p, Labels::Add(input.labels)).await
}
async fn set_labels(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    input: std::result::Result<Json<LabelInput>, JsonRejection>,
) -> Result<Json<Value>> {
    let Json(input) = input.map_err(|_| Error::invalid_simple("Invalid labels"))?;
    change_labels(&s, &headers, &p, Labels::Replace(input.labels)).await
}
async fn clear_labels(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    change_labels(&s, &headers, &p, Labels::Clear).await
}
async fn remove_label(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    change_labels(
        &s,
        &headers,
        &p,
        Labels::Remove(p.get("name").cloned().ok_or_else(Error::missing)?),
    )
    .await
}
async fn deprecation(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    native::list_runners(&s.pool, actor, scope)
        .await
        .map_err(error)?;
    let version = p.get("version").ok_or_else(Error::missing)?;
    if version.len() > 100 || version.chars().any(char::is_control) {
        return Err(Error::validation("Runner", "version", "invalid"));
    }
    Ok(Json(
        json!({"runner_version":version,"registration_deprecates_at":null,"runtime_deprecates_at":null}),
    ))
}
async fn downloads(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (actor, scope) = scope(&s, &headers, &p, false).await?;
    native::list_runners(&s.pool, actor, scope)
        .await
        .map_err(error)?;
    Ok(Json(
        json!([{"os":std::env::consts::OS,"architecture":match std::env::consts::ARCH {"x86_64"=>"x64","aarch64"=>"arm64",arch=>arch},"download_url":format!("{}/api/actions/runners/download",crate::backend::api::http::config::link_origin()),"filename":format!("codeyard-runner-{}-{}",std::env::consts::OS,std::env::consts::ARCH)}]),
    ))
}
async fn ticket(state: &AppState) -> Result<JoinTicket> {
    crate::backend::ci::runner_credentials::ticket(&state.pool, state.cluster.as_ref())
        .await
        .map_err(Error::unavailable)
}
async fn registration_token(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Response> {
    let (actor, scope) = scope(&s, &headers, &p, true).await?;
    // Check scope before disclosing whether this installation serves cluster pairing.
    native::list_runners(&s.pool, actor, scope)
        .await
        .map_err(error)?;
    let ticket = ticket(&s).await?;
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    native::registration(
        &s.pool,
        actor,
        scope,
        Registration {
            hash: &ticket.secret_hash(),
            expires,
            name: None,
            group: None,
            labels: &[],
        },
    )
    .await
    .map_err(error)?;
    Ok((
        StatusCode::CREATED,
        [("cache-control", "no-store")],
        Json(json!({"token":ticket.encode(),"expires_at":expires})),
    )
        .into_response())
}
async fn remove_token(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Response> {
    let (actor, scope) = scope(&s, &headers, &p, true).await?;
    let token = crate::backend::ci::runner_credentials::new_remove_token();
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    native::remove_token(
        &s.pool,
        actor,
        scope,
        &crate::backend::ci::runner_credentials::remove_hash(&token),
        expires,
    )
    .await
    .map_err(error)?;
    Ok((
        StatusCode::CREATED,
        [("cache-control", "no-store")],
        Json(json!({"token":token,"expires_at":expires})),
    )
        .into_response())
}
#[derive(Deserialize)]
struct Jit {
    name: String,
    runner_group_id: i64,
    labels: Vec<String>,
    #[serde(default = "work_folder")]
    work_folder: String,
}
fn work_folder() -> String {
    "_work".into()
}
async fn jit(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    input: std::result::Result<Json<Jit>, JsonRejection>,
) -> Result<Response> {
    let (actor, scope) = scope(&s, &headers, &p, true).await?;
    native::list_runners(&s.pool, actor, scope)
        .await
        .map_err(error)?;
    let Json(input) = input.map_err(|_| Error::invalid_simple("Invalid runner configuration"))?;
    if input.labels.is_empty() {
        return Err(Error::invalid_simple("Invalid runner labels"));
    }
    if !crate::backend::ci::runner_credentials::valid_work_folder(&input.work_folder) {
        return Err(Error::invalid_simple("Invalid runner work_folder"));
    }
    let ticket = ticket(&s).await?;
    let row = native::registration(
        &s.pool,
        actor,
        scope,
        Registration {
            hash: &ticket.secret_hash(),
            expires: chrono::Utc::now() + chrono::Duration::hours(1),
            name: Some(&input.name),
            group: Some(input.runner_group_id),
            labels: &input.labels,
        },
    )
    .await
    .map_err(runner_error)?
    .ok_or_else(|| Error::unavailable(()))?;
    let config = crate::backend::ci::runner_credentials::JitConfig {
        ticket: ticket.encode(),
        node_name: native::jit_node_name(row.id, &ticket.secret_hash()),
        work_folder: input.work_folder,
    };
    Ok((StatusCode::CREATED,[("cache-control","no-store")],Json(json!({"runner":document(&row)?,"encoded_jit_config":STANDARD.encode(serde_json::to_vec(&config).map_err(Error::unavailable)?)}))).into_response())
}
