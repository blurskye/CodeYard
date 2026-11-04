use axum::extract::rejection::JsonRejection;
use axum::extract::DefaultBodyLimit;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use axum::Router;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_app::repositories::lifecycle::LifecycleFailure;
use codeyard_app::repositories::lifecycle::StorageError;
use codeyard_app::repositories::store::gists::GistCredential;
use codeyard_db_repos::gists::GistList;
use codeyard_db_repos::gists::GistRefusal;
use codeyard_db_repos::gists::ListScope;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::gist_types::GistEdit;
use codeyard_git::gist_types::GistFileChange;
use codeyard_git::gist_types::MAX_GIST_REQUEST_BYTES;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

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
    handlers.extend(crate::gists::comments::handlers());
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
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
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
    if error.is::<codeyard_app::gists::GistSaved>()
        || error.is::<codeyard_app::gists::GistDeleted>()
    {
        return Error::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string());
    }
    match error.downcast::<RepoError>() {
        Ok(error) => failure_store(error),
        Err(error) => Error::unavailable(error),
    }
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
    let actor = codeyard_app::api_shared::github::auth::authenticate(
        &state,
        &headers,
        matches!(scope, ListingScope::Starred),
    )
    .await?;
    let cap = matches!(scope, ListingScope::Public);
    let scope = match scope {
        ListingScope::Default => actor.as_ref().map_or(ListScope::Public, |actor| {
            ListScope::Owner(actor.user.id, true)
        }),
        ListingScope::Public => ListScope::Public,
        ListingScope::Starred => ListScope::Starred(
            actor
                .as_ref()
                .ok_or_else(codeyard_app::api_shared::github::auth::required)?
                .user
                .id,
        ),
        ListingScope::User(name) => {
            ListScope::Owner(crate::users::target(&state, &name).await?.id, false)
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
        codeyard_app::gists::recover_owned(&state, owner)
            .await
            .map_err(failure)?;
    }
    let mut entries = codeyard_db_repos::gists::list(
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
        let loaded = codeyard_app::gists::load(&state, &entry.id, None)
            .await
            .map_err(failure)?;
        values.push(super::base(&state, &loaded, false).await?);
    }
    let etag = crate::gists::documents::validator(&values)?;
    crate::gists::documents::conditional(&headers, etag, page.response(&uri, values, more)?)
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
        codeyard_app::gists::GistSaved { id: id.into() }.to_string(),
    )
}
async fn created(state: &AppState, loaded: &codeyard_app::gists::LoadedGist) -> Result<Response> {
    let value = crate::gists::documents::detail(state, loaded)
        .await
        .map_err(|_| saved_error(&loaded.gist.id))?;
    let mut response = (StatusCode::CREATED, Json(value)).into_response();
    response.headers_mut().insert(
        header::LOCATION,
        codeyard_model::urls::api_url(&format!("/gists/{}", loaded.gist.id))
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
    let loaded = codeyard_app::gists::create(
        state.clone(),
        actor.user.id,
        GistCredential::Token(actor.token.id, actor.token.token_hash.clone()),
        codeyard_app::gists::CreateGist {
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
    codeyard_app::api_shared::github::auth::authenticate(&state, &headers, false).await?;
    let loaded = codeyard_app::gists::load(&state, &id, None)
        .await
        .map_err(failure)?;
    crate::gists::documents::response(
        &headers,
        crate::gists::documents::detail(&state, &loaded).await?,
    )
}
async fn revision(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, sha)): Path<(String, String)>,
) -> Result<Response> {
    codeyard_app::api_shared::github::auth::authenticate(&state, &headers, false).await?;
    let loaded = codeyard_app::gists::load(&state, &id, Some(&sha))
        .await
        .map_err(failure)?;
    crate::gists::documents::response(
        &headers,
        crate::gists::documents::detail(&state, &loaded).await?,
    )
}
#[derive(Deserialize)]
struct EditedFile {
    filename: Option<String>,
    content: Option<String>,
    truncated: Option<bool>,
}
#[derive(Deserialize)]
struct Edit {
    description: Option<String>,
    files: Option<BTreeMap<String, Option<EditedFile>>>,
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
    let mut files = input.files.unwrap_or_default();
    let mut expected = None;
    if method == axum::http::Method::POST
        && input.updated_at.as_deref() == Some("0001-01-01T00:00:00Z")
    {
        let original = codeyard_app::gists::load(&state, &id, None)
            .await
            .map_err(failure)?;
        let existing: std::collections::HashSet<_> = original
            .snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        let removed: Vec<_> = files
            .iter()
            .filter(|(name, file)| file.is_none() && existing.contains(name.as_str()))
            .map(|(name, _)| name.clone())
            .collect();
        let added: Vec<_> = files
            .iter()
            .filter(|(name, file)| file.is_some() && !existing.contains(name.as_str()))
            .map(|(name, _)| name.clone())
            .collect();
        if removed.len() == 1 && added.len() == 1 {
            files.remove(&added[0]);
            files.insert(
                removed[0].clone(),
                Some(EditedFile {
                    filename: Some(added[0].clone()),
                    content: None,
                    truncated: None,
                }),
            );
            for (name, file) in &mut files {
                if let Some(file) = file {
                    if file.truncated == Some(true)
                        && existing.contains(name.as_str())
                        && file
                            .filename
                            .as_deref()
                            .is_none_or(|filename| filename == name.as_str())
                    {
                        file.content = None;
                    }
                }
            }
            expected = Some(original.snapshot.sha);
        }
    }
    if files
        .values()
        .flatten()
        .any(|file| file.truncated == Some(true) && file.content.is_some())
    {
        return Err(Error::validation("Gist", "content", "truncated"));
    }
    let files = files
        .into_iter()
        .map(|(name, file)| {
            (
                name,
                file.map(|file| GistFileChange {
                    filename: file.filename,
                    content: file.content,
                }),
            )
        })
        .collect();
    let loaded = codeyard_app::gists::update(
        state.clone(),
        actor.user.id,
        GistCredential::Token(actor.token.id, actor.token.token_hash.clone()),
        id,
        GistEdit {
            description: input.description,
            files,
            expected,
        },
    )
    .await
    .map_err(hidden_failure)?;
    Ok(Json(
        crate::gists::documents::detail(&state, &loaded)
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
    codeyard_app::gists::delete(
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
    let loaded = codeyard_app::gists::fork(
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
    codeyard_app::api_shared::github::auth::authenticate(&state, &headers, false).await?;
    let loaded = codeyard_app::gists::load(&state, &id, None)
        .await
        .map_err(failure)?;
    let (offset, limit) = page.window()?;
    let mut values = crate::gists::documents::history(&state, &loaded, offset, limit + 1).await?;
    let more = values.len() > limit;
    values.truncate(limit);
    let etag = crate::gists::documents::validator(&values)?;
    crate::gists::documents::conditional(&headers, etag, page.response(&uri, values, more)?)
}
async fn forks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    codeyard_app::api_shared::github::auth::authenticate(&state, &headers, false).await?;
    let loaded = codeyard_app::gists::load(&state, &id, None)
        .await
        .map_err(failure)?;
    let (offset, limit) = page.window()?;
    let mut forks = codeyard_db_repos::gists::list(
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
        let loaded = codeyard_app::gists::load(&state, &fork.id, None)
            .await
            .map_err(failure)?;
        values.push(super::base(&state, &loaded, false).await?);
    }
    let etag = crate::gists::documents::validator(&values)?;
    crate::gists::documents::conditional(&headers, etag, page.response(&uri, values, more)?)
}
async fn check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response> {
    let actor = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let gist = codeyard_app::gists::metadata(&state.pool, &id)
        .await
        .map_err(failure)?;
    Ok(
        if codeyard_db_repos::gists::starred(&state.pool, actor.user.id, gist.repo_id)
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
    let gist = codeyard_app::gists::metadata(&state.pool, &id)
        .await
        .map_err(failure)?;
    codeyard_db_repos::gists::star(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        gist.repo_id,
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

use codeyard_api_http::shared::github::gists::buffered_write;

use codeyard_api_http::shared::github::gists::writer;
