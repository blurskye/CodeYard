use crate::gists::create;
use crate::gists::failure;
use crate::gists::handlers;
use crate::gists::hidden_failure;
use crate::gists::list;
use crate::gists::read;
use crate::gists::remove;
use crate::gists::update;
use codeyard_api_http::shared::github::gists::writer;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("gists/list-comments", get(list)),
        ("gists/create-comment", post(create)),
        ("gists/get-comment", get(read)),
        ("gists/update-comment", patch(update)),
        ("gists/delete-comment", delete(remove)),
    ]
}
#[derive(Deserialize)]
struct Body {
    body: String,
}
fn comment_id(value: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let gist = domain::metadata(&state.pool, &id).await.map_err(failure)?;
    let (offset, limit) = page.window()?;
    let mut comments = codeyard_db_repos::gists::comments(
        &state.pool,
        gist.repo_id,
        (limit + 1) as i64,
        offset as i64,
    )
    .await
    .map_err(Error::unavailable)?;
    let more = comments.len() > limit;
    comments.truncate(limit);
    let mut values = Vec::new();
    for comment in comments {
        values.push(documents::comment(&state, &gist, &comment).await?);
    }
    let etag = documents::validator(&values)?;
    documents::conditional(&headers, etag, page.response(&uri, values, more)?)
}
async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, comment)): Path<(String, String)>,
) -> Result<Response> {
    auth::authenticate(&state, &headers, false).await?;
    let gist = domain::metadata(&state.pool, &id).await.map_err(failure)?;
    let comment =
        codeyard_db_repos::gists::comment(&state.pool, gist.repo_id, comment_id(&comment)?)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    documents::response(&headers, documents::comment(&state, &gist, &comment).await?)
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    input: std::result::Result<Json<Body>, JsonRejection>,
) -> Result<Response> {
    let actor = writer(&state, &headers).await?;
    let Json(input) = input.map_err(|_| Error::validation("GistComment", "body", "invalid"))?;
    let gist = domain::metadata(&state.pool, &id).await.map_err(failure)?;
    let comment = codeyard_db_repos::gists::create_comment(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        gist.repo_id,
        &input.body,
    )
    .await
    .map_err(failure)?;
    let value = documents::comment(&state, &gist, &comment).await?;
    let location = value["url"]
        .as_str()
        .ok_or_else(|| Error::unavailable(()))?
        .parse()
        .map_err(Error::unavailable)?;
    let mut response = (StatusCode::CREATED, Json(value)).into_response();
    response.headers_mut().insert(header::LOCATION, location);
    Ok(response)
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, comment)): Path<(String, String)>,
    input: std::result::Result<Json<Body>, JsonRejection>,
) -> Result<Json<Value>> {
    let actor = writer(&state, &headers).await?;
    let Json(input) = input.map_err(|_| Error::validation("GistComment", "body", "invalid"))?;
    let gist = domain::metadata(&state.pool, &id).await.map_err(failure)?;
    let comment = codeyard_db_repos::gists::change_comment(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        gist.repo_id,
        comment_id(&comment)?,
        Some(&input.body),
    )
    .await
    .map_err(hidden_failure)?;
    Ok(Json(documents::comment(&state, &gist, &comment).await?))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, comment)): Path<(String, String)>,
) -> Result<StatusCode> {
    let actor = writer(&state, &headers).await?;
    let gist = domain::metadata(&state.pool, &id).await.map_err(failure)?;
    codeyard_db_repos::gists::change_comment(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        gist.repo_id,
        comment_id(&comment)?,
        None,
    )
    .await
    .map_err(failure)?;
    Ok(StatusCode::NO_CONTENT)
}
