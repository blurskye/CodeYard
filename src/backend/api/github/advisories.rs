//! Repository advisories and confidential reports share the native disclosure workflow.
use super::{auth, orgs, repo_url, repos, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        advisories::{self, Advisory, Data, Listing, Scope, Sort},
        repositories::security,
    },
    repositories::{
        advisories as workflow,
        lifecycle::{LifecycleFailure, StorageError},
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, patch, post, MethodRouter},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
type Params = HashMap<String, String>;
pub(super) fn failure(error: anyhow::Error) -> Error {
    if matches!(error.downcast_ref::<db::dependency_graph::GraphError>(), Some(db::dependency_graph::GraphError::Limit)) {
        return Error::new(StatusCode::UNPROCESSABLE_ENTITY, db::dependency_graph::GraphError::Limit.to_string());
    }
    match error.downcast_ref::<advisories::Refusal>() {
        Some(advisories::Refusal::Denied) => {
            return Error::new(StatusCode::FORBIDDEN, "Advisory administration denied")
        }
        Some(advisories::Refusal::Missing) => return Error::missing(),
        Some(advisories::Refusal::Invalid) => {
            return Error::validation("RepositoryAdvisory", "configuration", "invalid")
        }
        Some(advisories::Refusal::Conflict) => {
            return Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "The advisory state does not allow this operation",
            )
        }
        None => {}
    }
    match error.downcast_ref::<security::Refusal>() {
        Some(security::Refusal::Denied) => {
            return Error::new(
                StatusCode::FORBIDDEN,
                "Private vulnerability reporting denied",
            )
        }
        Some(security::Refusal::Missing) => return Error::missing(),
        Some(security::Refusal::Invalid(_)) => {
            return Error::validation("RepositoryAdvisory", "report", "invalid")
        }
        Some(security::Refusal::Limit) => {
            return Error::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Security reporting limit reached",
            )
        }
        None => {}
    }
    if let Some(error) = error.downcast_ref::<StorageError>() {
        return Error::new(
            match error.kind {
                LifecycleFailure::Denied => StatusCode::FORBIDDEN,
                LifecycleFailure::Conflict | LifecycleFailure::Invalid => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                _ => StatusCode::SERVICE_UNAVAILABLE,
            },
            error.message,
        );
    }
    Error::unavailable(error)
}
async fn person(state: &AppState, id: Option<i64>) -> Result<Value> {
    if let Some(id) = id {
        Ok(db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .as_ref()
            .map(payloads::user)
            .unwrap_or(Value::Null))
    } else {
        Ok(Value::Null)
    }
}
async fn document(
    state: &AppState,
    row: &Advisory,
    viewer: Option<&auth::Authentication>,
) -> Result<Value> {
    let data = Data::from_row(row).map_err(Error::unavailable)?;
    let repo = db::repositories::records::get_repository_by_id(&state.pool, row.repo_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let owner = db::repositories::records::repository_owner_name(&state.pool, row.repo_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let author = person(state, row.author_id).await?;
    let cvss = if let Some(vector) = &data.cvss_vector_string {
        json!({"vector_string":vector,"score":advisories::cvss_score(vector).map_err(Error::unavailable)?})
    } else {
        Value::Null
    };
    let mut identifiers = vec![json!({"type":"GHSA","value":row.ghsa_id})];
    if let Some(cve) = &data.cve_id {
        identifiers.push(json!({"type":"CVE","value":cve}));
    }
    let fork = if let (Some(id), Some(viewer)) = (row.private_fork_id, viewer) {
        if let Some(fork) = db::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
        {
            // The private fork shows only to a credential that may read it.
            let allowed = auth::readable_repositories(state, Some(viewer), &[id], "metadata")
                .await?
                .contains(&id);
            if allowed {
                repos::document(state, &fork, Some(viewer)).await?
            } else {
                Value::Null
            }
        } else {
            Value::Null
        }
    } else {
        Value::Null
    };
    Ok(
        json!({"ghsa_id":row.ghsa_id,"cve_id":data.cve_id,"url":format!("{}/security-advisories/{}",repo_url(&owner,&repo.name),row.ghsa_id),"html_url":format!("{}/{}/{}/security/advisories/{}",crate::backend::api::http::config::link_origin(),crate::webui::encode_component(&owner),crate::webui::encode_component(&repo.name),row.ghsa_id),"summary":data.summary,"description":data.description,"severity":data.severity,"author":author,"publisher":person(state,row.publisher_id).await?,"identifiers":identifiers,"state":row.state,"created_at":row.created_at,"updated_at":row.updated_at,"published_at":row.published_at,"closed_at":row.closed_at,"withdrawn_at":null,"submission":row.report_id.map(|_|json!({"accepted":row.report_accepted})),"vulnerabilities":data.vulnerabilities,"cvss":cvss,"cwes":null,"cwe_ids":data.cwe_ids,"credits":data.credits,"credits_detailed":null,"collaborating_users":[],"collaborating_teams":[],"private_fork":fork}),
    )
}
#[derive(Default, Deserialize)]
struct Options {
    state: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    per_page: Option<usize>,
    before: Option<String>,
    after: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Cursor {
    at: DateTime<Utc>,
    id: i64,
    sort: String,
    ascending: bool,
}
fn link(uri: &Uri, cursor: &str, before: bool, limit: usize) -> String {
    let mut pairs = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .filter(|(k, _)| !matches!(k.as_ref(), "before" | "after" | "per_page"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect::<Vec<_>>();
    pairs.push((
        if before { "before" } else { "after" }.into(),
        cursor.into(),
    ));
    pairs.push(("per_page".into(), limit.to_string()));
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    format!(
        "<{}{}?{query}>; rel=\"{}\"",
        crate::backend::api::http::config::link_origin(),
        uri.path(),
        if before { "prev" } else { "next" }
    )
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(query): Query<Options>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (scope, actor) = if let Some(org) = params.get("org") {
        let actor = auth::authenticate(&state, &headers, false).await?;
        let org = orgs::target(&state, org).await?;
        (Scope::Organization(org.id), actor)
    } else {
        let (repo, actor) =
            repository(&state, &headers, &params["owner"], &params["repo"], false).await?;
        (Scope::Repository(repo.id), actor)
    };
    let invalid = || Error::new(StatusCode::BAD_REQUEST, "Invalid advisory list options");
    let sort_name = query.sort.as_deref().unwrap_or("created");
    let sort = match sort_name {
        "created" => Sort::Created,
        "updated" => Sort::Updated,
        "published" => Sort::Published,
        _ => return Err(invalid()),
    };
    let ascending = match query.direction.as_deref().unwrap_or("desc") {
        "asc" => true,
        "desc" => false,
        _ => return Err(invalid()),
    };
    if query
        .state
        .as_deref()
        .is_some_and(|s| !matches!(s, "triage" | "draft" | "published" | "closed"))
        || query.before.is_some() && query.after.is_some()
    {
        return Err(invalid());
    }
    let cursor = query
        .before
        .as_ref()
        .or(query.after.as_ref())
        .map(|value| {
            if value.len() > 1024 {
                return Err(invalid());
            }
            let cursor: Cursor =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?)
                    .map_err(|_| invalid())?;
            if cursor.id <= 0 || cursor.sort != sort_name || cursor.ascending != ascending {
                return Err(invalid());
            }
            Ok((cursor.at, cursor.id))
        })
        .transpose()?;
    let limit = query.per_page.unwrap_or(30).clamp(1, 100);
    let options = Listing {
        state: query.state.as_deref(),
        sort,
        ascending,
        before: query.before.is_some(),
        cursor,
        limit: (limit + 1) as i64,
        offset: 0,
    };
    let mut rows = advisories::list(
        &state.pool,
        actor.as_ref().map(|a| a.user.id),
        scope,
        &options,
    )
    .await
    .map_err(failure)?;
    let more = rows.len() > limit;
    if more && options.before {
        rows.remove(0);
    } else {
        rows.truncate(limit);
    }
    let encode = |row: &Advisory| {
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&Cursor {
                at: sort.timestamp(row),
                id: row.id,
                sort: sort_name.into(),
                ascending,
            })
            .unwrap(),
        )
    };
    let mut links = Vec::new();
    if let Some(first) = rows.first() {
        if query.after.is_some() || (options.before && more) {
            links.push(link(&uri, &encode(first), true, limit));
        }
    }
    if let Some(last) = rows.last() {
        if query.before.is_some() || (!options.before && more) {
            links.push(link(&uri, &encode(last), false, limit));
        }
    }
    let mut documents = Vec::new();
    for row in rows {
        documents.push(document(&state, &row, actor.as_ref()).await?);
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty() {
        headers.insert(
            header::LINK,
            links.join(", ").parse().map_err(Error::unavailable)?,
        );
    }
    Ok((headers, Json(documents)).into_response())
}
async fn get_advisory(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, actor) =
        repository(&state, &headers, &params["owner"], &params["repo"], false).await?;
    let row = advisories::get(
        &state.pool,
        actor.as_ref().map(|a| a.user.id),
        repo.id,
        &params["ghsa_id"],
    )
    .await
    .map_err(failure)?
    .ok_or_else(Error::missing)?;
    Ok(Json(
        document(&state, &row, actor.as_ref()).await?,
    ))
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
    Json(value): Json<Value>,
) -> Result<Response> {
    let (repo, actor) =
        repository(&state, &headers, &params["owner"], &params["repo"], true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let report = uri.path().ends_with("/reports");
    let mut row = advisories::create(&state.pool, actor.user.id, repo.id, &value, report)
        .await
        .map_err(failure)?;
    if value["start_private_fork"] == true {
        match workflow::private_fork(&state, actor.user.id, &repo, &row.ghsa_id).await {
            Ok(_) => {
                if let Some(fresh) = advisories::get(&state.pool, Some(actor.user.id), repo.id, &row.ghsa_id).await.map_err(failure)? {
                    row = fresh;
                } else if !report {
                    return Err(Error::missing());
                }
            }
            Err(error) if report => {
                // The report is already committed; optional remediation cannot reject it.
                let denied = matches!(error.downcast_ref::<advisories::Refusal>(), Some(advisories::Refusal::Denied))
                    || error.downcast_ref::<StorageError>().is_some_and(|error| matches!(error.kind, LifecycleFailure::Denied));
                if !denied { tracing::warn!(ghsa_id=%row.ghsa_id,%error,"Optional vulnerability-report fork was not created"); }
            }
            Err(error) => return Err(failure(error)),
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(document(&state, &row, Some(&actor)).await?),
    )
        .into_response())
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Json(value): Json<Value>,
) -> Result<Json<Value>> {
    let (repo, actor) =
        repository(&state, &headers, &params["owner"], &params["repo"], true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let row = advisories::update(
        &state.pool,
        actor.user.id,
        repo.id,
        &params["ghsa_id"],
        &value,
    )
    .await
    .map_err(failure)?;
    Ok(Json(document(&state, &row, Some(&actor)).await?))
}
async fn cve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (repo, actor) =
        repository(&state, &headers, &params["owner"], &params["repo"], true).await?;
    advisories::request_cve(
        &state.pool,
        actor.ok_or_else(auth::required)?.user.id,
        repo.id,
        &params["ghsa_id"],
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::ACCEPTED)
}
async fn fork(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Response> {
    let (repo, actor) =
        repository(&state, &headers, &params["owner"], &params["repo"], true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let fork = workflow::private_fork(&state, actor.user.id, &repo, &params["ghsa_id"])
        .await
        .map_err(failure)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(repos::document(&state, &fork, Some(&actor)).await?),
    )
        .into_response())
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "security-advisories/list-org-repository-advisories",
            get(list),
        ),
        ("security-advisories/list-repository-advisories", get(list)),
        (
            "security-advisories/create-repository-advisory",
            post(create),
        ),
        (
            "security-advisories/create-private-vulnerability-report",
            post(create),
        ),
        (
            "security-advisories/get-repository-advisory",
            get(get_advisory),
        ),
        (
            "security-advisories/update-repository-advisory",
            patch(update),
        ),
        (
            "security-advisories/create-repository-advisory-cve-request",
            post(cve),
        ),
        ("security-advisories/create-fork", post(fork)),
    ]
}
