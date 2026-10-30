//! Check publishers share repository authority, but cannot overwrite another publisher's runs.
use super::{contents::store_error, pagination::Pagination, repo_url, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        checks::{self, Credential, Filter, Preference, Run, RunPatch, Suite, ACTIONS_APP_ID},
        models::Repository,
    },
    repositories::store::repo_store,
    webhooks::{self, payloads},
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
mod documents;

pub(in crate::backend::api) fn write_error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<checks::Refusal>() {
        Some(checks::Refusal::Missing) => Error::missing(),
        Some(checks::Refusal::Denied | checks::Refusal::Publisher) => {
            Error::new(StatusCode::FORBIDDEN, error.to_string())
        }
        Some(checks::Refusal::Invalid) => Error::invalid("Invalid check"),
        None => Error::unavailable(error),
    }
}

async fn app(pool: &db::DbPool, id: Option<i64>) -> Result<Value> {
    let Some(id) = id else { return Ok(Value::Null); };
    Ok(checks::publishers(pool, &[id]).await.map_err(Error::unavailable)?.first().map(app_value).unwrap_or(Value::Null))
}

fn app_value(app: &checks::Publisher) -> Value {
    let owner = payloads::Person { id: app.owner_id, login: app.owner_login.clone(), site_admin: app.owner_is_admin };
    json!({"id":app.id,"node_id":super::node_id::encode("A", app.id),"client_id":app.client_id,"name":app.name,"description":null,"owner":payloads::person(&owner),"external_url":app.homepage,"html_url":app.homepage,"created_at":app.created_at,"updated_at":app.created_at,"permissions":{"checks":"write","metadata":"read"},"events":["check_run","check_suite"]})
}

/// The app that publishes workflow run check suites, acting as the Actions bot account.
async fn actions_app(pool: &db::DbPool, repo: &Repository, owner: &str) -> Result<Value> {
    let Some(bot) = db::users::accounts::get_user_by_username(pool, db::ci_tokens::BOT)
        .await
        .map_err(Error::unavailable)?
    else {
        return Ok(Value::Null);
    };
    let url = format!(
        "{}/{}/{}/actions",
        crate::backend::api::http::config::link_origin(),
        crate::webui::encode_component(owner),
        crate::webui::encode_component(&repo.name)
    );
    Ok(
        json!({"id":ACTIONS_APP_ID,"slug":"github-actions","node_id":super::node_id::encode("A",ACTIONS_APP_ID),"name":"GitHub Actions","description":"Workflow runs","owner":payloads::user(&bot),"external_url":url,"html_url":url,"created_at":bot.created_at,"updated_at":bot.created_at,"permissions":{"actions":"write","checks":"write","contents":"write","metadata":"read","statuses":"write"},"events":["check_run","check_suite","push","pull_request","workflow_dispatch"]}),
    )
}

async fn pulls(pool: &db::DbPool, repo: &Repository, owner: &str, sha: &str) -> Result<Value> {
    let rows = checks::pulls(pool, repo.id, sha)
        .await
        .map_err(Error::unavailable)?;
    Ok(json!(rows.iter().map(|pull| pull_value(repo, owner, pull)).collect::<Vec<_>>()))
}

fn pull_value(repo: &Repository, owner: &str, pull: &checks::Pull) -> Value {
    let branch = |reference: &str, sha: &str| json!({"ref":reference,"sha":sha,"repo":{"id":repo.id,"name":repo.name,"url":repo_url(owner,&repo.name)}});
    json!({"id":pull.id,"number":pull.number,"url":format!("{}/pulls/{}",repo_url(owner,&repo.name),pull.number),"head":branch(&pull.head_branch,&pull.head_sha),"base":branch(&pull.base_branch,&pull.base_sha)})
}

async fn run_document(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    run: &Run,
) -> Result<Value> {
    documents::runs(state, repo, owner, std::slice::from_ref(run)).await?.pop().ok_or_else(Error::missing)
}

fn run_value(repo: &Repository, owner: &str, run: &Run, suite: &Suite, count: i64, publisher: &Value, pull_requests: &Value) -> Value {
    let url = format!("{}/check-runs/{}", repo_url(owner, &repo.name), run.id);
    json!({"id":run.id,"node_id":super::node_id::encode("CR",run.id),"head_sha":suite.head_sha,"name":run.name,"url":url,"html_url":format!("{}/{}/{}/runs/{}",crate::backend::api::http::config::link_origin(),crate::webui::encode_component(owner),crate::webui::encode_component(&repo.name),run.id),"details_url":run.data.details_url,"external_id":run.data.external_id,"status":run.status,"conclusion":run.conclusion,"started_at":run.data.started_at,"completed_at":run.data.completed_at,"check_suite":{"id":suite.id},"output":{"title":run.data.title,"summary":run.data.summary,"text":run.data.text,"annotations_count":count,"annotations_url":format!("{url}/annotations"),"images":run.data.images},"app":publisher,"pull_requests":pull_requests})
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
    suite_document_at(&state.pool, repo, owner, suite, &payloads::simple_commit(&commit)).await
}

async fn suite_document_at(
    pool: &db::DbPool,
    repo: &Repository,
    owner: &str,
    suite: &Suite,
    head_commit: &Value,
) -> Result<Value> {
    let count = checks::run_count(
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
            let run = db::ci::get_ci_run(pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            let (status, conclusion) = db::action_runs::run_state(&run);
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
    Ok(suite_value(repo, owner, suite, count, (&status, conclusion.as_deref()), SuiteDocuments { publisher: &publisher, head_commit, pull_requests: &pull_requests }))
}

struct SuiteDocuments<'a> {
    publisher: &'a Value,
    head_commit: &'a Value,
    pull_requests: &'a Value,
}

fn suite_value(repo: &Repository, owner: &str, suite: &Suite, count: i64, (status, conclusion): (&str, Option<&str>), documents: SuiteDocuments<'_>) -> Value {
    let SuiteDocuments { publisher, head_commit, pull_requests } = documents;
    let url = format!("{}/check-suites/{}", repo_url(owner, &repo.name), suite.id);
    json!({"id":suite.id,"node_id":super::node_id::encode("CSU",suite.id),"head_sha":suite.head_sha,"head_branch":suite.head_branch,"status":status,"conclusion":conclusion,"url":url,"before":null,"after":null,"created_at":suite.created_at,"updated_at":suite.updated_at,"repository":payloads::repository(repo,owner),"latest_check_runs_count":count,"check_runs_url":format!("{url}/check-runs"),"app":publisher,"head_commit":head_commit,"pull_requests":pull_requests,"rerequestable":true,"runs_rerequestable":true})
}

/// Uses the bounded head snapshot captured on the store that accepted the publication. The
/// push is already accepted, so a failure here is logged rather than returned.
pub(crate) async fn requested_after_push(
    pool: &db::DbPool,
    repo: &Repository,
    actor: i64,
    sha: &str,
    branch: &str,
    head: &payloads::PushedCommit,
) {
    if let Err(error) = request_after_push(pool, repo, actor, sha, branch, head).await {
        tracing::warn!(repo_id = repo.id, ?error, "Automatic check suites could not be requested");
    }
}

async fn request_after_push(
    pool: &db::DbPool,
    repo: &Repository,
    actor: i64,
    sha: &str,
    branch: &str,
    head: &payloads::PushedCommit,
) -> Result<()> {
    if !head.id.eq_ignore_ascii_case(sha) {
        return Err(Error::unavailable("Accepted push did not retain the expected head"));
    }
    let head_commit = payloads::pushed_head(head);
    let mut after = 0;
    let mut owner = None;
    loop {
        let page = checks::automatic_suites(pool, repo, sha, branch, after).await.map_err(write_error)?;
        after = page.after;
        if !page.suites.is_empty() && owner.is_none() {
            owner = Some(db::repositories::records::repository_owner_name(pool, repo.id).await.map_err(Error::unavailable)?.ok_or_else(Error::missing)?);
        }
        if let Some(owner) = &owner {
            let mut heads = std::collections::HashMap::from([(sha.to_ascii_lowercase(), head_commit.clone())]);
            for doc in documents::suites_at(pool, repo, owner, &page.suites, &mut heads).await? {
                emit_for_pool(pool, repo, actor, "check_suite", "requested", doc).await;
            }
        }
        if !page.has_next { break; }
    }
    Ok(())
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
async fn head(state: &AppState, repo: &Repository, sha: &str) -> Result<(String, Option<String>)> {
    if !crate::backend::git::is_full_commit_id(sha) {
        return Err(Error::invalid("head_sha must be a full commit ID"));
    }
    let store = repo_store(repo, state.cluster.as_ref());
    let sha = store.commit(sha).await.map_err(store_error)?.sha;
    let refs = store.refs().await.map_err(store_error)?;
    let mut branches: Vec<_> = refs
        .into_iter()
        .filter(|(_, tip)| *tip == sha)
        .filter_map(|(name, _)| name.strip_prefix("refs/heads/").map(str::to_owned))
        .collect();
    branches.sort();
    let branch = branches
        .iter()
        .find(|branch| **branch == repo.default_branch)
        .cloned()
        .or_else(|| branches.into_iter().next());
    Ok((sha, branch))
}
async fn create_suite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<CreateSuite>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(super::auth::required)?;
    let (sha, branch) = head(&state, &repo, &input.head_sha).await?;
    let (suite, created) = checks::create_suite(
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
    let auth = auth.ok_or_else(super::auth::required)?;
    input.patch.validate(true).map_err(write_error)?;
    let (sha, branch) = head(&state, &repo, &input.head_sha).await?;
    let changed = checks::create_run(
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
    let auth = auth.ok_or_else(super::auth::required)?;
    let changed = checks::update_run(
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
    if let Some(run) = checks::run(&state.pool, repo.id, id).await.map_err(Error::unavailable)? {
        return Ok(Json(run_document(&state, &repo, &owner, &run).await?));
    }
    let job = db::action_jobs::by_check_run(&state.pool, repo.id, id).await.map_err(Error::unavailable)?.ok_or_else(Error::missing)?;
    let run = db::ci::get_ci_run_by_id(&state.pool, repo.id, job.run_id).await.map_err(Error::unavailable)?.ok_or_else(Error::missing)?;
    let suite = db::action_runs::metadata(&state.pool, repo.id, &[run.id]).await.map_err(Error::unavailable)?
        .into_iter().next().and_then(|metadata| metadata.check_suite_id).ok_or_else(Error::missing)?;
    let (count, _) = db::action_jobs::annotations(&state.pool, repo.id, job.id, 0, 1).await.map_err(Error::unavailable)?;
    let job_document = payloads::action_job(&job, &[], &run, &owner, &repo.name);
    let url = format!("{}/check-runs/{id}", repo_url(&owner, &repo.name));
    Ok(Json(json!({"id":id,"node_id":super::node_id::encode("CR",id),"head_sha":run.head_sha,"name":job.name,"url":url,
        "html_url":job_document["html_url"],"details_url":job_document["html_url"],"external_id":null,
        "status":job.status,"conclusion":job.conclusion,"started_at":job.started_at,"completed_at":job.completed_at,
        "check_suite":{"id":suite},"output":{"title":job.name,"summary":null,"text":null,"annotations_count":count,"annotations_url":format!("{url}/annotations"),"images":[]},
        "app":actions_app(&state.pool,&repo,&owner).await?,"pull_requests":pulls(&state.pool,&repo,&owner,run.head_sha.as_deref().unwrap_or("")).await?})))
}
async fn get_suite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let suite = checks::suite(&state.pool, repo.id, id)
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
    checks::suite(&state.pool, repo.id, id)
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
        let count = checks::suite_count(&state.pool, repo.id, filter)
            .await
            .map_err(Error::unavailable)?;
        let rows = checks::suites(&state.pool, repo.id, filter, offset as i64, limit as i64)
            .await
            .map_err(Error::unavailable)?;
        let docs = documents::suites(state, repo, owner, &rows).await?;
        (count, "check_suites", docs)
    } else {
        let count = checks::run_count(&state.pool, repo.id, filter)
            .await
            .map_err(Error::unavailable)?;
        let rows = checks::runs(&state.pool, repo.id, filter, offset as i64, limit as i64)
            .await
            .map_err(Error::unavailable)?;
        let docs = documents::runs(state, repo, owner, &rows).await?;
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
    let Some(run) = checks::run(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
    else {
        let job = db::action_jobs::by_check_run(&state.pool, repo.id, id).await.map_err(Error::unavailable)?.ok_or_else(Error::missing)?;
        return super::actions::job_annotations(
            State(state),
            headers,
            Path((owner, name, job.id)),
            Query(page),
            OriginalUri(uri),
        )
        .await;
    };
    let suite = checks::suite(&state.pool, repo.id, run.suite_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let (offset, limit) = page.window()?;
    let count = checks::annotation_count(&state.pool, id)
        .await
        .map_err(Error::unavailable)?;
    let docs = checks::annotations(&state.pool, id, offset as i64, limit as i64)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|a| {
            let mut doc = json!(a.0);
            let base = format!("{}/{}/{}", crate::backend::api::http::config::link_origin(), crate::webui::encode_component(&owner), crate::webui::encode_component(&name));
            doc["blob_href"] = json!(crate::webui::pages::repos::ref_path::file_url(&base, "blob", &suite.head_sha, &a.path));
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
    let auth = auth.ok_or_else(super::auth::required)?;
    let is_run = uri.path().contains("/check-runs/");
    let (suite, run) = if is_run {
        let run = checks::run(&state.pool, repo.id, id)
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
pub(in crate::backend::api) async fn request_again(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    actor: i64,
    credential: Credential<'_>,
    ci_credential: db::ci::CiCredential,
    suite: i64,
    run: Option<&Run>,
) -> Result<()> {
    let stored = checks::suite(&state.pool, repo.id, suite)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    if let Some(ci_run) = stored.ci_run_id {
        // An Actions suite shows its workflow run, so rerequesting it queues another attempt of that run.
        let credential = Some(ci_credential);
        // Each attempt has its own suite and CI run; reruns go through the shared workflow run.
        let workflow_run = db::ci::get_ci_run_by_id(&state.pool, repo.id, ci_run)
            .await
            .map_err(Error::unavailable)?
            .map(|run| run.workflow_run_id.unwrap_or(run.id))
            .ok_or_else(Error::missing)?;
        state
            .ci_runs
            .rerun_workflow_run(repo.id, workflow_run, actor, false, credential)
            .await
            .map_err(super::actions::admission_error)?;
        return Ok(());
    }
    checks::rerequest(
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
        let suite = checks::suite(&state.pool, repo.id, suite)
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
pub(in crate::backend::api) async fn request_action(
    state: &Arc<AppState>, repo: &Repository, owner: &str, actor: i64,
    credential: Credential<'_>, run: i64, identifier: String,
) -> Result<()> {
    let run = checks::requested_action(&state.pool, credential, repo.id, run, &identifier).await.map_err(write_error)?;
    let (state, repo, owner) = (Arc::clone(state), repo.clone(), owner.to_owned());
    tokio::spawn(async move {
        let doc = run_document(&state, &repo, &owner, &run).await?;
        let repository = repo.clone();
        webhooks::emit(&state.pool, &repo, "check_run", vec![actor], move |owner, people| json!({"action":"requested_action", "requested_action":{"identifier":identifier}, "check_run":doc, "repository":payloads::repository(&repository, owner), "sender":payloads::person(&people.get(actor))})).await;
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
    let auth = auth.ok_or_else(super::auth::required)?;
    let saved = checks::preferences(
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
async fn emit(
    state: &AppState,
    repo: &Repository,
    actor: i64,
    event: &'static str,
    action: &'static str,
    doc: Value,
) {
    emit_for_pool(&state.pool, repo, actor, event, action, doc).await;
}

async fn emit_for_pool(pool: &db::DbPool, repo: &Repository, actor: i64, event: &'static str, action: &'static str, doc: Value) {
    let repository = repo.clone();
    webhooks::emit(pool,repo,event,vec![actor],move |owner,people| json!({"action":action,event:doc,"repository":payloads::repository(&repository,owner),"sender":payloads::person(&people.get(actor))})).await;
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
