use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_repos::repository_exports as exports;
use codeyard_db_repos::repository_exports::ExportOptions;
use codeyard_db_repos::repository_exports::ExportRefusal;
use codeyard_db_repos::repository_exports::RepositoryExport;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_storage::Download;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::io::ReaderStream;

type Params = HashMap<String, String>;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("migrations/list-for-org", get(list)),
        ("migrations/start-for-org", post(start)),
        ("migrations/get-status-for-org", get(status)),
        ("migrations/download-archive-for-org", get(download)),
        ("migrations/delete-archive-for-org", delete(delete_archive)),
        ("migrations/unlock-repo-for-org", delete(unlock)),
        ("migrations/list-repos-for-org", get(repositories)),
        ("migrations/list-for-authenticated-user", get(list)),
        ("migrations/start-for-authenticated-user", post(start)),
        ("migrations/get-status-for-authenticated-user", get(status)),
        (
            "migrations/get-archive-for-authenticated-user",
            get(download),
        ),
        (
            "migrations/delete-archive-for-authenticated-user",
            delete(delete_archive),
        ),
        (
            "migrations/unlock-repo-for-authenticated-user",
            delete(unlock),
        ),
        (
            "migrations/list-repos-for-authenticated-user",
            get(repositories),
        ),
    ]
}

fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<ExportRefusal>() {
        Some(ExportRefusal::Denied) => {
            Error::new(StatusCode::FORBIDDEN, "Migration ownership denied")
        }
        Some(ExportRefusal::Invalid) => Error::validation("Migration", "repositories", "invalid"),
        Some(ExportRefusal::Locked | ExportRefusal::Busy) => {
            Error::new(StatusCode::CONFLICT, "Migration already in progress")
        }
        Some(ExportRefusal::Archive) => Error::missing(),
        None => Error::unavailable(error),
    }
}

async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
    write: bool,
) -> Result<(codeyard_identity::github_auth::Authentication, Option<i64>)> {
    let actor = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    if write {
        actor.require_write()?;
    }
    let org = match params.get("org") {
        Some(name) => Some(
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?
                .id,
        ),
        None => None,
    };
    Ok((actor, org))
}

fn id(params: &Params) -> Result<i64> {
    params
        .get("migration_id")
        .and_then(|value| value.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)
}

async fn selected(
    state: &AppState,
    actor: &codeyard_identity::github_auth::Authentication,
    row: &RepositoryExport,
) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for id in row.repository_ids().map_err(Error::unavailable)? {
        let Some(repo) =
            codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
        else {
            continue;
        };
        if !codeyard_identity::repository_access::can_read(&state.pool, Some(&actor.user), &repo)
            .await
            .map_err(Error::unavailable)?
        {
            continue;
        }
        values.push(
            codeyard_app::api_shared::github::repos::document(state, &repo, Some(actor)).await?,
        );
    }
    Ok(values)
}

fn exclude_repositories(uri: &axum::http::Uri) -> Result<bool> {
    let mut exclude = false;
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        if key == "exclude" || key == "exclude[]" {
            if value != "repositories" {
                return Err(Error::validation("Migration", "exclude", "invalid"));
            }
            exclude = true;
        }
    }
    Ok(exclude)
}

async fn document(
    state: &AppState,
    actor: &codeyard_identity::github_auth::Authentication,
    row: &RepositoryExport,
    org_name: Option<&str>,
    exclude: bool,
) -> Result<Value> {
    let options = row.options().map_err(Error::unavailable)?;
    let path = match org_name {
        Some(name) => format!(
            "/orgs/{}/migrations/{}",
            codeyard_webui::encode_component(name),
            row.id
        ),
        None => format!("/user/migrations/{}", row.id),
    };
    let expired = matches!(row.state.as_str(), "pending" | "exporting")
        && row.updated_at + 1200 < chrono::Utc::now().timestamp();
    if expired {
        // The response says failed, so the stored state must too, or deletion refuses the row.
        exports::finish(&state.pool, row.id, "failed")
            .await
            .map_err(Error::unavailable)?;
    }
    let time = |seconds| {
        chrono::DateTime::from_timestamp(seconds, 0)
            .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    };
    let owner = if let Some(id) = row.owner_org_id {
        let org = codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(
            &state.pool,
            id,
        )
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
        let mut owner = codeyard_webhooks::payloads::person(&codeyard_webhooks::payloads::Person {
            id,
            login: org.name,
            site_admin: false,
        });
        owner["type"] = json!("Organization");
        owner["node_id"] = json!(codeyard_model::node_id::encode("O", id));
        owner
    } else {
        codeyard_webhooks::payloads::user(&actor.user)
    };
    let mut value = json!({
        "id": row.id, "node_id": codeyard_model::node_id::encode("MIG", row.id), "owner": owner, "guid": row.guid, "state": if expired { "failed" } else { &row.state },
        "lock_repositories": options.lock_repositories, "exclude_metadata": options.exclude_metadata,
        "exclude_git_data": options.exclude_git_data, "exclude_attachments": options.exclude_attachments,
        "exclude_releases": options.exclude_releases, "exclude_owner_projects": options.exclude_owner_projects,
        "org_metadata_only": options.org_metadata_only,
        "url": codeyard_model::urls::api_url(&path), "created_at": time(row.created_at), "updated_at": time(row.updated_at),
        "repositories": if exclude { Vec::new() } else { selected(state, actor, row).await? },
        "exclude": if exclude { vec!["repositories"] } else { Vec::<&str>::new() }
    });
    if row.state == "exported" && !row.archive_deleted {
        value["archive_url"] = json!(codeyard_model::urls::api_url(&format!("{path}/archive")));
    }
    Ok(value)
}

#[derive(Deserialize)]
struct Input {
    repositories: Option<Vec<String>>,
    #[serde(flatten)]
    options: ExportOptions,
    #[serde(default)]
    exclude: Vec<String>,
}

async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Json(input): Json<Input>,
) -> Result<Response> {
    let (actor, org) = scope(&state, &headers, &params, true).await?;
    if input.exclude.iter().any(|field| field != "repositories") {
        return Err(Error::validation("Migration", "exclude", "invalid"));
    }
    let names = input
        .repositories
        .ok_or_else(|| Error::validation("Migration", "repositories", "missing_field"))?;
    if names.len() > 100 {
        return Err(Error::validation("Migration", "repositories", "invalid"));
    }
    let owner = params
        .get("org")
        .map_or(actor.user.username.as_str(), String::as_str);
    let mut ids = Vec::new();
    for name in names {
        let name = if let Some((prefix, name)) = name.split_once('/') {
            if !prefix.eq_ignore_ascii_case(owner) || name.contains('/') {
                return Err(Error::validation("Migration", "repositories", "invalid"));
            }
            name.to_owned()
        } else {
            name
        };
        if name.is_empty() || name.len() > 255 {
            return Err(Error::validation("Migration", "repositories", "invalid"));
        }
        let repo =
            codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, &name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::validation("Migration", "repositories", "invalid"))?;
        ids.push(repo.id);
    }
    ids.sort_unstable();
    let row = exports::create(&state.pool, actor.user.id, org, &ids, &input.options)
        .await
        .map_err(failure)?;
    if !crate::migrations::export_worker::spawn(state.clone(), headers, row.clone()) {
        exports::finish(&state.pool, row.id, "failed")
            .await
            .map_err(Error::unavailable)?;
        return Err(Error::unavailable(()));
    }
    let value = document(
        &state,
        &actor,
        &row,
        params.get("org").map(String::as_str),
        !input.exclude.is_empty(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            value["url"].as_str().unwrap_or_default().to_owned(),
        )],
        Json(value),
    )
        .into_response())
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, org) = scope(&state, &headers, &params, false).await?;
    let (offset, limit) = page.window()?;
    let rows = exports::list(
        &state.pool,
        actor.user.id,
        org,
        limit as i64 + 1,
        offset as i64,
    )
    .await
    .map_err(failure)?;
    let has_next = rows.len() > limit;
    let mut values = Vec::new();
    for row in rows.into_iter().take(limit) {
        values.push(
            document(
                &state,
                &actor,
                &row,
                params.get("org").map(String::as_str),
                exclude_repositories(&uri)?,
            )
            .await?,
        );
    }
    page.response(&uri, values, has_next)
}

async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>> {
    let (actor, org) = scope(&state, &headers, &params, false).await?;
    let row = exports::get(&state.pool, actor.user.id, org, id(&params)?)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    Ok(Json(
        document(
            &state,
            &actor,
            &row,
            params.get("org").map(String::as_str),
            exclude_repositories(&uri)?,
        )
        .await?,
    ))
}

async fn repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (actor, org) = scope(&state, &headers, &params, false).await?;
    let row = exports::get(&state.pool, actor.user.id, org, id(&params)?)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    let (offset, limit) = page.window()?;
    let values = selected(&state, &actor, &row).await?;
    let has_next = values.len() > offset + limit;
    page.response(
        &uri,
        values.into_iter().skip(offset).take(limit).collect(),
        has_next,
    )
}

async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<Response> {
    let (actor, org) = scope(&state, &headers, &params, false).await?;
    let row = exports::get(&state.pool, actor.user.id, org, id(&params)?)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    if !row.archive_available() {
        return Err(Error::missing());
    }
    if exports::by_guid(&state.pool, actor.user.id, &row.guid)
        .await
        .map_err(failure)?
        .is_none()
    {
        return Err(Error::missing());
    }
    Ok((
        StatusCode::FOUND,
        [(
            header::LOCATION,
            codeyard_model::urls::api_url(&format!("/migration-archives/{}", row.guid)),
        )],
    )
        .into_response())
}

pub(crate) async fn archive(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(guid): Path<String>,
    method: axum::http::Method,
) -> Result<Response> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let row = exports::by_guid(&state.pool, actor.user.id, &guid)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    if !row.archive_available() {
        return Err(Error::missing());
    }
    if let Some(org) = row.owner_org_id {
        crate::auth::refuse_classic(&state, actor.token.id, org).await?;
    }
    let filename = format!("migration-{}.tar.gz", row.id);
    if method == axum::http::Method::HEAD {
        return Ok((
            [
                (header::CONTENT_TYPE, "application/gzip".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    codeyard_storage::attachment_disposition(&filename),
                ),
                (header::CACHE_CONTROL, "private, no-store".to_owned()),
            ],
            axum::body::Body::empty(),
        )
            .into_response());
    }
    let permit = state
        .storage
        .try_acquire_download()
        .ok_or_else(|| Error::unavailable(()))?;
    let mut response = match state
        .storage
        .download(&row.object_key(), &filename, None)
        .await
        .map_err(Error::unavailable)?
    {
        Download::Redirect(location) => {
            (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
        }
        Download::Stream { file, bytes } => {
            let stream = futures_util::stream::unfold(
                (ReaderStream::new(file), permit),
                |(mut reader, permit)| async move {
                    use futures_util::StreamExt;
                    reader.next().await.map(|chunk| (chunk, (reader, permit)))
                },
            );
            (
                [
                    (header::CONTENT_TYPE, "application/gzip".to_owned()),
                    (header::CONTENT_LENGTH, bytes.to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                codeyard_api_http::middleware::download_body(stream),
            )
                .into_response()
        }
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "private, no-store".parse().map_err(Error::unavailable)?,
    );
    Ok(response)
}

async fn delete_archive(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (actor, org) = scope(&state, &headers, &params, true).await?;
    exports::delete_archive(&state.pool, actor.user.id, org, id(&params)?)
        .await
        .map_err(failure)?
        .ok_or_else(Error::missing)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unlock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (actor, org) = scope(&state, &headers, &params, true).await?;
    let name = params.get("repo_name").ok_or_else(Error::missing)?;
    if !exports::unlock_repository(&state.pool, actor.user.id, org, id(&params)?, name)
        .await
        .map_err(failure)?
    {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}
