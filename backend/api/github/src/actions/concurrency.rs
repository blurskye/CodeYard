//! Concurrency reads expose live leases and retained workflow configuration separately.
use crate::actions::run;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

#[derive(Default, Deserialize)]
struct Page {
    per_page: Option<usize>,
    after: Option<String>,
    before: Option<String>,
}
fn decode(value: &str) -> Result<String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| Error::invalid("Invalid concurrency cursor"))?;
    let text =
        String::from_utf8(bytes).map_err(|_| Error::invalid("Invalid concurrency cursor"))?;
    text.strip_prefix("concurrency:v1:")
        .map(str::to_owned)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::invalid("Invalid concurrency cursor"))
}
fn page(
    groups: Vec<codeyard_db_actions::action_concurrency::Group>,
    query: &Page,
    uri: &Uri,
) -> Result<(
    HeaderMap,
    Vec<codeyard_db_actions::action_concurrency::Group>,
)> {
    if query.after.is_some() && query.before.is_some() {
        return Err(Error::invalid("Use either before or after"));
    }
    let after = query.after.as_deref().map(decode).transpose()?;
    let before = query.before.as_deref().map(decode).transpose()?;
    let limit = codeyard_api_http::shared::github::pagination::page_size(query.per_page);
    let mut groups: Vec<_> = groups
        .into_iter()
        .filter(|group| {
            after
                .as_ref()
                .is_none_or(|after| group.name.to_lowercase() > *after)
                && before
                    .as_ref()
                    .is_none_or(|before| group.name.to_lowercase() < *before)
        })
        .collect();
    if before.is_some() {
        groups.reverse();
    }
    let more = groups.len() > limit;
    groups.truncate(limit);
    if before.is_some() {
        groups.reverse();
    }
    let mut headers = HeaderMap::new();
    let mut links = Vec::new();
    for (group, key, relation, include) in [
        (
            groups.last(),
            "after",
            "next",
            if before.is_some() { true } else { more },
        ),
        (groups.first(), "before", "prev", before.is_some() && more),
    ] {
        if let Some(group) = group.filter(|_| include) {
            let cursor =
                URL_SAFE_NO_PAD.encode(format!("concurrency:v1:{}", group.name.to_lowercase()));
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair(key, &cursor)
                .append_pair("per_page", &limit.to_string())
                .finish();
            links.push(format!(
                "<{}{}?{}>; rel=\"{}\"",
                codeyard_model::http_config::link_origin(),
                uri.path(),
                query,
                relation
            ));
        }
    }
    if !links.is_empty() {
        headers.insert(
            header::LINK,
            links.join(", ").parse().map_err(Error::unavailable)?,
        );
    }
    Ok((headers, groups))
}
fn group_url(owner: &str, repo: &str, group: &str) -> String {
    format!(
        "{}/actions/concurrency_groups/{}",
        repo_url(owner, repo),
        codeyard_types::paths::encode_component(group)
    )
}
fn member(
    member: &codeyard_db_actions::action_concurrency::Member,
    owner: &str,
    repo: &str,
) -> Value {
    let run = &member.run;
    let id = run.workflow_run_id.unwrap_or(run.id);
    let mut value = json!({"run_id":id,"run_name":run.pipeline,"run_url":format!("{}/actions/runs/{id}",repo_url(owner,repo)),"run_html_url":format!("{}/{}/{}/actions?job={}",codeyard_model::http_config::link_origin(),codeyard_types::paths::encode_component(owner),codeyard_types::paths::encode_component(repo),run.run_id),"status":if member.holding{"in_progress"}else{"pending"}});
    if let Some(job) = member.job_id {
        value["job_id"] = json!(job);
        value["job_name"] = json!(member.job_name);
        value["job_url"] = json!(format!("{}/actions/jobs/{job}", repo_url(owner, repo)));
        value["job_html_url"] = value["run_html_url"].clone();
    }
    value
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Page>,
    OriginalUri(uri): OriginalUri,
) -> Result<(HeaderMap, Json<Value>)> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if query.before.is_some() {
        return Err(Error::invalid(
            "Use after for repository concurrency groups",
        ));
    }
    let groups = codeyard_db_actions::action_concurrency::groups(&state.pool, repo.id, None)
        .await
        .map_err(Error::unavailable)?;
    let total = groups.len();
    let (headers, groups) = page(groups, &query, &uri)?;
    Ok((
        headers,
        Json(
            json!({"total_count":total,"concurrency_groups":groups.into_iter().map(|group|json!({"group_name":group.name,"group_url":group_url(&owner,&name,&group.name),"last_acquired_at":group.last_acquired_at})).collect::<Vec<_>>()}),
        ),
    ))
}
#[derive(Default, Deserialize)]
struct Ahead {
    ahead_of_run: Option<i64>,
    ahead_of_job: Option<i64>,
}
async fn get_one(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, group)): Path<(String, String, String)>,
    Query(query): Query<Ahead>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let mut members =
        codeyard_db_actions::action_concurrency::members(&state.pool, repo.id, &group)
            .await
            .map_err(Error::unavailable)?;
    if members.is_empty() {
        return Err(Error::missing());
    }
    if query.ahead_of_job.is_some() && query.ahead_of_run.is_some() {
        return Err(Error::invalid("Use either ahead_of_job or ahead_of_run"));
    }
    if let Some(id) = query.ahead_of_job {
        let position = members
            .iter()
            .position(|member| member.job_id == Some(id))
            .ok_or_else(|| Error::invalid("Job is not in this concurrency group"))?;
        members.truncate(position + 1);
    }
    if let Some(id) = query.ahead_of_run {
        let position = members
            .iter()
            .position(|member| member.run.workflow_run_id.unwrap_or(member.run.id) == id)
            .ok_or_else(|| Error::invalid("Run is not in this concurrency group"))?;
        members.truncate(position + 1);
    }
    Ok(Json(
        json!({"group_name":group,"group_url":group_url(&owner,&name,&group),"total_count":members.len(),"group_members":members.iter().map(|item|member(item,&owner,&name)).collect::<Vec<_>>()}),
    ))
}
async fn for_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(query): Query<Page>,
    OriginalUri(uri): OriginalUri,
) -> Result<(HeaderMap, Json<Value>)> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let run = run(&state, repo.id, id).await?;
    let id = run.workflow_run_id.unwrap_or(run.id);
    let groups = codeyard_db_actions::action_concurrency::groups(&state.pool, repo.id, Some(id))
        .await
        .map_err(Error::unavailable)?;
    let total = groups.len();
    let (headers, groups) = page(groups, &query, &uri)?;
    let mut values = Vec::new();
    for group in groups {
        let url = group_url(&owner, &name, &group.name);
        let members =
            codeyard_db_actions::action_concurrency::members(&state.pool, repo.id, &group.name)
                .await
                .map_err(Error::unavailable)?;
        let values_in_group: Vec<_> = members
            .iter()
            .enumerate()
            .filter(|(_, member)| member.run.workflow_run_id.unwrap_or(member.run.id) == id)
            .map(|(index, item)| {
                let mut value = member(item, &owner, &name);
                value["position"] = json!(if item.holding { 0 } else { index.max(1) });
                value["position_url"] = json!(match item.job_id {
                    Some(job) => format!("{url}?ahead_of_job={job}"),
                    None => format!("{url}?ahead_of_run={id}"),
                });
                value
            })
            .collect();
        values
            .push(json!({"group_name":group.name,"group_url":url,"group_members":values_in_group}));
    }
    Ok((
        headers,
        Json(json!({"total_count":total,"concurrency_groups":values})),
    ))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/list-concurrency-groups-for-repository", get(list)),
        ("actions/get-concurrency-group-for-repository", get(get_one)),
        (
            "actions/list-concurrency-groups-for-workflow-run",
            get(for_run),
        ),
    ]
}
