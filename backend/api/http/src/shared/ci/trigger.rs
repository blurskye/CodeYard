use crate::context::PageContext;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Form;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_app::ci::runs::DispatchRequest;
use codeyard_webui::pages::ci as view;
use codeyard_webui::pages::repos::actions as view_actions;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
pub(super) async fn repository_dispatch(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    dispatch(state, headers, uri, form, Some((owner, repo))).await
}

pub fn draft(
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
        branch: form.get("branch").cloned().unwrap_or_default(),
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
        // Kept as typed, so a refused draft comes back unchanged.
        inputs_text: form.get("inputs").cloned().unwrap_or_default(),
    }
}

/// The `name=value` lines of the global form. Each name appears once. Spaces around the name and
/// the value are dropped, as in `name = value`; a choice that has such spaces can still be picked
/// in the repository's own form.
fn input_lines(text: &str) -> Result<BTreeMap<String, serde_json::Value>, &'static str> {
    const FORMAT: &str = "each workflow input goes on its own line as name=value";
    let mut inputs = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
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
pub async fn refusal_workflows(
    ctx: &PageContext,
    repository: &codeyard_db_core::models::Repository,
    branch: &str,
) -> anyhow::Result<Vec<view_actions::Workflow>> {
    anyhow::ensure!(
        codeyard_app::ci::admission::is_safe_ref(branch),
        "unsafe ref"
    );
    let store =
        codeyard_app::repositories::store::repo_store(repository, ctx.state.cluster.as_ref());
    let definitions = codeyard_app::ci::workflows::list_workflows_for_ref(&store, branch).await?;
    let user = ctx
        .user
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Sign in required"))?;
    crate::shared::ci::jobs::ensure_repo_read_access(&ctx.state, user, repository)
        .await
        .map_err(|_| anyhow::anyhow!("Repository read access required"))?;
    Ok(definitions
        .into_iter()
        .map(crate::shared::ci::responses::workflow_view)
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
    let loaded = if let Some((owner, name)) = scope {
        match crate::shared::repos::chrome::load_metadata(ctx, owner, name).await {
            Ok(loaded) => Some(loaded),
            Err(response) => return response,
        }
    } else {
        None
    };
    let mut message = message.to_string();
    let workflows = match &loaded {
        Some(loaded) => match refusal_workflows(ctx, &loaded.repository, &draft.branch).await {
            Ok(workflows) => workflows,
            Err(_) => {
                message.push_str(
                        " The workflows of this branch could not be read; open the repository's Actions page and run it from there.",
                    );
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    ctx.page_model_status(
        status,
        "Run was not accepted",
        codeyard_webui::app::Page::Actions(view::Page::DispatchRecovery {
            repo_chrome: loaded.map(|loaded| Box::new(loaded.chrome)),
            base_path: base_path.to_string(),
            csrf_token: ctx.csrf_token.clone(),
            message,
            draft: Box::new(draft),
            workflows,
        }),
    )
}

pub async fn dispatch(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    mut form: HashMap<String, String>,
    scope: Option<(String, String)>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.require_user() {
        return response;
    }
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let base_path = scope
        .as_ref()
        .map(|(owner, repo)| {
            format!(
                "/{}/{}/actions",
                codeyard_webui::encode_component(owner),
                codeyard_webui::encode_component(repo)
            )
        })
        .unwrap_or_else(|| "/ci".into());
    ctx.return_to = Some(base_path.clone());
    // Allow every individually valid input, including name=value and CRLF overhead.
    use codeyard_actions::workflow::dispatch_inputs::MAX_INPUTS;
    use codeyard_actions::workflow::dispatch_inputs::MAX_NAME_BYTES;
    use codeyard_actions::workflow::dispatch_inputs::MAX_VALUE_BYTES;
    const MAX_INPUT_LINES_BYTES: usize = MAX_INPUTS * (MAX_NAME_BYTES + 1 + MAX_VALUE_BYTES + 2);
    if form.iter().any(|(key, value)| {
        value.len()
            > if key == "return_query" {
                usize::MAX
            } else if key == "inputs" {
                MAX_INPUT_LINES_BYTES
            } else if key.starts_with("input:") {
                // Browsers send a textarea's line breaks as CRLF.
                2 * MAX_VALUE_BYTES
            } else {
                MAX_VALUE_BYTES
            }
    }) {
        return ctx.error(StatusCode::BAD_REQUEST, "Dispatch form value is too long.");
    }
    // Workflow files give their defaults LF line breaks, as YAML does, so an unchanged
    // multiline default reaches the run with its own bytes.
    for (key, value) in &mut form {
        if key.starts_with("input:") {
            *value = value.replace("\r\n", "\n");
            if value.len() > MAX_VALUE_BYTES {
                return ctx.error(StatusCode::BAD_REQUEST, "Dispatch form value is too long.");
            }
        }
    }
    let draft = draft(&form, scope.as_ref());
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
        // The rendered repository form sends its repository; a bad value matches none.
        repo_id: scope
            .as_ref()
            .and_then(|_| form.get("repository_id"))
            .map(|id| id.parse().unwrap_or(0)),
    };
    let actor_id = ctx.require_user().expect("user checked above").id;
    match ctx
        .state
        .ci_runs
        .dispatch_run(
            actor_id,
            request,
            crate::session::extract_session_id(&ctx.headers)
                .map(codeyard_db_core::queries::ci::CiCredential::Session),
        )
        .await
        .map_err(crate::shared::ci::admission_error)
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
