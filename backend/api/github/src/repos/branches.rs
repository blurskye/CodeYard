//! Branch reads use the same protection matcher as pushes.
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_git_storage::api_shared::github::contents::store_error;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use codeyard_types::urls::repo_url;
use codeyard_types::workflow_glob::WorkflowMatchBudget;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
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
    let rules = codeyard_git_storage::repositories::protection::load_rules(&state.pool, &repo)
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
        let protected = codeyard_git_storage::repositories::protection::branch_is_protected(
            &rules,
            &branch,
            &mut budget,
        )
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
        if let Some(target) = codeyard_db_repos::repositories::protection::branch_redirect(
            &state.pool,
            repo.id,
            &branch,
        )
        .await
        .map_err(Error::unavailable)?
        {
            if refs.contains_key(&format!("refs/heads/{target}")) {
                let url = format!(
                    "{}/branches/{}",
                    repo_url(&owner, &name),
                    codeyard_types::paths::encode_component(&target)
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
    repo: &codeyard_database::models::Repository,
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
    let rules =
        codeyard_git_storage::repositories::protection::matching_rules(&state.pool, repo, branch)
            .await
            .map_err(Error::unavailable)?;
    let protected = !rules.is_empty();
    let url = format!(
        "{}/branches/{}",
        repo_url(owner, name),
        codeyard_types::paths::encode_component(branch)
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
        .flat_map(crate::repos::protection::check_documents)
        .collect();
    checks.sort_by(|a, b| {
        a["context"]
            .as_str()
            .cmp(&b["context"].as_str())
            .then(a["app_id"].as_i64().cmp(&b["app_id"].as_i64()))
    });
    checks.dedup();
    Ok(json!({
        "name": branch, "commit": crate::repos::commits::read_document(state, repo.id, &commit, owner, name).await?,
        "protected": protected, "protection_url": format!("{url}/protection"),
        "protection": {"enabled": protected, "required_status_checks": {
            "enforcement_level": if rules.iter().any(|rule| rule.enforce_admins) { "everyone" } else if protected { "non_admins" } else { "off" },
            "contexts": contexts,
            "checks": checks,
        }},
        "_links": {"self": url, "html": format!("{}/{owner}/{name}/tree/{}", codeyard_types::http_config::link_origin(), codeyard_types::paths::encode_component(branch))},
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
    if !codeyard_git::is_full_commit_id(&sha) {
        return Err(Error::invalid("sha must be a full commit ID"));
    }
    let store = repo_store(&repo, state.cluster.as_ref());
    let sha = store.commit(&sha).await.map_err(store_error)?.sha;
    let refs = store.refs().await.map_err(store_error)?;
    let rules = codeyard_git_storage::repositories::protection::load_rules(&state.pool, &repo)
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
        let protected = codeyard_git_storage::repositories::protection::branch_is_protected(
            &rules,
            &branch,
            &mut budget,
        )
        .map_err(Error::unavailable)?;
        values.push(json!({"name": branch, "commit": {"sha": sha, "url": format!("{}/commits/{sha}", repo_url(&owner, &name))}, "protected": protected}));
    }
    page.response(&uri, values, has_next)
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-branches", get(list)),
        ("repos/get-branch", get(read)),
        ("repos/list-branches-for-head-commit", get(heads)),
    ]
}
