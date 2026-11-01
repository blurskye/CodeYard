//! Dependabot alerts use the same live database authority as native security forms.
use super::{advisories::failure, auth, orgs, repo_url, repos, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        dependabot_alerts::{self as store, Alert, Update},
    },
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, patch, MethodRouter},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
type Params = HashMap<String, String>;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("security-advisories/list-global-advisories", get(global)),
        (
            "security-advisories/get-global-advisory",
            get(global_detail),
        ),
        ("dependabot/list-alerts-for-repo", get(list)),
        ("dependabot/list-alerts-for-org", get(list)),
        ("dependabot/get-alert", get(detail)),
        ("dependabot/update-alert", patch(update)),
    ]
}
pub(super) async fn import(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let value = store::import(&state.pool, actor.database_actor(), &value)
        .await
        .map_err(failure)?;
    Ok((StatusCode::CREATED, Json(value)).into_response())
}
async fn global(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<Params>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    validate(&query, true)?;
    let sort = query.get("sort").map_or("published", String::as_str);
    let ascending = query.get("direction").is_some_and(|v| v == "asc");
    let limit = query
        .get("per_page")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| Error::invalid("Invalid page size"))?
        .unwrap_or(30)
        .clamp(1, 100);
    let cursor = if let Some(cursor) = query.get("after").or(query.get("before")) {
        if cursor.len() > 2048 {
            return Err(Error::invalid("Invalid cursor"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| Error::invalid("Invalid cursor"))?;
        let (field, direction, key): (String, bool, (String, String)) =
            serde_json::from_slice(&bytes).map_err(|_| Error::invalid("Invalid cursor"))?;
        if field != sort || direction != ascending {
            return Err(Error::invalid("Invalid cursor"));
        }
        Some(key)
    } else {
        None
    };
    let offset = if cursor.is_some() {
        0
    } else {
        let page = query
            .get("page")
            .map(|v| v.parse::<usize>())
            .transpose()
            .map_err(|_| Error::invalid("Invalid page"))?
            .unwrap_or(1);
        if !(1..=10000).contains(&page) {
            return Err(Error::invalid("Invalid page"));
        }
        (page - 1) * limit
    };
    let selected = store::global_page(
        &state.pool,
        &store::CatalogueQuery {
            filters: query.clone(),
            sort: sort.to_owned(),
            ascending,
            cursor,
            before: query.contains_key("before"),
            offset,
            limit,
        },
    )
    .await
    .map_err(failure)?;
    page_response(
        selected.documents,
        &query,
        &uri,
        true,
        selected.previous,
        selected.next,
    )
}
async fn global_detail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    auth::authenticate(&state, &headers, false).await?;
    let value = store::global_one(&state.pool, &id)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    Ok(Json(value))
}
fn csv(query: &Params, key: &str, value: &str) -> bool {
    query
        .get(key)
        .is_none_or(|s| s.split(',').any(|s| s == value))
}
fn validate(q: &Params, global: bool) -> Result<()> {
    let allowed = if global {
        &[
            "ghsa_id",
            "type",
            "cve_id",
            "ecosystem",
            "severity",
            "cwes",
            "is_withdrawn",
            "affects",
            "published",
            "updated",
            "modified",
            "sort",
            "direction",
            "per_page",
            "page",
            "before",
            "after",
        ][..]
    } else {
        &[
            "state",
            "severity",
            "ecosystem",
            "package",
            "manifest",
            "scope",
            "has",
            "sort",
            "direction",
            "per_page",
            "page",
            "before",
            "after",
            "classification",
        ][..]
    };
    if q.keys().any(|k| !allowed.contains(&k.as_str()))
        || q.get("direction")
            .is_some_and(|s| !matches!(s.as_str(), "asc" | "desc"))
        || q.contains_key("before") && q.contains_key("after")
    {
        return Err(Error::invalid("Invalid list options"));
    }
    let sorts = if global {
        &["published", "updated"][..]
    } else {
        &["created", "updated", "severity"][..]
    };
    if q.get("sort").is_some_and(|s| !sorts.contains(&s.as_str())) {
        return Err(Error::invalid("Invalid sort"));
    }
    if q.get("has").is_some_and(|value| value != "patch") {
        return Err(Error::invalid("Invalid patch availability filter"));
    }
    if q.get("is_withdrawn")
        .is_some_and(|s| !matches!(s.as_str(), "true" | "false"))
    {
        return Err(Error::invalid("Invalid withdrawal filter"));
    }
    Ok(())
}
fn sort_key(v: &Value, sort: &str, global: bool) -> (String, String) {
    let value = match sort {
        "severity" => match v["security_advisory"]["severity"].as_str().unwrap_or("") {
            "critical" => "4",
            "high" => "3",
            "medium" => "2",
            _ => "1",
        }
        .into(),
        field => {
            let field = match field {
                "published" => "published_at",
                "updated" => "updated_at",
                _ => "created_at",
            };
            v[field]
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| {
                    value
                        .with_timezone(&chrono::Utc)
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                })
                .unwrap_or_default()
        }
    };
    let id = if global {
        v["ghsa_id"].as_str().unwrap_or("").to_owned()
    } else {
        format!(
            "{:020}:{:020}",
            v["repository"]["id"].as_i64().unwrap_or(0),
            v["number"].as_i64().unwrap_or(0)
        )
    };
    (value, id)
}
fn page(mut rows: Vec<Value>, q: &Params, uri: &Uri, global: bool) -> Result<Response> {
    let sort = q
        .get("sort")
        .map_or(if global { "published" } else { "created" }, String::as_str);
    let ascending = q.get("direction").is_some_and(|s| s == "asc");
    let limit = super::pagination::page_size(
        q.get("per_page")
            .map(|s| s.parse::<usize>())
            .transpose()
            .map_err(|_| Error::invalid("Invalid page size"))?,
    );
    rows.sort_by_cached_key(|v| sort_key(v, sort, global));
    if !ascending {
        rows.reverse();
    }
    let offset = if let Some(cursor) = q.get("after").or(q.get("before")) {
        if cursor.len() > 2048 {
            return Err(Error::invalid("Invalid cursor"));
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| Error::invalid("Invalid cursor"))?;
        let (cursor_sort, cursor_ascending, key): (String, bool, (String, String)) =
            serde_json::from_slice(&decoded).map_err(|_| Error::invalid("Invalid cursor"))?;
        if cursor_sort != sort || cursor_ascending != ascending {
            return Err(Error::invalid("Invalid cursor"));
        }
        if q.contains_key("before") {
            let end = rows.partition_point(|v| {
                if ascending {
                    sort_key(v, sort, global) < key
                } else {
                    sort_key(v, sort, global) > key
                }
            });
            rows.truncate(end);
            end.saturating_sub(limit)
        } else {
            rows.partition_point(|v| {
                if ascending {
                    sort_key(v, sort, global) <= key
                } else {
                    sort_key(v, sort, global) >= key
                }
            })
        }
    } else {
        let page = q
            .get("page")
            .map(|s| s.parse::<usize>())
            .transpose()
            .map_err(|_| Error::invalid("Invalid page"))?
            .unwrap_or(1);
        if !(1..=10000).contains(&page) {
            return Err(Error::invalid("Invalid page"));
        }
        (page - 1) * limit
    };
    let more = offset.saturating_add(limit) < rows.len();
    let selected: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
    page_response(
        selected,
        q,
        uri,
        global,
        offset > 0 || q.contains_key("after"),
        more || q.contains_key("before"),
    )
}
fn page_response(
    selected: Vec<Value>,
    q: &Params,
    uri: &Uri,
    global: bool,
    previous: bool,
    next: bool,
) -> Result<Response> {
    let sort = q
        .get("sort")
        .map_or(if global { "published" } else { "created" }, String::as_str);
    let ascending = q.get("direction").is_some_and(|s| s == "asc");
    let mut links = Vec::new();
    for (rel, row, include) in [
        ("prev", selected.first(), previous),
        ("next", selected.last(), next),
    ] {
        if !include {
            continue;
        }
        if let Some(row) = row {
            let cursor = URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&(sort, ascending, sort_key(row, sort, global)))
                    .map_err(Error::unavailable)?,
            );
            let mut params = q.clone();
            for k in ["page", "before", "after"] {
                params.remove(k);
            }
            params.insert(
                if rel == "prev" { "before" } else { "after" }.into(),
                cursor,
            );
            let query = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(params)
                .finish();
            links.push(format!(
                "<{}{}?{}>; rel=\"{rel}\"",
                crate::backend::api::http::config::link_origin(),
                uri.path(),
                query
            ));
        }
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty() {
        headers.insert(
            header::LINK,
            links.join(", ").parse().map_err(Error::unavailable)?,
        );
    }
    Ok((headers, Json(selected)).into_response())
}

pub(crate) async fn document(
    state: &AppState,
    row: &Alert,
    owner: &str,
    name: &str,
) -> Result<Value> {
    let global: Value = serde_json::from_str(&row.advisory).map_err(Error::unavailable)?;
    let vulnerability = |v: &Value| json!({"package":v["package"],"severity":global["severity"],"vulnerable_version_range":v["vulnerable_version_range"],"first_patched_version":v["first_patched_version"].as_str().map(|s|json!({"identifier":s}))});
    let vulnerabilities: Vec<_> = global["vulnerabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .map(vulnerability)
        .collect();
    let v: Value = serde_json::from_str(&row.vulnerability).map_err(Error::unavailable)?;
    let cvss = global
        .get("cvss")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or(json!({"score":0.0,"vector_string":null}));
    let advisory = json!({"ghsa_id":global["ghsa_id"],"cve_id":global["cve_id"],"summary":global["summary"],"description":global["description"].as_str().unwrap_or(""),"severity":global["severity"],"classification":if global["type"]=="malware" {"malware"} else {"general"},"cvss":cvss,"cwes":global["cwes"].as_array().cloned().unwrap_or_default(),"identifiers":global["identifiers"],"references":global["references"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|s|json!({"url":s})).collect::<Vec<_>>(),"vulnerabilities":vulnerabilities,"published_at":global["published_at"],"updated_at":global["updated_at"],"withdrawn_at":global["withdrawn_at"]});
    let dismissed_by = if let Some(id) = row.dismissed_by {
        db::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .as_ref()
            .map(payloads::user)
    } else {
        None
    };
    Ok(
        json!({"number":row.number,"state":row.state,"dependency":{"package":{"ecosystem":row.ecosystem,"name":row.package},"manifest_path":row.manifest,"version_requirement":row.version,"scope":if row.scope=="unknown" {None} else {Some(&row.scope)},"relationship":"unknown"},"security_advisory":advisory,"security_vulnerability":vulnerability(&v),"url":format!("{}/dependabot/alerts/{}",repo_url(owner,name),row.number),"html_url":format!("{}/{}/{}/security/dependabot",crate::backend::api::http::config::link_origin(),crate::webui::encode_component(owner),crate::webui::encode_component(name)),"created_at":row.created_at,"updated_at":row.updated_at,"fixed_at":row.fixed_at,"dismissed_at":row.dismissed_at,"dismissed_by":dismissed_by,"dismissed_reason":row.dismissed_reason,"dismissed_comment":row.dismissed_comment,"auto_dismissed_at":null,"assignees":[]}),
    )
}
fn alert_matches(v: &Value, q: &Params) -> bool {
    if q.contains_key("has") && v["security_vulnerability"]["first_patched_version"].is_null() {
        return false;
    }
    [
        ("state", v["state"].as_str()),
        ("severity", v["security_advisory"]["severity"].as_str()),
        (
            "classification",
            v["security_advisory"]["classification"].as_str(),
        ),
        (
            "ecosystem",
            v["dependency"]["package"]["ecosystem"].as_str(),
        ),
        ("package", v["dependency"]["package"]["name"].as_str()),
        ("manifest", v["dependency"]["manifest_path"].as_str()),
        ("scope", v["dependency"]["scope"].as_str()),
    ]
    .into_iter()
    .all(|(key, value)| csv(q, key, value.unwrap_or("")))
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(query): Query<Params>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    validate(&query, false)?;
    let org = params.get("org");
    let (ids, actor) = if let Some(org) = org {
        let actor = auth::authenticate(&state, &headers, false)
            .await?
            .ok_or_else(auth::required)?;
        let org = orgs::target(&state, org).await?;
        let ids = store::organization_repositories(&state.pool, actor.user.id, org.id)
            .await
            .map_err(failure)?;
        (ids, actor)
    } else {
        let (repo, actor) =
            repository(&state, &headers, &params["owner"], &params["repo"], false).await?;
        (vec![repo.id], actor.ok_or_else(auth::required)?)
    };
    let database_actor = actor.database_actor();
    let mut documents = Vec::new();
    for id in ids {
        let rows = match store::alerts(&state.pool, database_actor.clone(), id).await {
            Ok(rows) => rows,
            // A repository with alerts disabled is left out of an organization listing.
            Err(error)
                if org.is_some()
                    && matches!(
                        error.downcast_ref::<db::advisories::Refusal>(),
                        Some(db::advisories::Refusal::Missing)
                    ) =>
            {
                continue
            }
            Err(error) => return Err(failure(error)),
        };
        let repo = db::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let owner = db::repositories::records::repository_owner_name(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let mut repository_document = None;
        for row in rows {
            let mut doc = document(&state, &row, &owner, &repo.name).await?;
            if !alert_matches(&doc, &query) {
                continue;
            }
            if org.is_some() {
                if repository_document.is_none() {
                    repository_document = Some(repos::document(&state, &repo, Some(&actor)).await?);
                }
                doc["repository"] = repository_document.as_ref().unwrap().clone();
            }
            documents.push(doc);
        }
    }
    page(documents, &query, &uri, false)
}
async fn detail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let row = store::alerts(&state.pool, actor.database_actor(), repo.id)
        .await
        .map_err(failure)?
        .into_iter()
        .find(|r| r.number == number)
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&state, &row, &owner, &name).await?))
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Json(input): Json<Update>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let row = store::update(&state.pool, actor.database_actor(), repo.id, number, &input)
        .await
        .map_err(failure)?;
    Ok(Json(document(&state, &row, &owner, &name).await?))
}
