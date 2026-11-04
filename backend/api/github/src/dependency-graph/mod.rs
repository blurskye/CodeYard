use axum::extract::rejection::JsonRejection;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("dependency-graph/create-repository-snapshot", post(submit)),
        ("dependency-graph/diff-range", get(compare)),
        ("dependency-graph/export-sbom", get(export)),
        ("dependency-graph/generate-sbom-report", get(generate)),
        ("dependency-graph/fetch-sbom-report", get(fetch)),
    ]
}

fn error(error: codeyard_model::dependency_graph::GraphError) -> Error {
    use codeyard_model::dependency_graph::GraphError;
    match error {
        GraphError::Denied => Error::new(StatusCode::FORBIDDEN, GraphError::Denied.to_string()),
        GraphError::Invalid => Error::validation("Snapshot", "snapshot", "invalid"),
        error @ (GraphError::Limit | GraphError::Disabled) => {
            Error::new(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
        }
        _ => Error::unavailable(error),
    }
}

async fn readable(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<(
    codeyard_db_core::models::Repository,
    codeyard_db_security::dependency_graph::Reader,
)> {
    let (repo, authentication) = repository(state, headers, owner, name, false).await?;
    let actor = authentication.as_ref().map(|auth| auth.database_actor());
    Ok((
        repo,
        codeyard_db_security::dependency_graph::Reader::Viewer(actor),
    ))
}

async fn submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    body: std::result::Result<Json<codeyard_model::dependency_graph::Snapshot>, JsonRejection>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(codeyard_identity::github_auth::required)?;
    let Json(mut snapshot) =
        body.map_err(|_| Error::validation("Snapshot", "snapshot", "invalid"))?;
    snapshot.validate().map_err(error)?;
    // Only Codeyard's own scan saves under its detector and job, so alerts follow real scans.
    if codeyard_repos_pulls::repositories::dependency_graph::is_native(&snapshot) {
        return Err(Error::validation("Snapshot", "detector", "invalid"));
    }
    // A submission may describe an older commit, but never an object outside this repository.
    snapshot.sha = repo_store(&repo, state.cluster.as_ref())
        .commit(&snapshot.sha)
        .await
        .map_err(store_error)?
        .sha;
    let submission = codeyard_db_security::dependency_graph::submit(
        &state.pool,
        actor.database_actor(),
        repo.id,
        &snapshot,
    )
    .await
    .map_err(error)?;
    codeyard_db_security::vulnerabilities::enqueue(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    Ok((StatusCode::CREATED, Json(json!({
        "id":submission.id, "created_at":submission.created_at,
        "result":if submission.updated { "SUCCESS" } else { "ACCEPTED" },
        "message":if submission.updated { "Snapshot created and dependencies updated" } else { "Snapshot created; a newer scan already exists" }
    }))).into_response())
}

#[derive(Deserialize)]
struct Compare {
    name: Option<String>,
    page: Option<usize>,
    per_page: Option<usize>,
}

async fn compare(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, range)): Path<(String, String, String)>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<Compare>,
) -> Result<Response> {
    let (repo, reader) = readable(&state, &headers, &owner, &name).await?;
    let (base, head) = range
        .split_once("...")
        .filter(|(base, head)| !base.is_empty() && !head.is_empty())
        .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "Invalid comparison"))?;
    codeyard_db_security::dependency_graph::check_read(&state.pool, &reader, repo.id)
        .await
        .map_err(error)?;
    let store = repo_store(&repo, state.cluster.as_ref());
    let comparison = store
        .comparison_from(&store, base, head, 0, 1)
        .await
        .map_err(store_error)?;
    let mut base = codeyard_db_security::dependency_graph::snapshots(
        &state.pool,
        &reader,
        repo.id,
        None,
        Some(&comparison.merge_base_commit.sha),
    )
    .await
    .map_err(error)?;
    let mut head = codeyard_db_security::dependency_graph::snapshots(
        &state.pool,
        &reader,
        repo.id,
        None,
        Some(&comparison.head_sha),
    )
    .await
    .map_err(error)?;
    for (sha, snapshots) in [
        (&comparison.merge_base_commit.sha, &mut base),
        (&comparison.head_sha, &mut head),
    ] {
        codeyard_repos_pulls::repositories::dependency_graph::append_native_snapshot(
            &state, &reader, &repo, sha, None, snapshots,
        )
        .await
        .map_err(store_error)?;
    }
    let changes = codeyard_model::dependency_graph::diff(&base, &head, query.name.as_deref());
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let has_more = offset.saturating_add(limit) < changes.len();
    // Only the requested page gets vulnerability lookups.
    let mut changes: Vec<_> = changes.into_iter().skip(offset).take(limit).collect();
    let local = if changes
        .iter()
        .any(|change| change["version"].as_str().is_some_and(|v| !v.is_empty()))
    {
        codeyard_db_security::dependabot_alerts::without_osv(&state.pool)
            .await
            .map_err(Error::unavailable)?
    } else {
        Vec::new()
    };
    for change in &mut changes {
        let package = codeyard_model::repositories::Package {
            manifest: change["manifest"].as_str().unwrap_or_default().into(),
            ecosystem: change["ecosystem"].as_str().unwrap_or_default().into(),
            name: change["name"].as_str().unwrap_or_default().into(),
            version: change["version"].as_str().unwrap_or_default().into(),
            package_url: change["package_url"].as_str().unwrap_or_default().into(),
            scope: change["scope"].as_str().unwrap_or("unknown").into(),
        };
        if package.version.is_empty() {
            change["vulnerabilities"] = json!([]);
            continue;
        }
        let mut vulnerabilities = codeyard_repos_pulls::vulnerabilities::findings(&state.pool,&package)
            .await.map_err(Error::unavailable)?.iter().map(|a|json!({
                "severity":a.severity(),
                "advisory_ghsa_id":a.catalogue_id(),
                "advisory_summary":a.summary,
                "advisory_url":format!("https://osv.dev/vulnerability/{}",codeyard_webui::encode_component(&a.id)),
            })).collect::<Vec<_>>();
        if let Some(ecosystem) = codeyard_model::vulnerabilities::ecosystem(&package.ecosystem) {
            for advisory in &local {
                if !advisory["withdrawn_at"].is_null() {
                    continue;
                }
                let affected = advisory["vulnerabilities"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|entry| {
                        codeyard_model::vulnerabilities::ecosystem(
                            entry["package"]["ecosystem"].as_str().unwrap_or(""),
                        ) == Some(ecosystem)
                            && codeyard_model::vulnerabilities::name(
                                ecosystem,
                                entry["package"]["name"].as_str().unwrap_or(""),
                            ) == codeyard_model::vulnerabilities::name(ecosystem, &package.name)
                            && codeyard_model::vulnerabilities::in_range(
                                ecosystem,
                                &package.version,
                                entry["vulnerable_version_range"].as_str().unwrap_or(""),
                            )
                    });
                if affected {
                    vulnerabilities.push(json!({"severity":advisory["severity"],"advisory_ghsa_id":advisory["ghsa_id"],"advisory_summary":advisory["summary"],"advisory_url":codeyard_model::urls::api_url(&format!("/advisories/{}", advisory["ghsa_id"].as_str().unwrap_or("")))}));
                }
            }
        }
        change["vulnerabilities"] = json!(vulnerabilities);
    }
    codeyard_db_security::dependency_graph::check_read(&state.pool, &reader, repo.id)
        .await
        .map_err(error)?;
    Ok((page.headers(&uri, has_more)?, Json(changes)).into_response())
}

async fn current(
    state: &AppState,
    reader: &codeyard_db_security::dependency_graph::Reader,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
) -> Result<Value> {
    codeyard_db_security::dependency_graph::check_read(&state.pool, reader, repo.id)
        .await
        .map_err(error)?;
    let (refs, branch) = repo_store(repo, state.cluster.as_ref())
        .refs_and_default()
        .await
        .map_err(store_error)?;
    let reference = format!("refs/heads/{branch}");
    // One commit for both the saved snapshots and the live inventory, so a branch that moves
    // mid-request cannot mix versions.
    let Some(sha) = refs.get(&reference).cloned() else {
        let value = codeyard_model::dependency_graph::sbom(&format!("{owner}/{}", repo.name), &[]);
        codeyard_db_security::dependency_graph::check_read(&state.pool, reader, repo.id)
            .await
            .map_err(error)?;
        return Ok(value);
    };
    let mut snapshots = codeyard_db_security::dependency_graph::snapshots(
        &state.pool,
        reader,
        repo.id,
        Some(&reference),
        Some(&sha),
    )
    .await
    .map_err(error)?;
    // The native scan keeps the branch that selected the submitted snapshots, so snapshots of
    // other branches at the same commit neither hide nor join its manifests.
    codeyard_repos_pulls::repositories::dependency_graph::append_native_snapshot(
        state,
        reader,
        repo,
        &reference,
        Some(&sha),
        &mut snapshots,
    )
    .await
    .map_err(store_error)?;
    let value =
        codeyard_model::dependency_graph::sbom(&format!("{owner}/{}", repo.name), &snapshots);
    codeyard_db_security::dependency_graph::check_read(&state.pool, reader, repo.id)
        .await
        .map_err(error)?;
    Ok(value)
}

#[derive(Deserialize)]
struct Export {
    report: Option<String>,
}

async fn export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Export>,
) -> Result<Json<Value>> {
    let (repo, reader) = readable(&state, &headers, &owner, &name).await?;
    let value = if let Some(id) = query.report {
        let body =
            codeyard_db_security::dependency_graph::report(&state.pool, &reader, repo.id, &id)
                .await
                .map_err(error)?
                .ok_or_else(Error::missing)?;
        serde_json::from_str(&body).map_err(Error::unavailable)?
    } else {
        current(&state, &reader, &repo, &owner).await?
    };
    Ok(Json(value))
}

async fn generate(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, reader) = readable(&state, &headers, &owner, &name).await?;
    codeyard_db_security::dependency_graph::check_read(&state.pool, &reader, repo.id)
        .await
        .map_err(error)?;
    if method == Method::HEAD {
        return Ok(StatusCode::CREATED.into_response());
    }
    let body = current(&state, &reader, &repo, &owner).await?;
    let id = uuid::Uuid::new_v4().to_string();
    codeyard_db_security::dependency_graph::save_report(
        &state.pool,
        &reader,
        repo.id,
        &id,
        &body.to_string(),
    )
    .await
    .map_err(error)?;
    Ok((StatusCode::CREATED, Json(json!({"sbom_url":format!("{}/dependency-graph/sbom/fetch-report/{id}", repo_url(&owner, &name))}))).into_response())
}

async fn fetch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, String)>,
) -> Result<Response> {
    let (repo, reader) = readable(&state, &headers, &owner, &name).await?;
    if codeyard_db_security::dependency_graph::report(&state.pool, &reader, repo.id, &id)
        .await
        .map_err(error)?
        .is_none()
    {
        return Err(Error::missing());
    }
    Ok((
        StatusCode::FOUND,
        [(
            header::LOCATION,
            format!(
                "{}/dependency-graph/sbom?report={}",
                repo_url(&owner, &name),
                codeyard_webui::encode_component(&id)
            ),
        )],
    )
        .into_response())
}
