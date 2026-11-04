//! Artifact inventory writes resolve the same repository-scoped provenance as attestation reads.
use axum::response::IntoResponse;

use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_repos::artifact_metadata::Actor;
use codeyard_db_repos::artifact_metadata::ClusterInput;
use codeyard_db_repos::artifact_metadata::DeploymentInput;
use codeyard_db_repos::artifact_metadata::Mutation;
use codeyard_db_repos::artifact_metadata::Refusal;
use codeyard_db_repos::artifact_metadata::StorageInput;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::de::DeserializeOwned;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => Error::new(
            StatusCode::FORBIDDEN,
            "Artifact metadata write permission required",
        ),
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Invalid) => Error::validation("ArtifactMetadata", "record", "invalid"),
        Some(Refusal::Busy) => Error::new(
            StatusCode::CONFLICT,
            "A job is already in progress for this cluster",
        ),
        Some(Refusal::TooLarge) => Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Artifact inventory is too large for one response",
        ),
        None => Error::unavailable(error),
    }
}

async fn actor(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    write: bool,
) -> Result<(Actor, i64)> {
    let auth = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    if write {
        auth.require_write()?;
    }
    let org = crate::orgs::target(state, &params["org"]).await?;
    let repo = if auth.user.username == codeyard_db_core::queries::ci_tokens::BOT {
        let job = codeyard_db_actions::ci_tokens::get(&state.pool, auth.token.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(codeyard_identity::github_auth::required)?;
        let permissions: codeyard_model::permissions::Permissions =
            serde_json::from_str(&job.permissions).map_err(Error::unavailable)?;
        if !permissions.allows("artifact-metadata", write) {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "Resource not accessible by token",
            ));
        }
        let repo = codeyard_db_repos::repositories::records::get_repository_by_id(
            &state.pool,
            job.repo_id,
        )
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        if repo.owner_org_id != Some(org.id) {
            return Err(Error::missing());
        }
        Some(repo.id)
    } else {
        None
    };
    Ok((
        Actor {
            user: auth.user.id,
            token: auth.token.id,
            repo,
        },
        org.id,
    ))
}

async fn body<T: DeserializeOwned>(request: Request) -> Result<T> {
    let bytes = axum::body::to_bytes(request.into_body(), 16 * 1024 * 1024)
        .await
        .map_err(|_| Error::validation("ArtifactMetadata", "body", "invalid"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::validation("ArtifactMetadata", "body", "invalid"))
}

fn mutation(
    mutation: Mutation,
    kind: &str,
    return_records: bool,
    partial: bool,
) -> Result<Response> {
    let mut value = json!({"total_count":mutation.records.len()});
    if return_records {
        let records = mutation
            .records
            .iter()
            .map(|row| row.document())
            .collect::<anyhow::Result<Vec<_>>>()
            .map_err(Error::unavailable)?;
        value[format!("{kind}_records")] = json!(records);
    }
    if partial {
        value["errors"] = json!(mutation.errors);
    }
    let status = if partial && !mutation.errors.is_empty() {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::OK
    };
    Ok((status, Json(value)).into_response())
}

async fn deployment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let (actor, org) = actor(&state, &headers, &params, true).await?;
    let input: Value = body(request).await?;
    if !input.get("status").is_some_and(Value::is_string) {
        return Err(Error::validation(
            "ArtifactMetadata",
            "status",
            "missing_field",
        ));
    }
    let input: DeploymentInput = serde_json::from_value(input)
        .map_err(|_| Error::validation("ArtifactMetadata", "record", "invalid"))?;
    let rows = codeyard_db_repos::artifact_metadata::deployment(&state.pool, actor, org, &input)
        .await
        .map_err(failure)?;
    mutation(rows, "deployment", input.return_records, false)
}

async fn storage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let (actor, org) = actor(&state, &headers, &params, true).await?;
    let input: StorageInput = body(request).await?;
    let rows = codeyard_db_repos::artifact_metadata::storage(&state.pool, actor, org, &input)
        .await
        .map_err(failure)?;
    mutation(rows, "storage", input.return_records, false)
}

async fn cluster(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let (actor, org) = actor(&state, &headers, &params, true).await?;
    let input: ClusterInput = body(request).await?;
    let rows = codeyard_db_repos::artifact_metadata::cluster(
        &state.pool,
        actor,
        org,
        &params["cluster"],
        &input,
    )
    .await
    .map_err(failure)?;
    mutation(
        rows,
        "deployment",
        input.return_records,
        input.partial_success,
    )
}

async fn enqueue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    request: Request,
) -> Result<Response> {
    let (actor, org) = actor(&state, &headers, &params, true).await?;
    let input: ClusterInput = body(request).await?;
    let (id, errors) = codeyard_db_repos::artifact_metadata::enqueue(
        &state.pool,
        actor,
        org,
        &params["cluster"],
        &input,
    )
    .await
    .map_err(failure)?;
    let mut value = json!({"job_id":id});
    if !errors.is_empty() {
        value["errors"] = json!(errors);
    }
    Ok((StatusCode::ACCEPTED, Json(value)).into_response())
}

async fn job(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Response> {
    let (actor, org) = actor(&state, &headers, &params, false).await?;
    let id = params["job_id"]
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)?;
    let job =
        codeyard_db_repos::artifact_metadata::job(&state.pool, actor, org, &params["cluster"], id)
            .await
            .map_err(failure)?
            .ok_or_else(Error::missing)?;
    let mut value = json!({"job_id":job.id,"status":job.status});
    if let Some(started) = job.started_at {
        value["started_at"] = json!(started);
    }
    if let Some(total) = job.total_count {
        value["total_count"] = json!(total);
    }
    if matches!(job.status.as_str(), "completed" | "failed") {
        value["errors"] = serde_json::from_str(&job.errors).map_err(Error::unavailable)?;
    }
    Ok(Json(value).into_response())
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, org) = actor(&state, &headers, &params, false).await?;
    let digest = &params["subject_digest"];
    if !digest.starts_with("sha256:")
        || !codeyard_db_core::queries::attestations::valid_digest(digest)
    {
        return Err(Error::validation(
            "ArtifactMetadata",
            "subject_digest",
            "invalid",
        ));
    }
    let kind = if uri.path().ends_with("/storage-records") {
        "storage"
    } else {
        "deployment"
    };
    let rows = codeyard_db_repos::artifact_metadata::list(&state.pool, actor, org, digest, kind)
        .await
        .map_err(failure)?;
    mutation(
        Mutation {
            records: rows,
            errors: vec![],
        },
        kind,
        true,
        false,
    )
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/create-artifact-deployment-record", post(deployment)),
        ("orgs/set-cluster-deployment-records", post(cluster)),
        ("orgs/create-cluster-deployment-records-job", post(enqueue)),
        ("orgs/get-cluster-deployment-records-job", get(job)),
        ("orgs/create-artifact-storage-record", post(storage)),
        ("orgs/list-artifact-deployment-records", get(list)),
        ("orgs/list-artifact-storage-records", get(list)),
    ]
}
