use crate::backend::{
    api::{http::error::ApiError, http::session::get_current_user},
    app::AppState,
    ci::runs::DispatchRequest,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct TriggerCiResponse {
    pub job_id: String,
    pub mode: String,
    pub pipeline: String,
    pub workflow_path: String,
    pub status: String,
    pub message: String,
}

pub async fn trigger_ci(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<DispatchRequest>,
) -> Result<Json<TriggerCiResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let run = state
        .ci_runs
        .dispatch_run(user.id, request)
        .await
        .map_err(super::admission_error)?;
    Ok(Json(TriggerCiResponse {
        job_id: run.run_id.to_string(),
        mode: run.mode,
        pipeline: run.pipeline,
        workflow_path: run.workflow_path,
        status: run.status,
        message: "CI/CD job started".to_owned(),
    }))
}

use crate::backend::api::http::context::PageContext;
use crate::webui::pages::ci as view;
use crate::webui::pages::repos::actions as view_actions;
use axum::{
    extract::{OriginalUri, Path},
    http::Uri,
    response::Response,
    Form,
};
use std::collections::{BTreeMap, HashMap};
pub(super) async fn global_dispatch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    dispatch(state, headers, uri, form, None).await
}
pub(super) async fn repository_dispatch(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    dispatch(state, headers, uri, form, Some((owner, repo))).await
}

fn draft(
    form: &HashMap<String, String>,
    scope: Option<&(String, String)>,
) -> view_actions::DispatchDraft {
    let value = |key: &str| {
        form.get(key)
            .map(|value| value.trim().to_string())
            .unwrap_or_default()
    };
    view_actions::DispatchDraft {
        owner: scope
            .map(|(owner, _)| owner.clone())
            .unwrap_or_else(|| value("repo_owner")),
        repository: scope
            .map(|(_, repo)| repo.clone())
            .unwrap_or_else(|| value("repo_name")),
        branch: value("branch"),
        event: if scope.is_some() {
            "workflow_dispatch".into()
        } else {
            value("event")
        },
        pipeline: if scope.is_some() {
            "ci".into()
        } else {
            value("pipeline")
        },
        workflow: value("workflow_path"),
        head_sha: value("head_sha"),
        environment: value("environment"),
        inputs: if scope.is_some() {
            form.iter()
                .filter(|(key, _)| key.starts_with("input:"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        } else {
            BTreeMap::new()
        },
        inputs_text: value("inputs"),
    }
}

/// The `name=value` lines of the global form. Each name appears once.
fn input_lines(text: &str) -> Result<BTreeMap<String, serde_json::Value>, &'static str> {
    const FORMAT: &str = "each workflow input goes on its own line as name=value";
    let mut inputs = BTreeMap::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once('=').ok_or(FORMAT)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(FORMAT);
        }
        if inputs
            .insert(
                name.to_owned(),
                serde_json::Value::String(value.trim().to_owned()),
            )
            .is_some()
        {
            return Err("a workflow input is given more than once");
        }
    }
    Ok(inputs)
}

/// The workflows of the draft's branch, for the repository form on a refusal page. Only called
/// once the repository chrome has loaded, which checked that the viewer may read the repository.
async fn refusal_workflows(
    ctx: &PageContext,
    owner: &str,
    name: &str,
    branch: &str,
) -> anyhow::Result<Vec<view_actions::Workflow>> {
    anyhow::ensure!(
        crate::backend::ci::admission::is_safe_ref(branch),
        "unsafe ref"
    );
    let repository =
        crate::backend::db::repositories::records::get_repository(&ctx.state.pool, owner, name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("repository not found"))?;
    let store =
        crate::backend::repositories::store::repo_store(&repository, ctx.state.cluster.as_ref());
    let definitions = crate::backend::ci::list_workflows_for_ref(&store, branch).await?;
    Ok(definitions
        .into_iter()
        .map(super::jobs::collection::workflow_view)
        .collect())
}

async fn dispatch_error(
    ctx: &PageContext,
    status: StatusCode,
    base_path: &str,
    scope: Option<&(String, String)>,
    draft: view_actions::DispatchDraft,
    message: &str,
) -> Response {
    // Only repository routes can supply chrome. Global drafts are untrusted input,
    // not authority to disclose a repository header. Do not load on accepted runs.
    let repo_chrome = if let Some((owner, name)) = scope {
        match crate::backend::api::repos::chrome::load_chrome(ctx, owner, name).await {
            Ok(chrome) => Some(Box::new(chrome)),
            Err(response) => return response,
        }
    } else {
        None
    };
    let mut message = message.to_string();
    let workflows = match scope {
        Some((owner, name)) if repo_chrome.is_some() => {
            match refusal_workflows(ctx, owner, name, &draft.branch).await {
                Ok(workflows) => workflows,
                Err(_) => {
                    message.push_str(
                        " The workflows of this branch could not be read; open the repository's Actions page and run it from there.",
                    );
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    };
    ctx.page_model_status(
        status,
        "Run was not accepted",
        crate::webui::app::Page::Actions(view::Page::DispatchRecovery {
            repo_chrome,
            base_path: base_path.to_string(),
            csrf_token: ctx.csrf_token.clone(),
            message,
            draft: Box::new(draft),
            workflows,
        }),
    )
}

async fn dispatch(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    form: HashMap<String, String>,
    scope: Option<(String, String)>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.require_user() {
        return response;
    }
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    // The global form's inputs box holds several `name=value` lines.
    if form
        .iter()
        .any(|(key, value)| value.len() > if key == "inputs" { 8192 } else { 2048 })
    {
        return ctx.error(StatusCode::BAD_REQUEST, "Dispatch form value is too long.");
    }
    let draft = draft(&form, scope.as_ref());
    let base_path = scope
        .as_ref()
        .map(|(owner, repo)| {
            format!(
                "/{}/{}/actions",
                crate::webui::encode_component(owner),
                crate::webui::encode_component(repo)
            )
        })
        .unwrap_or_else(|| "/ci".into());
    if !matches!(draft.pipeline.as_str(), "ci" | "cd" | "full") {
        return dispatch_error(
            &ctx,
            StatusCode::BAD_REQUEST,
            &base_path,
            scope.as_ref(),
            draft,
            "Check the pipeline and workflow/environment field lengths.",
        )
        .await;
    }
    let optional = |value: &str| (!value.is_empty()).then(|| value.to_string());
    // The repository form posts every dispatchable workflow's inputs, named `input:<path>:<name>`;
    // only the chosen workflow's are this run's. The global form sends `name=value` lines.
    let inputs: Option<BTreeMap<String, serde_json::Value>> = match scope.as_ref() {
        Some(_) => {
            let prefix = format!("input:{}:", draft.workflow);
            Some(
                draft
                    .inputs
                    .iter()
                    .filter_map(|(key, value)| {
                        key.strip_prefix(&prefix)
                            .map(|name| (name.to_owned(), serde_json::Value::String(value.clone())))
                    })
                    .collect(),
            )
        }
        None => match input_lines(&draft.inputs_text) {
            Ok(inputs) => Some(inputs),
            Err(message) => {
                return dispatch_error(
                    &ctx,
                    StatusCode::BAD_REQUEST,
                    &base_path,
                    scope.as_ref(),
                    draft,
                    message,
                )
                .await
            }
        },
    };
    let request = DispatchRequest {
        repo_owner: draft.owner.clone(),
        repo_name: draft.repository.clone(),
        branch: draft.branch.clone(),
        event: draft.event.clone(),
        head_sha: optional(&draft.head_sha),
        workflow_path: optional(&draft.workflow),
        pipeline: Some(draft.pipeline.clone()),
        environment: optional(&draft.environment),
        inputs,
    };
    let actor_id = ctx.require_user().expect("user checked above").id;
    match ctx
        .state
        .ci_runs
        .dispatch_run(actor_id, request)
        .await
        .map_err(super::admission_error)
    {
        Ok(result) => {
            let mut query = BTreeMap::new();
            if let Some(return_query) = form.get("return_query") {
                if let Ok(uri) = return_query.parse::<Uri>() {
                    if uri.scheme().is_none()
                        && uri.authority().is_none()
                        && uri.path() == base_path
                    {
                        if let Some(values) = uri.query() {
                            for (key, value) in url::form_urlencoded::parse(values.as_bytes()) {
                                if matches!(
                                    key.as_ref(),
                                    "workflow"
                                        | "event"
                                        | "status"
                                        | "branch"
                                        | "q"
                                        | "log_q"
                                        | "wrap"
                                        | "limit"
                                        | "log_limit"
                                        | "ref"
                                ) && value.len() <= 500
                                {
                                    query.insert(key.into_owned(), value.into_owned());
                                }
                            }
                        }
                    }
                }
            }
            ctx.redirect(&view_actions::query_url(
                &base_path,
                &query,
                &[("job", &result.run_id.to_string()), ("view", "logs")],
            ))
        }
        Err((status, Json(error))) => {
            let message = if status.is_server_error() {
                "The runner could not confirm admission. Check run history before retrying; do not assume execution succeeded.".to_owned()
            } else {
                // Client refusals name the field at fault, such as a missing workflow input.
                format!("Run was not accepted. {}", error.message)
            };
            dispatch_error(&ctx, status, &base_path, scope.as_ref(), draft, &message).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dispatch_form_offers_only_events_the_server_accepts() {
        for event in view_actions::TRIGGER_EVENTS {
            assert!(crate::backend::ci::is_supported_event(event), "{event}");
        }
    }
    #[test]
    fn repository_dispatch_uses_route_identity_not_hidden_fields() {
        let form = HashMap::from([
            ("repo_owner".into(), "other".into()),
            ("repo_name".into(), "private".into()),
            ("event".into(), "release".into()),
            ("branch".into(), "feature/x".into()),
        ]);
        let scope = ("alice".into(), "repo".into());
        let draft = draft(&form, Some(&scope));
        assert_eq!(draft.owner, "alice");
        assert_eq!(draft.repository, "repo");
        assert_eq!(draft.event, "workflow_dispatch");
        assert_eq!(draft.branch, "feature/x");
    }
}
