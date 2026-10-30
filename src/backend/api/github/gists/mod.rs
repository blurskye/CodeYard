use super::{auth, pagination::Pagination, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        gists::{GistList, GistRefusal, ListScope},
    },
    gists as domain,
    repositories::{
        lifecycle::{LifecycleFailure, StorageError},
        store::{
            repo_store, ErrorKind, GistCredential, GistEdit, GistFileChange, RepoError,
            MAX_GIST_REQUEST_BYTES,
        },
    },
};
use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, OriginalUri, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
mod comments;
mod documents;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    let mut handlers = vec![
        ("gists/list", get(list)),
        ("gists/list-public", get(public)),
        ("gists/list-starred", get(starred)),
        ("gists/list-for-user", get(for_user)),
        (
            "gists/create",
            post(create)
                .layer(DefaultBodyLimit::max(MAX_GIST_REQUEST_BYTES))
                .layer(axum::middleware::from_fn(buffered_write)),
        ),
        ("gists/get", get(read)),
        ("gists/get-revision", get(revision)),
        (
            "gists/update",
            patch(update)
                .layer(DefaultBodyLimit::max(MAX_GIST_REQUEST_BYTES))
                .layer(axum::middleware::from_fn(buffered_write)),
        ),
        ("gists/delete", delete(remove)),
        ("gists/list-commits", get(commits)),
        ("gists/list-forks", get(forks)),
        ("gists/fork", post(fork)),
        ("gists/check-is-starred", get(check)),
        ("gists/star", put(star)),
        ("gists/unstar", delete(unstar)),
    ];
    handlers.extend(comments::handlers());
    handlers
}
// gh uses POST for edits even though the published REST operation is PATCH.
pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new().route(
        "/gists/:gist_id",
        post(update)
            .layer(DefaultBodyLimit::max(MAX_GIST_REQUEST_BYTES))
            .layer(axum::middleware::from_fn(buffered_write)),
    )
}
pub(crate) fn failure_store(error: RepoError) -> Error {
    if error.is_write_denied() {
        return Error::new(StatusCode::FORBIDDEN, "Gist permission denied");
    }
    if error.is_response_too_large() {
        return Error::invalid("Gist exceeds its content or computation limit");
    }
    match error.kind {
        ErrorKind::NotFound | ErrorKind::MissingPath => Error::missing(),
        ErrorKind::Invalid => Error::invalid(error.message),
        ErrorKind::Conflict => Error::new(StatusCode::CONFLICT, error.message),
        ErrorKind::Unavailable => Error::unavailable(()),
    }
}
pub(crate) fn failure(error: anyhow::Error) -> Error {
    if let Some(error) = error.downcast_ref::<GistRefusal>() {
        return match error {
            GistRefusal::Missing => Error::missing(),
            GistRefusal::Denied => Error::new(StatusCode::FORBIDDEN, "Gist permission denied"),
            GistRefusal::Conflict => {
                Error::new(StatusCode::CONFLICT, "Gist changed; reload before saving")
            }
            GistRefusal::Invalid => Error::validation("Gist", "files", "invalid"),
        };
    }
    if error.is::<db::users::accounts::AccountRestricted>() {
        return Error::new(StatusCode::FORBIDDEN, "Account cannot change gists");
    }
    if let Some(error) = error.downcast_ref::<StorageError>() {
        return match error.kind {
            LifecycleFailure::Invalid
            | LifecycleFailure::OwnRepositoryFork
            | LifecycleFailure::Conflict => Error::validation("Gist", "forks", "invalid"),
            LifecycleFailure::Denied => Error::new(StatusCode::FORBIDDEN, "Gist permission denied"),
            _ => Error::unavailable(()),
        };
    }
    if error.is::<domain::GistSaved>() || error.is::<domain::GistDeleted>() {
        return Error::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string());
    }
    match error.downcast::<RepoError>() {
        Ok(error) => failure_store(error),
        Err(error) => Error::unavailable(error),
    }
}
static GIST_UPLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
async fn buffered_write(
    axum::Extension(state): axum::Extension<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Err(error) = writer(&state, request.headers()).await {
        return error.into_response();
    }
    let Ok(_permit) = GIST_UPLOADS.try_acquire() else {
        return Error::new(StatusCode::SERVICE_UNAVAILABLE, "Gist uploads are busy")
            .into_response();
    };
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        axum::body::to_bytes(body, MAX_GIST_REQUEST_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return Error::new(StatusCode::PAYLOAD_TOO_LARGE, "Gist request is too large")
                .into_response()
        }
        Err(_) => {
            return Error::new(StatusCode::REQUEST_TIMEOUT, "Gist request timed out")
                .into_response()
        }
    };
    next.run(axum::extract::Request::from_parts(
        parts,
        axum::body::Body::from(bytes),
    ))
    .await
}
fn hidden_failure(error: anyhow::Error) -> Error {
    if matches!(
        error.downcast_ref::<GistRefusal>(),
        Some(GistRefusal::Denied)
    ) {
        Error::missing()
    } else {
        failure(error)
    }
}
async fn writer(state: &AppState, headers: &HeaderMap) -> Result<auth::Authentication> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    Ok(actor)
}
#[derive(Deserialize, Default)]
struct Listing {
    page: Option<usize>,
    per_page: Option<usize>,
    since: Option<String>,
}
impl Listing {
    fn page(&self) -> Pagination {
        Pagination {
            page: self.page,
            per_page: self.per_page,
        }
    }
    fn since(&self) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        self.since
            .as_deref()
            .map(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map(|date| date.with_timezone(&chrono::Utc))
                    .map_err(|_| Error::validation("Gist", "since", "invalid"))
            })
            .transpose()
    }
}
enum ListingScope {
    Default,
    Public,
    Starred,
    User(String),
}
async fn collection(
    state: Arc<AppState>,
    headers: HeaderMap,
    query: Listing,
    uri: axum::http::Uri,
    scope: ListingScope,
) -> Result<Response> {
    let actor =
        auth::authenticate(&state, &headers, matches!(scope, ListingScope::Starred)).await?;
    let cap = matches!(scope, ListingScope::Public);
    let scope = match scope {
        ListingScope::Default => actor.as_ref().map_or(ListScope::Public, |actor| {
            ListScope::Owner(actor.user.id, true)
        }),
        ListingScope::Public => ListScope::Public,
        ListingScope::Starred => {
            ListScope::Starred(actor.as_ref().ok_or_else(auth::required)?.user.id)
        }
        ListingScope::User(name) => {
            ListScope::Owner(super::users::target(&state, &name).await?.id, false)
        }
    };
    let page = query.page();
    let (offset, limit) = page.window()?;
    let available = if cap {
        3000usize.saturating_sub(offset)
    } else {
        usize::MAX
    };
    let limit = limit.min(available);
    if let ListScope::Owner(owner, true) = scope {
        domain::recover_owned(&state, owner)
            .await
            .map_err(failure)?;
    }
    let mut entries = db::gists::list(
        &state.pool,
        &GistList {
            scope,
            since: query.since()?,
            limit: (limit.saturating_add(1).min(available)) as i64,
            offset: offset as i64,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let more = entries.len() > limit;
    entries.truncate(limit);
    let mut values = Vec::new();
    for entry in entries {
        let loaded = domain::load(&state, &entry.id, None)
            .await
            .map_err(failure)?;
        values.push(documents::base(&state, &loaded, false).await?);
    }
    let etag = documents::validator(&values)?;
    documents::conditional(&headers, etag, page.response(&uri, values, more)?)
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<Listing>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    collection(state, headers, query, uri, ListingScope::Default).await
}
async fn public(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<Listing>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    collection(state, headers, query, uri, ListingScope::Public).await
}
async fn starred(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<Listing>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    collection(state, headers, query, uri, ListingScope::Starred).await
}
async fn for_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user): Path<String>,
    Query(query): Query<Listing>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    collection(state, headers, query, uri, ListingScope::User(user)).await
}
#[derive(Deserialize)]
struct NewFile {
    content: String,
}
#[derive(Deserialize)]
struct NewGist {
    #[serde(default)]
    description: String,
    files: BTreeMap<String, NewFile>,
    #[serde(default, deserialize_with = "public_flag")]
    public: bool,
}
fn public_flag<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<bool, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::Bool(value) => Ok(value),
        Value::String(value) if value == "true" => Ok(true),
        Value::String(value) if value == "false" => Ok(false),
        _ => Err(serde::de::Error::custom("public must be true or false")),
    }
}
fn saved_error(id: &str) -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        domain::GistSaved { id: id.into() }.to_string(),
    )
}
async fn created(state: &AppState, loaded: &domain::LoadedGist) -> Result<Response> {
    let value = documents::detail(state, loaded)
        .await
        .map_err(|_| saved_error(&loaded.gist.id))?;
    let mut response = (StatusCode::CREATED, Json(value)).into_response();
    response.headers_mut().insert(
        header::LOCATION,
        super::api_url(&format!("/gists/{}", loaded.gist.id))
            .parse()
            .map_err(|_| saved_error(&loaded.gist.id))?,
    );
    Ok(response)
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: std::result::Result<Json<NewGist>, JsonRejection>,
) -> Result<Response> {
    let actor = writer(&state, &headers).await?;
    let Json(input) = input.map_err(|_| Error::validation("Gist", "files", "invalid"))?;
    let loaded = domain::create(
        state.clone(),
        actor.user.id,
        GistCredential::Token(actor.token.id, actor.token.token_hash.clone()),
        domain::CreateGist {
            description: input.description,
            is_public: input.public,
            files: input
                .files
                .into_iter()
                .map(|(name, file)| (name, file.content))
                .collect(),
        },
    )
    .await
    .map_err(failure)?;
    created(&state, &loaded).await
}
async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let loaded = domain::load(&state, &id, None).await.map_err(failure)?;
    documents::response(&headers, documents::detail(&state, &loaded).await?)
}
async fn revision(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, sha)): Path<(String, String)>,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let loaded = domain::load(&state, &id, Some(&sha))
        .await
        .map_err(failure)?;
    documents::response(&headers, documents::detail(&state, &loaded).await?)
}
#[derive(Deserialize)]
struct Edit {
    description: Option<String>,
    files: Option<BTreeMap<String, Option<GistFileChange>>>,
    updated_at: Option<String>,
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: axum::http::Method,
    input: std::result::Result<Json<Option<Edit>>, JsonRejection>,
) -> Result<Json<Value>> {
    let actor = writer(&state, &headers).await?;
    let Json(input) = input.map_err(|_| Error::validation("Gist", "files", "invalid"))?;
    let mut input = input.ok_or_else(|| Error::validation("Gist", "files", "invalid"))?;
    // gh rename serializes a zero GistFile for the old name, unlike edit's null deletions.
    if method == axum::http::Method::POST
        && input.updated_at.as_deref() == Some("0001-01-01T00:00:00Z")
    {
        for change in input.files.iter_mut().flat_map(|files| files.values_mut()) {
            if change
                .as_ref()
                .is_some_and(|file| file.filename.is_none() && file.content.as_deref() == Some(""))
            {
                *change = None;
            }
        }
    }
    let loaded = domain::update(
        state.clone(),
        actor.user.id,
        GistCredential::Token(actor.token.id, actor.token.token_hash.clone()),
        id,
        GistEdit {
            description: input.description,
            files: input.files.unwrap_or_default(),
            expected: None,
        },
    )
    .await
    .map_err(hidden_failure)?;
    Ok(Json(
        documents::detail(&state, &loaded)
            .await
            .map_err(|_| saved_error(&loaded.gist.id))?,
    ))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let actor = writer(&state, &headers).await?;
    domain::delete(
        state,
        actor.user.id,
        GistCredential::Token(actor.token.id, actor.token.token_hash.clone()),
        id,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn fork(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response> {
    let actor = writer(&state, &headers).await?;
    let loaded = domain::fork(
        state.clone(),
        actor.user.id,
        GistCredential::Token(actor.token.id, actor.token.token_hash.clone()),
        id,
    )
    .await
    .map_err(failure)?;
    created(&state, &loaded).await
}
async fn commits(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let loaded = domain::load(&state, &id, None).await.map_err(failure)?;
    let (offset, limit) = page.window()?;
    let mut values = documents::history(&state, &loaded, offset, limit + 1).await?;
    let more = values.len() > limit;
    values.truncate(limit);
    let etag = documents::validator(&values)?;
    documents::conditional(&headers, etag, page.response(&uri, values, more)?)
}
async fn forks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let loaded = domain::load(&state, &id, None).await.map_err(failure)?;
    let (offset, limit) = page.window()?;
    let mut forks = db::gists::list(
        &state.pool,
        &GistList {
            scope: ListScope::Forks(loaded.repository.id),
            since: None,
            limit: (limit + 1) as i64,
            offset: offset as i64,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let more = forks.len() > limit;
    forks.truncate(limit);
    let mut values = Vec::new();
    for fork in forks {
        let loaded = domain::load(&state, &fork.id, None)
            .await
            .map_err(failure)?;
        values.push(documents::base(&state, &loaded, false).await?);
    }
    let etag = documents::validator(&values)?;
    documents::conditional(&headers, etag, page.response(&uri, values, more)?)
}
async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    let loaded = domain::load(&state, &id, None).await.map_err(failure)?;
    Ok(
        if db::gists::starred(&state.pool, actor.user.id, loaded.repository.id)
            .await
            .map_err(Error::unavailable)?
        {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::NOT_FOUND
        }
        .into_response(),
    )
}
async fn change_star(
    state: Arc<AppState>,
    headers: HeaderMap,
    id: String,
    adding: bool,
) -> Result<StatusCode> {
    let actor = writer(&state, &headers).await?;
    let loaded = domain::load(&state, &id, None).await.map_err(failure)?;
    db::gists::star(
        &state.pool,
        actor.user.id,
        &db::users::accounts::AccountCredential::Token(actor.token.id, &actor.token.token_hash),
        loaded.repository.id,
        adding,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn star(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    change_star(state, headers, id, true).await
}
async fn unstar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    change_star(state, headers, id, false).await
}
