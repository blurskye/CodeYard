use super::{
    auth, contents::store_error, pagination::Pagination, repo_url, repository, Error, Result,
};
use crate::backend::{app::AppState, db, dependency_graph, repositories::store::repo_store};
use axum::{
    extract::{rejection::JsonRejection, OriginalUri, Path, Query, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
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

fn error(error: db::dependency_graph::GraphError) -> Error {
    use db::dependency_graph::GraphError;
    match error {
        GraphError::Denied => Error::new(
            StatusCode::FORBIDDEN,
            "Must have write access to this repository",
        ),
        GraphError::Invalid => Error::validation("Snapshot", "snapshot", "invalid"),
        _ => Error::unavailable(error),
    }
}

async fn submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    body: std::result::Result<Json<dependency_graph::Snapshot>, JsonRejection>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    let Json(mut snapshot) =
        body.map_err(|_| Error::validation("Snapshot", "snapshot", "invalid"))?;
    snapshot.validate().map_err(error)?;
    // Only Codeyard's own scan saves under its detector and job, so alerts follow real scans.
    if crate::backend::repositories::dependency_graph::is_native(&snapshot) {
        return Err(Error::validation("Snapshot", "detector", "invalid"));
    }
    // A submission may describe an older commit, but never an object outside this repository.
    snapshot.sha = repo_store(&repo, state.cluster.as_ref())
        .commit(&snapshot.sha)
        .await
        .map_err(store_error)?
        .sha;
    let submission = db::dependency_graph::submit(&state.pool, actor.database_actor(&headers)?, repo.id, &snapshot)
        .await
        .map_err(error)?;
    db::vulnerabilities::enqueue(&state.pool, repo.id)
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
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let (base, head) = range
        .split_once("...")
        .filter(|(base, head)| !base.is_empty() && !head.is_empty())
        .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "Invalid comparison"))?;
    let store = repo_store(&repo, state.cluster.as_ref());
    let comparison = store
        .comparison_from(&store, base, head, 0, 1)
        .await
        .map_err(store_error)?;
    let mut base = db::dependency_graph::snapshots(
        &state.pool,
        repo.id,
        None,
        Some(&comparison.merge_base_commit.sha),
    )
    .await
    .map_err(error)?;
    let mut head =
        db::dependency_graph::snapshots(&state.pool, repo.id, None, Some(&comparison.head_sha))
            .await
            .map_err(error)?;
    append_inventory(&state, &repo, &comparison.merge_base_commit.sha, &mut base).await?;
    append_inventory(&state, &repo, &comparison.head_sha, &mut head).await?;
    let changes = dependency_graph::diff(&base, &head, query.name.as_deref());
    let page = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (offset, limit) = page.window()?;
    let has_more = offset.saturating_add(limit) < changes.len();
    // Only the requested page gets vulnerability lookups.
    let mut changes: Vec<_> = changes.into_iter().skip(offset).take(limit).collect();
    for change in &mut changes {
        let package = crate::backend::repositories::dependency_graph::Package {
            manifest: change["manifest"].as_str().unwrap_or_default().into(),
            ecosystem: change["ecosystem"].as_str().unwrap_or_default().into(),
            name: change["name"].as_str().unwrap_or_default().into(),
            version: change["version"].as_str().unwrap_or_default().into(),
            package_url: change["package_url"].as_str().unwrap_or_default().into(),
            scope: change["scope"].as_str().unwrap_or("unknown").into(),
        };
        change["vulnerabilities"] = json!(crate::backend::vulnerabilities::findings(&state.pool,&package)
            .await.map_err(Error::unavailable)?.iter().map(|a|json!({
                "severity":a.severity(),
                "advisory_ghsa_id":a.aliases.iter().chain(std::iter::once(&a.id)).find(|id|id.starts_with("GHSA-")).unwrap_or(&a.id),
                "advisory_summary":a.summary,
                "advisory_url":format!("https://osv.dev/vulnerability/{}",crate::webui::encode_component(&a.id)),
            })).collect::<Vec<_>>());
    }
    Ok((page.headers(&uri, has_more)?, Json(changes)).into_response())
}

// Feed pinned native observations through the same typed SPDX and comparison projections as
// submitted detector snapshots, retaining the latter's dependency relationships and metadata.
async fn append_inventory(
    state: &AppState,
    repo: &db::models::Repository,
    reference: &str,
    snapshots: &mut Vec<dependency_graph::Snapshot>,
) -> Result<()> {
    use crate::backend::repositories::{dependency_graph as inventory, store::ErrorKind};
    let (sha, packages) = match inventory::native_inventory(state, repo, reference).await {
        Ok(inventory) => inventory,
        // Submitted detector snapshots also support repositories with no native commits yet.
        Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath) => {
            return Ok(())
        }
        Err(error) => return Err(store_error(error)),
    };
    let mut manifests = std::collections::BTreeMap::new();
    for package in packages {
        let manifest = manifests
            .entry(package.manifest.clone())
            .or_insert_with(|| dependency_graph::Manifest {
                name: package.manifest.clone(),
                file: None,
                metadata: None,
                resolved: Default::default(),
            });
        manifest.resolved.insert(
            package.package_url.clone(),
            dependency_graph::Dependency {
                package_url: Some(package.package_url),
                metadata: None,
                relationship: None,
                scope: Some(package.scope),
                dependencies: Vec::new(),
            },
        );
    }
    snapshots.push(dependency_graph::Snapshot {
        version: 0,
        sha,
        reference: reference.to_owned(),
        job: dependency_graph::Job {
            id: "native-inventory".into(),
            correlator: "native-inventory".into(),
            html_url: None,
        },
        detector: dependency_graph::Detector {
            name: "Codeyard native inventory".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            url: crate::backend::api::http::config::link_origin(),
        },
        scanned: chrono::Utc::now(),
        metadata: None,
        manifests,
    });
    Ok(())
}

async fn current(state: &AppState, repo: &db::models::Repository, owner: &str) -> Result<Value> {
    let (refs, branch) = repo_store(repo, state.cluster.as_ref())
        .refs_and_default()
        .await
        .map_err(store_error)?;
    let reference = format!("refs/heads/{branch}");
    // One commit for both the saved snapshots and the live inventory, so a branch that moves
    // mid-request cannot mix versions.
    let Some(sha) = refs.get(&reference).cloned() else {
        return Ok(dependency_graph::sbom(
            &format!("{owner}/{}", repo.name),
            &[],
        ));
    };
    let mut snapshots =
        db::dependency_graph::snapshots(&state.pool, repo.id, Some(&reference), Some(&sha))
            .await
            .map_err(error)?;
    append_inventory(state, repo, &sha, &mut snapshots).await?;
    Ok(dependency_graph::sbom(
        &format!("{owner}/{}", repo.name),
        &snapshots,
    ))
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
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let value = if let Some(id) = query.report {
        let body = db::dependency_graph::report(&state.pool, repo.id, &id)
            .await
            .map_err(error)?
            .ok_or_else(Error::missing)?;
        serde_json::from_str(&body).map_err(Error::unavailable)?
    } else {
        current(&state, &repo, &owner).await?
    };
    Ok(Json(value))
}

async fn generate(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if method == Method::HEAD {
        return Ok(StatusCode::CREATED.into_response());
    }
    let body = current(&state, &repo, &owner).await?;
    let id = uuid::Uuid::new_v4().to_string();
    db::dependency_graph::save_report(&state.pool, repo.id, &id, &body.to_string())
        .await
        .map_err(error)?;
    Ok((StatusCode::CREATED, Json(json!({"sbom_url":format!("{}/dependency-graph/sbom/fetch-report/{id}", repo_url(&owner, &name))}))).into_response())
}

async fn fetch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, String)>,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if db::dependency_graph::report(&state.pool, repo.id, &id)
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
                crate::webui::encode_component(&id)
            ),
        )],
    )
        .into_response())
}
