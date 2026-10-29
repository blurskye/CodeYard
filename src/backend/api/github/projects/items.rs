use super::*;
use crate::backend::db::projects::{Content, Field, FieldUpdate, Item, NewView};
use axum::routing::{delete, patch};
use serde_json::json;

/// The items among `items` whose linked issue or pull request this request may read: the
/// user's access and the credential's bounds under the issues or pull requests permission.
/// The other linked items keep their place in the project with their content redacted.
async fn readable_items<'a>(
    state: &AppState,
    auth: Option<&auth::Authentication>,
    items: impl IntoIterator<Item = &'a Item>,
) -> Result<std::collections::HashSet<i64>> {
    let linked: Vec<(i64, i64, bool)> = items
        .into_iter()
        .filter_map(|item| Some((item.id, item.repo_id?, item.pull_request_id.is_some())))
        .collect();
    let repositories = |pulls: bool| {
        let mut ids: Vec<i64> = linked
            .iter()
            .filter(|(_, _, pull)| *pull == pulls)
            .map(|(_, repo, _)| *repo)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let issues = auth::readable_repositories(state, auth, &repositories(false), "issues").await?;
    let pulls =
        auth::readable_repositories(state, auth, &repositories(true), "pull-requests").await?;
    Ok(linked
        .into_iter()
        .filter(|(_, repo, pull)| if *pull { &pulls } else { &issues }.contains(repo))
        .map(|(id, _, _)| id)
        .collect())
}

#[allow(clippy::too_many_arguments)]
async fn item_document(
    state: &AppState,
    headers: &HeaderMap,
    project: &Project,
    name: &str,
    item: &Item,
    fields: &[Field],
    selected: Option<&str>,
    readable: &std::collections::HashSet<i64>,
) -> Result<Value> {
    let creator = db::users::accounts::get_user_by_id(&state.pool, item.creator_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let content = match item.repo_id {
        Some(_) if !readable.contains(&item.id) => Value::Null,
        Some(id) => {
            let repo = db::repositories::records::get_repository_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            let owner = db::repositories::records::repository_owner_name(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            if let Some(id) = item.issue_id {
                let auth = auth::authenticate(state, headers, false).await?;
                super::super::issues::document(
                    &state.pool,
                    super::super::issues::Reader::Request(state, auth.as_ref()),
                    &repo,
                    &owner,
                    db::assignees::AssignedItem::Issue(id),
                )
                .await?
            } else {
                let pull = db::pull_requests::records::get_pull_request_by_id(
                    &state.pool,
                    item.pull_request_id.ok_or_else(Error::missing)?,
                )
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
                super::super::pulls::document(state, headers, &repo, &owner, &pull, false).await?
            }
        }
        None => payloads::project_draft(item, payloads::user(&creator)),
    };
    let mut value = payloads::project_item(
        item,
        &project_url(project, name),
        payloads::user(&creator),
        content,
    );
    let mut values = Vec::new();
    for field in fields {
        if selected.is_some_and(|s| {
            !s.split(',').any(|s| {
                s.trim() == field.id.to_string() || s.trim().eq_ignore_ascii_case(&field.name)
            })
        }) {
            continue;
        }
        let raw = item.value(field).map_err(Error::unavailable)?;
        values.push(
            json!({"id": field.id, "name": field.name, "data_type": field.data_type, "value": raw}),
        );
    }
    value["fields"] = json!(values);
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddItem {
    #[serde(rename = "type")]
    kind: String,
    id: Option<i64>,
    owner: Option<String>,
    repo: Option<String>,
    number: Option<i32>,
}

async fn add_item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    input: std::result::Result<Json<AddItem>, axum::extract::rejection::JsonRejection>,
) -> Result<Response> {
    let (project, name, auth) = context(&state, &headers, &params, true, false).await?;
    let Json(input) =
        input.map_err(super::super::json_error)?;
    if !matches!(input.kind.as_str(), "Issue" | "PullRequest") {
        return Err(Error::validation("ProjectV2Item", "type", "invalid"));
    }
    let issue = input.kind == "Issue";
    let (repo_id, id) = match (input.id, input.owner, input.repo, input.number) {
        (Some(id), None, None, None) if issue => db::issues::get_issue_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .map(|issue| (issue.repo_id, issue.id)),
        (Some(id), None, None, None) => {
            db::pull_requests::records::get_pull_request_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .map(|pull| (pull.repo_id, pull.id))
        }
        (None, Some(owner), Some(name), Some(number)) => {
            let repo = db::repositories::records::get_repository(&state.pool, &owner, &name)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            if issue {
                db::issues::get_issue(&state.pool, repo.id, number)
                    .await
                    .map_err(Error::unavailable)?
                    .map(|issue| (repo.id, issue.id))
            } else {
                db::pull_requests::records::get_pull_request(&state.pool, repo.id, number)
                    .await
                    .map_err(Error::unavailable)?
                    .map(|pull| (repo.id, pull.id))
            }
        }
        _ => return Err(Error::validation("ProjectV2Item", "content", "invalid")),
    }
    .ok_or_else(Error::missing)?;
    // The content must be readable by the user and inside the credential's bounds.
    let scope = if issue { "issues" } else { "pull-requests" };
    if !auth::readable_repositories(&state, auth.as_ref(), &[repo_id], scope)
        .await?
        .contains(&repo_id)
    {
        return Err(Error::missing());
    }
    let content = if issue {
        Content::Issue(id)
    } else {
        Content::PullRequest(id)
    };
    let actor = auth.as_ref().ok_or_else(auth::required)?.user.id;
    let item = projects::add_item(&state.pool, actor, project.id, &content)
        .await
        .map_err(project_error)?;
    let fields = projects::fields(&state.pool, Some(actor), project.id)
        .await
        .map_err(project_error)?;
    let readable = readable_items(&state, auth.as_ref(), [&item]).await?;
    Ok((
        StatusCode::CREATED,
        Json(
            item_document(&state, &headers, &project, &name, &item, &fields, None, &readable)
                .await?,
        ),
    )
        .into_response())
}

#[derive(Deserialize)]
struct Draft {
    title: String,
    body: Option<String>,
}
async fn draft(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    input: std::result::Result<Json<Draft>, axum::extract::rejection::JsonRejection>,
) -> Result<Response> {
    let (project, name, auth) =
        context(&state, &headers, &params, true, !params.contains_key("org")).await?;
    let Json(input) =
        input.map_err(super::super::json_error)?;
    let actor = auth.ok_or_else(auth::required)?.user.id;
    let item = projects::add_item(
        &state.pool,
        actor,
        project.id,
        &Content::Draft {
            title: input.title,
            body: input.body,
        },
    )
    .await
    .map_err(project_error)?;
    let fields = projects::fields(&state.pool, Some(actor), project.id)
        .await
        .map_err(project_error)?;
    Ok((
        StatusCode::CREATED,
        Json(
            item_document(&state, &headers, &project, &name, &item, &fields, None, &Default::default())
                .await?,
        ),
    )
        .into_response())
}

#[derive(Deserialize)]
struct ItemQuery {
    q: Option<String>,
    fields: Option<String>,
}

async fn list_items(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<ItemQuery>,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (project, name, auth) = context(&state, &headers, &params, false, false).await?;
    let viewer = auth.as_ref().map(|a| a.user.id);
    let fields = projects::fields(&state.pool, viewer, project.id)
        .await
        .map_err(project_error)?;
    let view = if let Some(number) = params.get("view_number") {
        let number: i64 = number.parse().map_err(|_| Error::missing())?;
        let view = projects::views(&state.pool, viewer, project.id)
            .await
            .map_err(project_error)?
            .into_iter()
            .find(|v| v.number == number)
            .ok_or_else(Error::missing)?;
        Some(serde_json::from_str::<NewView>(&view.configuration).map_err(Error::unavailable)?)
    } else {
        None
    };
    let items = projects::items(&state.pool, viewer, project.id)
        .await
        .map_err(project_error)?;
    let items = crate::backend::projects::filter_items(
        items,
        &fields,
        query.q.as_deref().unwrap_or(""),
        view.as_ref(),
    )
    .map_err(project_error)?;
    let (offset, limit) = page.window()?;
    let window: Vec<&Item> = items.iter().skip(offset).take(limit + 1).collect();
    let readable = readable_items(&state, auth.as_ref(), window.iter().copied()).await?;
    let mut documents = Vec::new();
    for item in window {
        documents.push(
            item_document(
                &state,
                &headers,
                &project,
                &name,
                item,
                &fields,
                query.fields.as_deref(),
                &readable,
            )
            .await?,
        );
    }
    let more = documents.len() > limit;
    page.response(&uri, documents, more)
}

async fn get_item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<Value>> {
    let (project, name, auth) = context(&state, &headers, &params, false, false).await?;
    let viewer = auth.as_ref().map(|a| a.user.id);
    let id: i64 = params["item_id"].parse().map_err(|_| Error::missing())?;
    let item = projects::items(&state.pool, viewer, project.id)
        .await
        .map_err(project_error)?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(Error::missing)?;
    let fields = projects::fields(&state.pool, viewer, project.id)
        .await
        .map_err(project_error)?;
    let readable = readable_items(&state, auth.as_ref(), [&item]).await?;
    Ok(Json(
        item_document(
            &state,
            &headers,
            &project,
            &name,
            &item,
            &fields,
            query.fields.as_deref(),
            &readable,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
struct Update {
    fields: Vec<FieldUpdate>,
}
async fn update_item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    input: std::result::Result<Json<Update>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>> {
    let (project, name, auth) = context(&state, &headers, &params, true, false).await?;
    let Json(input) = input.map_err(super::super::json_error)?;
    let actor = auth.as_ref().ok_or_else(auth::required)?.user.id;
    let id: i64 = params["item_id"].parse().map_err(|_| Error::missing())?;
    let item = projects::update_item(&state.pool, actor, project.id, id, &input.fields)
        .await
        .map_err(project_error)?;
    let fields = projects::fields(&state.pool, Some(actor), project.id)
        .await
        .map_err(project_error)?;
    let readable = readable_items(&state, auth.as_ref(), [&item]).await?;
    Ok(Json(
        item_document(&state, &headers, &project, &name, &item, &fields, None, &readable).await?,
    ))
}

async fn delete_item(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
) -> Result<StatusCode> {
    let (project, _, auth) = context(&state, &headers, &params, true, false).await?;
    let actor = auth.ok_or_else(auth::required)?.user.id;
    let id: i64 = params["item_id"].parse().map_err(|_| Error::missing())?;
    projects::delete_item(&state.pool, actor, project.id, id)
        .await
        .map_err(project_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_view(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<Params>,
    input: std::result::Result<Json<NewView>, axum::extract::rejection::JsonRejection>,
) -> Result<Response> {
    let (project, name, auth) =
        context(&state, &headers, &params, true, !params.contains_key("org")).await?;
    let Json(input) = input.map_err(super::super::json_error)?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let view = projects::create_view(&state.pool, actor.id, project.id, &input)
        .await
        .map_err(project_error)?;
    let html = format!(
        "{}/{}/{}/projects/{}/views/{}",
        crate::backend::api::http::config::link_origin(),
        if project.owner_kind == "org" {
            "orgs"
        } else {
            "users"
        },
        crate::webui::encode_component(&name),
        project.number,
        view.number
    );
    Ok((
        StatusCode::CREATED,
        Json(
            payloads::project_view(
                &view,
                &project_url(&project, &name),
                &html,
                payloads::user(&actor),
            )
            .map_err(Error::unavailable)?,
        ),
    )
        .into_response())
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("projects/create-draft-item-for-org", post(draft)),
        (
            "projects/create-draft-item-for-authenticated-user",
            post(draft),
        ),
        ("projects/add-item-for-org", post(add_item)),
        ("projects/add-item-for-user", post(add_item)),
        ("projects/list-items-for-org", get(list_items)),
        ("projects/list-items-for-user", get(list_items)),
        ("projects/get-org-item", get(get_item)),
        ("projects/get-user-item", get(get_item)),
        ("projects/update-item-for-org", patch(update_item)),
        ("projects/update-item-for-user", patch(update_item)),
        ("projects/delete-item-for-org", delete(delete_item)),
        ("projects/delete-item-for-user", delete(delete_item)),
        ("projects/create-view-for-org", post(create_view)),
        ("projects/create-view-for-user", post(create_view)),
        ("projects/list-view-items-for-org", get(list_items)),
        ("projects/list-view-items-for-user", get(list_items)),
    ]
}
