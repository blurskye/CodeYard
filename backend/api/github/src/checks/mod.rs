//! Check publishers share repository authority, but cannot overwrite another publisher's runs.
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::contents::store_error;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_app::repositories::store::repo_store;
use codeyard_webhooks::payloads;
use codeyard_db_actions::checks::Credential;
use codeyard_db_actions::checks::Filter;
use codeyard_db_actions::checks::Preference;
use codeyard_db_actions::checks::Run;
use codeyard_db_actions::checks::RunPatch;
use codeyard_db_actions::checks::Suite;
use codeyard_db_core::models::Repository;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

mod documents;

async fn run_document(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    run: &Run,
) -> Result<Value> {
    crate::checks::documents::runs(state, repo, owner, std::slice::from_ref(run))
        .await?
        .pop()
        .ok_or_else(Error::missing)
}

fn run_value(
    repo: &Repository,
    owner: &str,
    run: &Run,
    suite: &Suite,
    count: i64,
    publisher: &Value,
    pull_requests: &Value,
) -> Value {
    let url = format!("{}/check-runs/{}", repo_url(owner, &repo.name), run.id);
    json!({"id":run.id,"node_id":codeyard_model::node_id::encode("CR",run.id),"head_sha":suite.head_sha,"name":run.name,"url":url,"html_url":format!("{}/{}/{}/runs/{}",codeyard_model::http_config::link_origin(),codeyard_webui::encode_component(owner),codeyard_webui::encode_component(&repo.name),run.id),"details_url":run.data.details_url,"external_id":run.data.external_id,"status":run.status,"conclusion":run.conclusion,"started_at":run.data.started_at,"completed_at":run.data.completed_at,"check_suite":{"id":suite.id},"output":{"title":run.data.title,"summary":run.data.summary,"text":run.data.text,"annotations_count":count,"annotations_url":format!("{url}/annotations"),"images":run.data.images},"app":publisher,"pull_requests":pull_requests})
}

async fn suite_document(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    suite: &Suite,
) -> Result<Value> {
    let commit = repo_store(repo, state.cluster.as_ref())
        .commit(&suite.head_sha)
        .await
        .map_err(store_error)?;
    suite_document_at(
        &state.pool,
        repo,
        owner,
        suite,
        &codeyard_webhooks::payloads::simple_commit(&commit),
    )
    .await
}

async fn suite_document_at(
    pool: &codeyard_db_core::DbPool,
    repo: &Repository,
    owner: &str,
    suite: &Suite,
    head_commit: &Value,
) -> Result<Value> {
    let count = codeyard_db_actions::checks::run_count(
        pool,
        repo.id,
        &Filter {
            suite_id: Some(suite.id),
            ..Filter::default()
        },
    )
    .await
    .map_err(Error::unavailable)?;
    // A workflow run's suite has the run's state and is published by Actions.
    let (status, conclusion, publisher) = match suite.ci_run_id {
        Some(id) => {
            let run = codeyard_db_actions::ci::get_ci_run(pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            let (status, conclusion) = codeyard_db_actions::action_runs::run_state(&run);
            (
                status.to_owned(),
                conclusion.map(str::to_owned),
                actions_app(pool, repo, owner).await?,
            )
        }
        None => (
            suite.status.clone(),
            suite.conclusion.clone(),
            app(pool, suite.app_id).await?,
        ),
    };
    let pull_requests = pulls(pool, repo, owner, &suite.head_sha).await?;
    Ok(suite_value(
        repo,
        owner,
        suite,
        count,
        (&status, conclusion.as_deref()),
        SuiteDocuments {
            publisher: &publisher,
            head_commit,
            pull_requests: &pull_requests,
        },
    ))
}

#[derive(Deserialize)]
struct CreateSuite {
    head_sha: String,
}
#[derive(Deserialize)]
struct CreateRun {
    head_sha: String,
    #[serde(flatten)]
    patch: RunPatch,
}

async fn create_suite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<CreateSuite>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let (sha, branch) = head(&state, &repo, &input.head_sha).await?;
    let (suite, created) = codeyard_db_actions::checks::create_suite(
        &state.pool,
        Credential::Token(auth.token.id),
        repo.id,
        &sha,
        branch.as_deref(),
    )
    .await
    .map_err(write_error)?;
    let doc = suite_document(&state, &repo, &owner, &suite).await?;
    if created {
        emit(
            &state,
            &repo,
            auth.user.id,
            "check_suite",
            "requested",
            doc.clone(),
        )
        .await;
    }
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(doc),
    )
        .into_response())
}
async fn create_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<CreateRun>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    input.patch.validate(true).map_err(write_error)?;
    let (sha, branch) = head(&state, &repo, &input.head_sha).await?;
    let changed = codeyard_db_actions::checks::create_run(
        &state.pool,
        Credential::Token(auth.token.id),
        repo.id,
        &sha,
        branch.as_deref(),
        &input.patch,
    )
    .await
    .map_err(write_error)?;
    let doc = run_document(&state, &repo, &owner, &changed.run).await?;
    emit(
        &state,
        &repo,
        auth.user.id,
        "check_run",
        "created",
        doc.clone(),
    )
    .await;
    if let Some(suite) = changed.completed_suite {
        let doc = suite_document(&state, &repo, &owner, &suite).await?;
        emit(&state, &repo, auth.user.id, "check_suite", "completed", doc).await;
    }
    Ok((StatusCode::CREATED, Json(doc)).into_response())
}
async fn update_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Json(input): Json<RunPatch>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let changed = codeyard_db_actions::checks::update_run(
        &state.pool,
        Credential::Token(auth.token.id),
        repo.id,
        id,
        &input,
    )
    .await
    .map_err(write_error)?;
    let doc = run_document(&state, &repo, &owner, &changed.run).await?;
    if changed.run.status == "completed" {
        emit(
            &state,
            &repo,
            auth.user.id,
            "check_run",
            "completed",
            doc.clone(),
        )
        .await;
    }
    if let Some(suite) = changed.completed_suite {
        let doc = suite_document(&state, &repo, &owner, &suite).await?;
        emit(&state, &repo, auth.user.id, "check_suite", "completed", doc).await;
    }
    Ok(Json(doc))
}
async fn get_run(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    if let Some(run) = codeyard_db_actions::checks::run(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(Json(run_document(&state, &repo, &owner, &run).await?));
    }
    let job = codeyard_db_actions::action_jobs::by_check_run(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let run = codeyard_db_actions::ci::get_ci_run_by_id(&state.pool, repo.id, job.run_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let suite = codeyard_db_actions::action_runs::metadata(&state.pool, repo.id, &[run.id])
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .next()
        .and_then(|metadata| metadata.check_suite_id)
        .ok_or_else(Error::missing)?;
    let (count, _) =
        codeyard_db_actions::action_jobs::annotations(&state.pool, repo.id, job.id, 0, 1)
            .await
            .map_err(Error::unavailable)?;
    let job_document =
        codeyard_webhooks::payloads::action_job(&job, &[], &run, &owner, &repo.name);
    let url = format!("{}/check-runs/{id}", repo_url(&owner, &repo.name));
    Ok(Json(
        json!({"id":id,"node_id":codeyard_model::node_id::encode("CR",id),"head_sha":run.head_sha,"name":job.name,"url":url,
        "html_url":job_document["html_url"],"details_url":job_document["html_url"],"external_id":null,
        "status":job.status,"conclusion":job.conclusion,"started_at":job.started_at,"completed_at":job.completed_at,
        "check_suite":{"id":suite},"output":{"title":job.name,"summary":null,"text":null,"annotations_count":count,"annotations_url":format!("{url}/annotations"),"images":[]},
        "app":actions_app(&state.pool,&repo,&owner).await?,"pull_requests":pulls(&state.pool,&repo,&owner,run.head_sha.as_deref().unwrap_or("")).await?}),
    ))
}
async fn get_suite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let suite = codeyard_db_actions::checks::suite(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(suite_document(&state, &repo, &owner, &suite).await?))
}

#[derive(Deserialize)]
struct List {
    page: Option<usize>,
    per_page: Option<usize>,
    check_name: Option<String>,
    status: Option<String>,
    filter: Option<String>,
    app_id: Option<i64>,
}
impl List {
    fn pagination(&self) -> Pagination {
        Pagination {
            page: self.page,
            per_page: self.per_page,
        }
    }
    fn filter(&self) -> Result<Filter> {
        if !self.status.as_deref().is_none_or(|s| {
            matches!(
                s,
                "queued" | "in_progress" | "completed" | "waiting" | "requested" | "pending"
            )
        }) || !self
            .filter
            .as_deref()
            .is_none_or(|s| matches!(s, "all" | "latest"))
        {
            return Err(Error::invalid("Invalid check filter"));
        }
        Ok(Filter {
            name: self.check_name.clone(),
            status: self.status.clone(),
            app_id: self.app_id,
            all: self.filter.as_deref() == Some("all"),
            ..Filter::default()
        })
    }
}
async fn list_for_ref(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, reference)): Path<(String, String, String)>,
    Query(input): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let mut filter = input.filter()?;
    filter.head_sha = Some(
        repo_store(&repo, state.cluster.as_ref())
            .commit(&reference)
            .await
            .map_err(store_error)?
            .sha,
    );
    list(
        &state,
        &repo,
        &owner,
        input.pagination(),
        &uri,
        &filter,
        uri.path().ends_with("check-suites"),
    )
    .await
}
async fn list_for_suite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(input): Query<List>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    codeyard_db_actions::checks::suite(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut filter = input.filter()?;
    filter.suite_id = Some(id);
    list(
        &state,
        &repo,
        &owner,
        input.pagination(),
        &uri,
        &filter,
        false,
    )
    .await
}
async fn list(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    page: Pagination,
    uri: &axum::http::Uri,
    filter: &Filter,
    suites: bool,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let (count, key, docs) = if suites {
        let count = codeyard_db_actions::checks::suite_count(&state.pool, repo.id, filter)
            .await
            .map_err(Error::unavailable)?;
        let rows = codeyard_db_actions::checks::suites(
            &state.pool,
            repo.id,
            filter,
            offset as i64,
            limit as i64,
        )
        .await
        .map_err(Error::unavailable)?;
        let docs =
            codeyard_app::api_shared::github::checks::documents::suites(state, repo, owner, &rows)
                .await?;
        (count, "check_suites", docs)
    } else {
        let count = codeyard_db_actions::checks::run_count(&state.pool, repo.id, filter)
            .await
            .map_err(Error::unavailable)?;
        let rows = codeyard_db_actions::checks::runs(
            &state.pool,
            repo.id,
            filter,
            offset as i64,
            limit as i64,
        )
        .await
        .map_err(Error::unavailable)?;
        let docs = crate::checks::documents::runs(state, repo, owner, &rows).await?;
        (count, "check_runs", docs)
    };
    Ok((
        page.headers(uri, count > (offset + limit) as i64)?,
        Json(json!({"total_count":count,key:docs})),
    )
        .into_response())
}
async fn annotations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let Some(run) = codeyard_db_actions::checks::run(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
    else {
        let job = codeyard_db_actions::action_jobs::by_check_run(&state.pool, repo.id, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        return crate::actions::jobs::annotations(
            State(state),
            headers,
            Path((owner, name, job.id)),
            Query(page),
            OriginalUri(uri),
        )
        .await;
    };
    let suite = codeyard_db_actions::checks::suite(&state.pool, repo.id, run.suite_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let (offset, limit) = page.window()?;
    let count = codeyard_db_actions::checks::annotation_count(&state.pool, id)
        .await
        .map_err(Error::unavailable)?;
    let docs =
        codeyard_db_actions::checks::annotations(&state.pool, id, offset as i64, limit as i64)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|a| {
                let mut doc = json!(a.0);
                let base = format!(
                    "{}/{}/{}",
                    codeyard_model::http_config::link_origin(),
                    codeyard_webui::encode_component(&owner),
                    codeyard_webui::encode_component(&name)
                );
                doc["blob_href"] = json!(codeyard_webui::pages::repos::ref_path::file_url(
                    &base,
                    "blob",
                    &suite.head_sha,
                    &a.path
                ));
                doc
            })
            .collect();
    page.response(&uri, docs, count > (offset + limit) as i64)
}
async fn rerequest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let is_run = uri.path().contains("/check-runs/");
    let (suite, run) = if is_run {
        let run = codeyard_db_actions::checks::run(&state.pool, repo.id, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        (run.suite_id, Some(run))
    } else {
        (id, None)
    };
    request_again(
        &state,
        &repo,
        &owner,
        auth.workflow_actor(&state, repo.id).await?,
        Credential::Token(auth.token.id),
        auth.ci_credential(&headers, "actions")?,
        suite,
        run.as_ref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({}))).into_response())
}
#[expect(
    clippy::too_many_arguments,
    reason = "Rerequesting a suite needs the caller's repository, both credentials and the suite's run."
)]
pub async fn request_again(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    actor: i64,
    credential: Credential<'_>,
    ci_credential: codeyard_db_core::queries::ci::CiCredential,
    suite: i64,
    run: Option<&Run>,
) -> Result<()> {
    let stored = codeyard_db_actions::checks::suite(&state.pool, repo.id, suite)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if let Some(ci_run) = stored.ci_run_id {
        // An Actions suite shows its workflow run, so rerequesting it queues another attempt of that run.
        let credential = Some(ci_credential);
        // Each attempt has its own suite and CI run; reruns go through the shared workflow run.
        let workflow_run = codeyard_db_actions::ci::get_ci_run_by_id(&state.pool, repo.id, ci_run)
            .await
            .map_err(Error::unavailable)?
            .map(|run| run.workflow_run_id.unwrap_or(run.id))
            .ok_or_else(Error::missing)?;
        state
            .ci_runs
            .rerun_workflow_run(repo.id, workflow_run, actor, false, credential)
            .await
            .map_err(crate::actions::workflows::admission_error)?;
        return Ok(());
    }
    codeyard_db_actions::checks::rerequest(
        &state.pool,
        credential,
        repo.id,
        suite,
        run.map(|run| run.id),
    )
    .await
    .map_err(write_error)?;
    let (event, doc) = if let Some(run) = run {
        ("check_run", run_document(state, repo, owner, run).await?)
    } else {
        let suite = codeyard_db_actions::checks::suite(&state.pool, repo.id, suite)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        (
            "check_suite",
            suite_document(state, repo, owner, &suite).await?,
        )
    };
    emit(state, repo, actor, event, "rerequested", doc).await;
    Ok(())
}
/// The accepted action and its delivery finish even if the request is cancelled.
pub async fn request_action(
    state: &Arc<AppState>,
    repo: &Repository,
    owner: &str,
    actor: i64,
    credential: Credential<'_>,
    run: i64,
    identifier: String,
) -> Result<()> {
    let run = codeyard_db_actions::checks::requested_action(
        &state.pool,
        credential,
        repo.id,
        run,
        &identifier,
    )
    .await
    .map_err(write_error)?;
    let (state, repo, owner) = (Arc::clone(state), repo.clone(), owner.to_owned());
    tokio::spawn(async move {
        let doc = run_document(&state, &repo, &owner, &run).await?;
        let repository = repo.clone();
        codeyard_app::webhooks::emit(&state.pool, &repo, "check_run", vec![actor], move |owner, people| json!({"action":"requested_action", "requested_action":{"identifier":identifier}, "check_run":doc, "repository":payloads::repository(&repository, owner), "sender":payloads::person(&people.get(actor))})).await;
        Ok(())
    }).await.map_err(Error::unavailable)?
}

#[derive(Deserialize)]
struct Preferences {
    #[serde(default)]
    auto_trigger_checks: Vec<Preference>,
}
async fn preferences(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Preferences>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let saved = codeyard_db_actions::checks::preferences(
        &state.pool,
        Credential::Token(auth.token.id),
        repo.id,
        &input.auto_trigger_checks,
    )
    .await
    .map_err(write_error)?;
    Ok(Json(
        json!({"preferences":{"auto_trigger_checks":saved},"repository":payloads::repository(&repo,&owner)}),
    ))
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("checks/create-suite", post(create_suite)),
        ("checks/create", post(create_run)),
        ("checks/get-suite", get(get_suite)),
        ("checks/get", get(get_run)),
        ("checks/list-annotations", get(annotations)),
        ("checks/list-for-ref", get(list_for_ref)),
        ("checks/list-for-suite", get(list_for_suite)),
        ("checks/list-suites-for-ref", get(list_for_ref)),
        ("checks/rerequest-run", post(rerequest)),
        ("checks/rerequest-suite", post(rerequest)),
        ("checks/set-suites-preferences", patch(preferences)),
        ("checks/update", patch(update_run)),
    ]
}

use codeyard_app::api_shared::github::checks::SuiteDocuments;

use codeyard_app::api_shared::github::checks::actions_app;

use codeyard_app::api_shared::github::checks::app;

use codeyard_app::api_shared::github::checks::emit;

use codeyard_app::api_shared::github::checks::head;

use codeyard_app::api_shared::github::checks::pulls;

use codeyard_app::api_shared::github::checks::suite_value;

use codeyard_app::api_shared::github::checks::write_error;
