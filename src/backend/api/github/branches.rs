//! Branch reads use the same protection matcher as pushes.
use super::{contents::store_error, pagination::Pagination, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    ci::workflows::WorkflowMatchBudget,
    db,
    repositories::{protection, store::repo_store},
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Deserialize)]
struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    protected: Option<bool>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let rules = protection::load_rules(&state.pool, &repo)
        .await
        .map_err(Error::unavailable)?;
    let refs = repo_store(&repo, state.cluster.as_ref())
        .refs()
        .await
        .map_err(store_error)?;
    let mut refs: Vec<_> = refs
        .into_iter()
        .filter_map(|(reference, sha)| {
            reference
                .strip_prefix("refs/heads/")
                .map(|branch| (branch.to_owned(), sha))
        })
        .collect();
    refs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut has_next = refs.len() > offset + limit;
    if query.protected.is_none() {
        refs = refs.into_iter().skip(offset).take(limit).collect();
    }
    let mut branches = Vec::new();
    let mut budget = WorkflowMatchBudget::new();
    for (branch, sha) in refs {
        let protected = protection::branch_is_protected(&rules, &branch, &mut budget)
            .map_err(Error::unavailable)?;
        if query.protected.is_none_or(|filter| filter == protected) {
            branches.push((branch, sha, protected));
        }
    }
    if query.protected.is_some() {
        has_next = branches.len() > offset + limit;
        branches = branches.into_iter().skip(offset).take(limit).collect();
    }
    let values = branches.into_iter().map(|(branch, sha, protected)| json!({
        "name": branch, "commit": {"sha": sha, "url": format!("{}/commits/{sha}", repo_url(&owner, &name))},
        "protected": protected,
    })).collect();
    page.response(&uri, values, has_next)
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, branch)): Path<(String, String, String)>,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let refs = repo_store(&repo, state.cluster.as_ref())
        .refs()
        .await
        .map_err(store_error)?;
    if !refs.contains_key(&format!("refs/heads/{branch}")) {
        if let Some(target) =
            db::repositories::protection::branch_redirect(&state.pool, repo.id, &branch)
                .await
                .map_err(Error::unavailable)?
        {
            if refs.contains_key(&format!("refs/heads/{target}")) {
                let url = format!(
                    "{}/branches/{}",
                    repo_url(&owner, &name),
                    codeyard_webui::encode_component(&target)
                );
                return Ok((
                    StatusCode::MOVED_PERMANENTLY,
                    [(header::LOCATION, url)],
                    Json(json!({"message":"Moved Permanently"})),
                )
                    .into_response());
            }
        }
        return Err(Error::missing());
    }
    Ok(Json(document(&state, &repo, &owner, &name, &branch).await?).into_response())
}

pub(super) async fn document(
    state: &AppState,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<Value> {
    let store = repo_store(repo, state.cluster.as_ref());
    let refs = store.refs().await.map_err(store_error)?;
    let sha = refs
        .get(&format!("refs/heads/{branch}"))
        .ok_or_else(Error::missing)?;
    let commit = store.commit(sha).await.map_err(store_error)?;
    let rules = protection::matching_rules(&state.pool, repo, branch)
        .await
        .map_err(Error::unavailable)?;
    let protected = !rules.is_empty();
    let url = format!(
        "{}/branches/{}",
        repo_url(owner, name),
        codeyard_webui::encode_component(branch)
    );
    let mut contexts: Vec<_> = rules
        .iter()
        .filter(|rule| rule.require_status_checks)
        .flat_map(|rule| rule.required_status_checks.clone())
        .collect();
    contexts.sort();
    contexts.dedup();
    let mut checks: Vec<_> = rules
        .iter()
        .filter(|rule| rule.require_status_checks)
        .flat_map(super::protection::check_documents)
        .collect();
    checks.sort_by(|a, b| {
        a["context"]
            .as_str()
            .cmp(&b["context"].as_str())
            .then(a["app_id"].as_i64().cmp(&b["app_id"].as_i64()))
    });
    checks.dedup();
    Ok(json!({
        "name": branch, "commit": super::commits::read_document(state, repo.id, &commit, owner, name).await?,
        "protected": protected, "protection_url": format!("{url}/protection"),
        "protection": {"enabled": protected, "required_status_checks": {
            "enforcement_level": if rules.iter().any(|rule| rule.enforce_admins) { "everyone" } else if protected { "non_admins" } else { "off" },
            "contexts": contexts,
            "checks": checks,
        }},
        "_links": {"self": url, "html": format!("{}/{owner}/{name}/tree/{}", crate::backend::api::http::config::link_origin(), codeyard_webui::encode_component(branch))},
    }))
}

async fn heads(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let (offset, limit) = page.window()?;
    if !crate::backend::git::is_full_commit_id(&sha) {
        return Err(Error::invalid("sha must be a full commit ID"));
    }
    let store = repo_store(&repo, state.cluster.as_ref());
    let sha = store.commit(&sha).await.map_err(store_error)?.sha;
    let refs = store.refs().await.map_err(store_error)?;
    let rules = protection::load_rules(&state.pool, &repo)
        .await
        .map_err(Error::unavailable)?;
    let mut matches: Vec<_> = refs
        .into_iter()
        .filter_map(|(reference, tip)| {
            reference
                .strip_prefix("refs/heads/")
                .filter(|_| tip == sha)
                .map(str::to_owned)
        })
        .collect();
    matches.sort();
    let has_next = matches.len() > offset + limit;
    let mut values = Vec::new();
    let mut budget = WorkflowMatchBudget::new();
    for branch in matches.into_iter().skip(offset).take(limit) {
        let protected = protection::branch_is_protected(&rules, &branch, &mut budget)
            .map_err(Error::unavailable)?;
        values.push(json!({"name": branch, "commit": {"sha": sha, "url": format!("{}/commits/{sha}", repo_url(&owner, &name))}, "protected": protected}));
    }
    page.response(&uri, values, has_next)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-branches", get(list)),
        ("repos/get-branch", get(read)),
        ("repos/list-branches-for-head-commit", get(heads)),
    ]
}
