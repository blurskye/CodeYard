//! Native branch merge adapters; both JSON and no-JavaScript forms use the shared domain operation.
use axum::extract::DefaultBodyLimit;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Form;
use axum::Json;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_api_http::shared::repos::chrome::Loaded;
use codeyard_api_http::shared::repos::forms::authorize_post;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_api_http::shared::repos::forms::with_status;
use codeyard_app::app::AppState;
use codeyard_app::repositories::merges::BranchMerge;
use codeyard_app::repositories::refs::RefError;
use codeyard_webui::components::SelectOption;
use codeyard_webui::pages::repos::merge::MergeForm;
use codeyard_webui::pages::repos::page::Page;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct MergeInput {
    pub base: String,
    pub head: String,
    pub message: Option<String>,
    pub expected_base: Option<String>,
    pub expected_head: Option<String>,
}

pub(super) async fn merge_json(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<MergeInput>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repo = crate::refs::repository_id(&state, &owner, &name).await?;
    let result = codeyard_app::repositories::merges::merge_branch(
        &state.pool,
        state.cluster.as_ref(),
        &state.ci_runs,
        codeyard_db_core::queries::credentials::Actor::from((
            actor.id,
            codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
        )),
        repo,
        &owner,
        BranchMerge {
            base: &input.base,
            head: &input.head,
            message: input.message.as_deref(),
            expected_base: input.expected_base.as_deref(),
            expected_head: input.expected_head.as_deref(),
        },
    )
    .await
    .map_err(merge_error)?;
    Ok(match result {
        Some(commit) => (StatusCode::CREATED, Json(commit)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

fn merge_error(error: RefError) -> (StatusCode, Json<ApiError>) {
    match error {
        RefError::Git(error) if error.kind == codeyard_git::error::ErrorKind::Conflict => {
            crate::responses::repo_store_error(error, StatusCode::CONFLICT)
        }
        error => crate::responses::ref_error(error),
    }
}

fn model(loaded: &Loaded, values: &HashMap<String, String>, submitted: bool) -> MergeForm {
    let mut branches: Vec<_> = loaded
        .refs
        .iter()
        .filter_map(|(name, sha)| name.strip_prefix("refs/heads/").map(|name| (name, sha)))
        .collect();
    branches.sort_unstable_by_key(|(name, _)| *name);
    let mut choices: Vec<_> = branches
        .iter()
        .map(|(name, sha)| {
            SelectOption::new(
                format!("{sha} {name}"),
                format!("{name} ({})", codeyard_webui::components::short_sha(sha)),
            )
        })
        .collect();
    let pick = |key: &str| {
        if submitted {
            return field(values, key).chars().take(4096).collect::<String>();
        }
        let name = values
            .get(key)
            .map(String::as_str)
            .unwrap_or(&loaded.default_branch);
        branches
            .iter()
            .find(|(branch, _)| *branch == name)
            .map(|(name, sha)| format!("{sha} {name}"))
            .unwrap_or_default()
    };
    let base = pick("base");
    let head = pick("head");
    for value in [&base, &head] {
        if !value.is_empty() && !choices.iter().any(|option| option.value == *value) {
            choices.push(SelectOption::new(
                value.clone(),
                "Submitted selection (reload to choose a current revision)",
            ));
        }
    }
    let repository_id = if submitted {
        values
            .get("repository_id")
            .filter(|value| value.len() <= 20 && value.parse::<i64>().is_ok())
            .cloned()
            .unwrap_or_default()
    } else {
        loaded.repository.id.to_string()
    };
    MergeForm {
        repository_id,
        base,
        head,
        choices,
        message: field(values, "message"),
        error: None,
        can_merge: loaded.chrome.can_write
            && !loaded.repository.is_archived
            && !loaded.git_unavailable,
    }
}

fn reply(ctx: &PageContext, loaded: &Loaded, form: MergeForm) -> Response {
    ctx.page_model(
        "Merge branches",
        codeyard_webui::app::Page::Repositories(Page::BranchMerge {
            repo: loaded.chrome.clone(),
            form: Box::new(form),
        }),
    )
}

async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match crate::chrome::load(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let mut form = model(&loaded, &ctx.query, false);
    if form.base.is_empty() || form.head.is_empty() {
        form.can_merge = false;
        form.error =
            Some("Choose existing branches. This repository may have no commits yet.".into());
        return with_status(reply(&ctx, &loaded, form), StatusCode::NOT_FOUND);
    }
    reply(&ctx, &loaded, form)
}

fn selected(value: &str) -> Result<(&str, &str), RefError> {
    let (sha, branch) = value
        .split_once(' ')
        .ok_or(RefError::Invalid("Choose a branch and its revision"))?;
    if !codeyard_git::is_full_commit_id(sha) || branch.is_empty() {
        return Err(RefError::Invalid("Choose a branch and its revision"));
    }
    Ok((branch, sha))
}

async fn submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path((owner, name)): Path<(String, String)>,
    Form(values): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = authorize_post(&ctx, &values) {
        return response;
    }
    let loaded =
        match codeyard_api_http::shared::repos::chrome::load_metadata(&ctx, &owner, &name).await {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
    let outcome = async {
        if field(&values, "repository_id").parse::<i64>().ok() != Some(loaded.repository.id) {
            return Err(RefError::Git(codeyard_git::error::RepoError::new(
                codeyard_git::error::ErrorKind::Conflict,
                "Repository identity changed",
            )));
        }
        let (base, expected_base) =
            selected(values.get("base").map(String::as_str).unwrap_or_default())?;
        let (head, expected_head) =
            selected(values.get("head").map(String::as_str).unwrap_or_default())?;
        let message = values
            .get("message")
            .filter(|message| !message.is_empty())
            .map(String::as_str);
        codeyard_app::repositories::merges::merge_branch(
            &ctx.state.pool,
            ctx.state.cluster.as_ref(),
            &ctx.state.ci_runs,
            ctx.database_actor(
                ctx.require_user()
                    .map_err(|_| {
                        RefError::Access(
                            codeyard_identity::repository_access::AccessError::WriteDenied,
                        )
                    })?
                    .id,
            ),
            loaded.repository.id,
            &loaded.chrome.owner,
            BranchMerge {
                base,
                head,
                message,
                expected_base: Some(expected_base),
                expected_head: Some(expected_head),
            },
        )
        .await
    }
    .await;
    match outcome {
        Ok(Some(commit)) => {
            ctx.redirect(&format!("{}/commit/{}", loaded.chrome.base(), commit.sha))
        }
        Ok(None) => ctx.redirect(&format!("{}/branches", loaded.chrome.base())),
        Err(error) => {
            let (status, Json(error)) = merge_error(error);
            let mut form = model(&loaded, &values, true);
            form.error = Some(error.message);
            with_status(reply(&ctx, &loaded, form), status)
        }
    }
}

pub(super) fn pages() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/merge", get(page).post(submit))
        .layer(DefaultBodyLimit::max(1024 * 1024))
}

pub(super) fn json_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/repos/:owner/:repo/merge", post(merge_json))
}
