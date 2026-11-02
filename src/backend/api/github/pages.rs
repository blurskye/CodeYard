//! Pages control plane. Build and deployment attempts fail explicitly without a publisher.
use super::{
    auth, contents::store_error, pagination::Pagination, repo_url, repository, Error, Result,
};
use crate::backend::{
    app::AppState,
    db::{self, pages},
    repositories::{access, store::repo_store},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
type Params = HashMap<String, String>;
fn failure(error: anyhow::Error) -> Error {
    if error.is::<db::users::accounts::AccountRestricted>() {
        return Error::new(StatusCode::FORBIDDEN, "Pages administration denied");
    }
    match error.downcast_ref::<pages::Refusal>() {
        Some(pages::Refusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Pages administration denied")
        }
        Some(pages::Refusal::Missing) => Error::missing(),
        Some(pages::Refusal::Duplicate) => Error::validation("Pages", "site", "already_exists"),
        Some(pages::Refusal::Invalid) => Error::validation("Pages", "configuration", "invalid"),
        Some(pages::Refusal::Conflict) => Error::new(
            StatusCode::CONFLICT,
            "This Pages deployment is no longer cancellable",
        ),
        None => Error::unavailable(error),
    }
}
async fn visible(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    write: bool,
) -> Result<(db::models::Repository, Option<auth::Authentication>)> {
    repository(state, headers, &params["owner"], &params["repo"], write).await
}
fn reader(actor: Option<&auth::Authentication>) -> db::repositories::credentials::Reader<'_> {
    db::repositories::credentials::Reader {
        user: actor.map(|actor| actor.user.id),
        credential: actor.map(auth::Authentication::credential),
    }
}
async fn require_site(
    state: &AppState,
    actor: Option<&auth::Authentication>,
    repo: i64,
) -> Result<pages::Site> {
    pages::site(&state.pool, &reader(actor), repo)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)
}
fn site_document(site: &pages::Site, owner: &str, name: &str, status: Option<&str>) -> Value {
    let mut value = json!({"url":format!("{}/pages",repo_url(owner,name)),"status":status,"cname":site.cname,"custom_404":false,"build_type":site.build_type,"public":site.public,"https_enforced":site.https_enforced,"protected_domain_state":"unverified"});
    if let (Some(branch), Some(path)) = (&site.branch, &site.path) {
        value["source"] = json!({"branch":branch,"path":path});
    }
    value
}
async fn settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, actor) = visible(&state, &headers, &params, false).await?;
    let site = require_site(&state, actor.as_ref(), repo.id).await?;
    let builds = pages::builds(&state.pool, &reader(actor.as_ref()), repo.id, 0, 1)
        .await
        .map_err(failure)?;
    Ok(Json(site_document(
        &site,
        &params["owner"],
        &params["repo"],
        builds.first().map(|b| b.status.as_str()),
    )))
}
async fn mutate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    method: Method,
    Json(value): Json<Value>,
) -> Result<Response> {
    let (repo, actor) = visible(&state, &headers, &params, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    if !access::can_maintain(&state.pool, &actor.user, &repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Pages administration denied",
        ));
    }
    let create = method == Method::POST;
    let update = pages::Update::parse(&value, create).map_err(failure)?;
    if let Some(source) = &update.source {
        repo_store(&repo, state.cluster.as_ref())
            .commit(&format!("refs/heads/{}", source.branch))
            .await
            .map_err(|error| {
                if error.kind == crate::backend::repositories::store::ErrorKind::NotFound {
                    Error::validation("Pages", "source", "invalid")
                } else {
                    store_error(error)
                }
            })?;
    }
    let site = pages::save(
        &state.pool,
        actor.user.id,
        &db::users::accounts::AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        repo.id,
        &update,
        create,
    )
    .await
    .map_err(failure)?;
    if create {
        Ok((
            StatusCode::CREATED,
            Json(site_document(
                &site,
                &params["owner"],
                &params["repo"],
                None,
            )),
        )
            .into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (repo, actor) = visible(&state, &headers, &params, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    pages::delete(
        &state.pool,
        actor.user.id,
        &db::users::accounts::AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        repo.id,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn build_document(
    state: &AppState,
    owner: &str,
    name: &str,
    build: &pages::Build,
) -> Result<Value> {
    let pusher = if let Some(id) = build.actor_id {
        db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .as_ref()
            .map(payloads::user)
    } else {
        None
    };
    Ok(
        json!({"url":format!("{}/pages/builds/{}",repo_url(owner,name),build.id),"status":build.status,"error":{"message":build.error},"pusher":pusher,"commit":build.commit_sha,"duration":0,"created_at":build.created_at,"updated_at":build.updated_at}),
    )
}
async fn builds(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, actor) = visible(&state, &headers, &params, false).await?;
    require_site(&state, actor.as_ref(), repo.id).await?;
    let (offset, limit) = page.window()?;
    let rows = pages::builds(
        &state.pool,
        &reader(actor.as_ref()),
        repo.id,
        offset as i64,
        (limit + 1) as i64,
    )
    .await
    .map_err(failure)?;
    let has_next = rows.len() > limit;
    let mut documents = Vec::new();
    for row in rows.into_iter().take(limit) {
        documents.push(build_document(&state, &params["owner"], &params["repo"], &row).await?);
    }
    page.response(&uri, documents, has_next)
}
async fn build(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, actor) = visible(&state, &headers, &params, false).await?;
    require_site(&state, actor.as_ref(), repo.id).await?;
    let row = if let Some(id) = params.get("build_id") {
        pages::build(
            &state.pool,
            &reader(actor.as_ref()),
            repo.id,
            id.parse().map_err(|_| Error::missing())?,
        )
        .await
        .map_err(failure)?
    } else {
        pages::builds(&state.pool, &reader(actor.as_ref()), repo.id, 0, 1)
            .await
            .map_err(failure)?
            .into_iter()
            .next()
    }
    .ok_or_else(Error::missing)?;
    Ok(Json(
        build_document(&state, &params["owner"], &params["repo"], &row).await?,
    ))
}
async fn request_build(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Response> {
    let (repo, actor) = visible(&state, &headers, &params, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let site = require_site(&state, Some(&actor), repo.id).await?;
    let branch = site
        .branch
        .ok_or_else(|| Error::validation("Pages", "source", "invalid"))?;
    let commit = repo_store(&repo, state.cluster.as_ref())
        .commit(&format!("refs/heads/{branch}"))
        .await
        .map_err(store_error)?;
    let build = pages::request_build(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        repo.id,
        &commit.sha,
    )
    .await
    .map_err(failure)?;
    Ok((StatusCode::CREATED,Json(json!({"url":format!("{}/pages/builds/{}",repo_url(&params["owner"],&params["repo"]),build.id),"status":build.status}))).into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentInput {
    artifact_id: Option<i64>,
    artifact_url: Option<String>,
    environment: Option<String>,
    pages_build_version: String,
    oidc_token: String,
}
async fn create_deployment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Json(input): Json<DeploymentInput>,
) -> Result<Json<Value>> {
    let (repo, actor) = visible(&state, &headers, &params, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let site = require_site(&state, Some(&actor), repo.id).await?;
    let invalid = || Error::validation("PagesDeployment", "configuration", "invalid");
    if input.oidc_token.is_empty()
        || input.oidc_token.len() > 16384
        || !crate::git_ids::is_full_commit_id(&input.pages_build_version)
        || input.artifact_id.is_some_and(|id| id <= 0)
        || (input.artifact_id.is_none() && input.artifact_url.is_none())
    {
        return Err(invalid());
    }
    if let Some(url) = &input.artifact_url {
        let url = url::Url::parse(url).map_err(|_| invalid())?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid());
        }
    }
    let environment = input.environment.as_deref().unwrap_or("github-pages");
    if environment.is_empty()
        || environment.len() > 255
        || environment.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let sha = repo_store(&repo, state.cluster.as_ref())
        .commit(&input.pages_build_version)
        .await
        .map_err(store_error)?
        .sha;
    let deployment = pages::request_deployment(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        repo.id,
        pages::Attempt {
            sha: &sha,
            environment,
            artifact_id: input.artifact_id,
        },
    )
    .await
    .map_err(failure)?;
    let target = site
        .cname
        .map(|cname| format!("https://{cname}/"))
        .unwrap_or_else(|| {
            format!(
                "{}/{}/{}/pages",
                crate::backend::api::http::config::link_origin(),
                crate::webui::encode_component(&params["owner"]),
                crate::webui::encode_component(&params["repo"])
            )
        });
    Ok(Json(
        json!({"id":deployment.id.to_string(),"status_url":format!("{}/pages/deployments/{}",repo_url(&params["owner"],&params["repo"]),deployment.id),"page_url":target}),
    ))
}
async fn deployment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, actor) = visible(&state, &headers, &params, false).await?;
    require_site(&state, actor.as_ref(), repo.id).await?;
    let id = params["pages_deployment_id"]
        .parse()
        .map_err(|_| Error::missing())?;
    let row = pages::deployment(&state.pool, &reader(actor.as_ref()), repo.id, id)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    Ok(Json(json!({"status":row.status})))
}
async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (repo, actor) = visible(&state, &headers, &params, true).await?;
    require_site(&state, actor.as_ref(), repo.id).await?;
    let id = params["pages_deployment_id"]
        .parse()
        .map_err(|_| Error::missing())?;
    let actor = actor.ok_or_else(auth::required)?;
    pages::cancel(&state.pool, actor.user.id, &actor.credential(), repo.id, id)
        .await
        .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, actor) = visible(&state, &headers, &params, false).await?;
    let site = require_site(&state, actor.as_ref(), repo.id).await?;
    let host = site
        .cname
        .ok_or_else(|| Error::invalid("No custom Pages domain is configured"))?;
    // Resolve only; never fetch caller-selected hosts or probe an internal HTTP service.
    let resolves = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::lookup_host((host.as_str(), 443)),
    )
    .await
    {
        Ok(Ok(mut addresses)) => addresses.next().is_some(),
        _ => false,
    };
    Ok(Json(
        json!({"domain":{"host":host,"uri":format!("https://{host}/"),"dns_resolves":resolves,"is_valid_domain":true,"is_valid":false,"is_served_by_pages":false,"is_pages_domain":false,"reason":pages::NO_PUBLISHER},"alt_domain":null}),
    ))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/get-pages", get(settings)),
        ("repos/create-pages-site", post(mutate)),
        ("repos/update-information-about-pages-site", put(mutate)),
        ("repos/delete-pages-site", delete(remove)),
        ("repos/list-pages-builds", get(builds)),
        ("repos/request-pages-build", post(request_build)),
        ("repos/get-latest-pages-build", get(build)),
        ("repos/get-pages-build", get(build)),
        ("repos/create-pages-deployment", post(create_deployment)),
        ("repos/get-pages-deployment", get(deployment)),
        ("repos/cancel-pages-deployment", post(cancel)),
        ("repos/get-pages-health-check", get(health)),
    ]
}
