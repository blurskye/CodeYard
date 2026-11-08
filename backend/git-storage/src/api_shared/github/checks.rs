//! Check publishers share repository authority, but cannot overwrite another publisher's runs.
use crate::api_shared::github::contents::store_error;
use crate::repositories::store::repo_store;
use crate::request_context::RequestContext;
use axum::http::StatusCode;
use codeyard_database::models::Repository;
use codeyard_db_actions::checks::Suite;
use codeyard_db_actions::checks::ACTIONS_APP_ID;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use codeyard_types::urls::repo_url;
use codeyard_webhooks::payloads;
use serde_json::json;
use serde_json::Value;
pub fn write_error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<codeyard_db_actions::checks::Refusal>() {
        Some(codeyard_db_actions::checks::Refusal::Missing) => Error::missing(),
        Some(
            codeyard_db_actions::checks::Refusal::Denied
            | codeyard_db_actions::checks::Refusal::Publisher,
        ) => Error::new(StatusCode::FORBIDDEN, error.to_string()),
        Some(codeyard_db_actions::checks::Refusal::Invalid) => Error::invalid("Invalid check"),
        None => Error::unavailable(error),
    }
}

pub async fn app(pool: &codeyard_database::DbPool, id: Option<i64>) -> Result<Value> {
    let Some(id) = id else {
        return Ok(Value::Null);
    };
    Ok(codeyard_db_actions::checks::publishers(pool, &[id])
        .await
        .map_err(Error::unavailable)?
        .first()
        .map(app_value)
        .unwrap_or(Value::Null))
}

fn app_value(app: &codeyard_db_actions::checks::Publisher) -> Value {
    let owner = codeyard_webhooks::payloads::Person {
        id: app.owner_id,
        login: app.owner_login.clone(),
        site_admin: app.owner_is_admin,
    };
    json!({"id":app.id,"node_id":codeyard_types::node_id::scoped("A", app.owner_id, app.id),"client_id":app.client_id,"name":app.name,"description":null,"owner":payloads::person(&owner),"external_url":app.homepage,"html_url":app.homepage,"created_at":app.created_at,"updated_at":app.created_at,"permissions":{"checks":"write","metadata":"read"},"events":["check_run","check_suite"]})
}

/// The app that publishes workflow run check suites, acting as the Actions bot account.
pub async fn actions_app(
    pool: &codeyard_database::DbPool,
    repo: &Repository,
    owner: &str,
) -> Result<Value> {
    let Some(bot) = codeyard_db_identity::users::accounts::get_user_by_username(
        pool,
        codeyard_database::queries::ci_tokens::BOT,
    )
    .await
    .map_err(Error::unavailable)?
    else {
        return Ok(Value::Null);
    };
    let url = format!(
        "{}/{}/{}/actions",
        codeyard_types::http_config::link_origin(),
        codeyard_types::paths::encode_component(owner),
        codeyard_types::paths::encode_component(&repo.name)
    );
    Ok(
        json!({"id":ACTIONS_APP_ID,"slug":"github-actions","node_id":codeyard_types::node_id::scoped("A",bot.id,ACTIONS_APP_ID),"name":"GitHub Actions","description":"Workflow runs","owner":payloads::user(&bot),"external_url":url,"html_url":url,"created_at":bot.created_at,"updated_at":bot.created_at,"permissions":{"actions":"write","checks":"write","contents":"write","metadata":"read","statuses":"write"},"events":["check_run","check_suite","push","pull_request","workflow_dispatch"]}),
    )
}

pub async fn pulls(
    pool: &codeyard_database::DbPool,
    repo: &Repository,
    owner: &str,
    sha: &str,
) -> Result<Value> {
    let rows = codeyard_db_actions::checks::pulls(pool, repo.id, sha)
        .await
        .map_err(Error::unavailable)?;
    Ok(json!(rows
        .iter()
        .map(|pull| pull_value(repo, owner, pull))
        .collect::<Vec<_>>()))
}

fn pull_value(repo: &Repository, owner: &str, pull: &codeyard_db_actions::checks::Pull) -> Value {
    let branch = |reference: &str, sha: &str| json!({"ref":reference,"sha":sha,"repo":{"id":repo.id,"name":repo.name,"url":repo_url(owner,&repo.name)}});
    json!({"id":pull.id,"number":pull.number,"url":format!("{}/pulls/{}",repo_url(owner,&repo.name),pull.number),"head":branch(&pull.head_branch,&pull.head_sha),"base":branch(&pull.base_branch,&pull.base_sha)})
}

pub struct SuiteDocuments<'a> {
    pub publisher: &'a Value,
    pub head_commit: &'a Value,
    pub pull_requests: &'a Value,
}

pub fn suite_value(
    repo: &Repository,
    owner: &str,
    suite: &Suite,
    count: i64,
    (status, conclusion): (&str, Option<&str>),
    documents: SuiteDocuments<'_>,
) -> Value {
    let SuiteDocuments {
        publisher,
        head_commit,
        pull_requests,
    } = documents;
    let url = format!("{}/check-suites/{}", repo_url(owner, &repo.name), suite.id);
    json!({"id":suite.id,"node_id":codeyard_types::node_id::scoped("CSU",repo.id,suite.id),"head_sha":suite.head_sha,"head_branch":suite.head_branch,"status":status,"conclusion":conclusion,"url":url,"before":null,"after":null,"created_at":suite.created_at,"updated_at":suite.updated_at,"repository":payloads::repository(repo,owner),"latest_check_runs_count":count,"check_runs_url":format!("{url}/check-runs"),"app":publisher,"head_commit":head_commit,"pull_requests":pull_requests,"rerequestable":true,"runs_rerequestable":true})
}

/// Uses the bounded head snapshot captured on the store that accepted the publication. The
/// push is already accepted, so a failure here is logged rather than returned.
pub(crate) async fn requested_after_push(
    pool: &codeyard_database::DbPool,
    repo: &Repository,
    actor: i64,
    sha: &str,
    branch: &str,
    head: &codeyard_webhooks::payloads::PushedCommit,
) {
    if let Err(error) = request_after_push(pool, repo, actor, sha, branch, head).await {
        tracing::warn!(
            repo_id = repo.id,
            ?error,
            "Automatic check suites could not be requested"
        );
    }
}

async fn request_after_push(
    pool: &codeyard_database::DbPool,
    repo: &Repository,
    actor: i64,
    sha: &str,
    branch: &str,
    head: &codeyard_webhooks::payloads::PushedCommit,
) -> Result<()> {
    if !head.id.eq_ignore_ascii_case(sha) {
        return Err(Error::unavailable(
            "Accepted push did not retain the expected head",
        ));
    }
    let head_commit = codeyard_webhooks::payloads::pushed_head(head);
    let mut after = 0;
    let mut owner = None;
    loop {
        let page = codeyard_db_actions::checks::automatic_suites(pool, repo, sha, branch, after)
            .await
            .map_err(write_error)?;
        after = page.after;
        if !page.suites.is_empty() && owner.is_none() {
            owner = Some(
                codeyard_db_repos::repositories::records::repository_owner_name(pool, repo.id)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(Error::missing)?,
            );
        }
        if let Some(owner) = &owner {
            let mut heads =
                std::collections::HashMap::from([(sha.to_ascii_lowercase(), head_commit.clone())]);
            for doc in crate::api_shared::github::checks::documents::suites_at(
                pool,
                repo,
                owner,
                &page.suites,
                &mut heads,
            )
            .await?
            {
                emit_for_pool(pool, repo, actor, "check_suite", "requested", doc).await;
            }
        }
        if !page.has_next {
            break;
        }
    }
    Ok(())
}

pub async fn head(
    state: &dyn RequestContext,
    repo: &Repository,
    sha: &str,
) -> Result<(String, Option<String>)> {
    if !codeyard_git::is_full_commit_id(sha) {
        return Err(Error::invalid("head_sha must be a full commit ID"));
    }
    let store = repo_store(repo, state.cluster());
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

pub async fn emit(
    state: &dyn RequestContext,
    repo: &Repository,
    actor: i64,
    event: &'static str,
    action: &'static str,
    doc: Value,
) {
    emit_for_pool(state.pool(), repo, actor, event, action, doc).await;
}

async fn emit_for_pool(
    pool: &codeyard_database::DbPool,
    repo: &Repository,
    actor: i64,
    event: &'static str,
    action: &'static str,
    doc: Value,
) {
    let repository = repo.clone();
    crate::webhooks::emit(pool,repo,event,vec![actor],move |owner,people| json!({"action":action,event:doc,"repository":payloads::repository(&repository,owner),"sender":payloads::person(&people.get(actor))})).await;
}

pub mod documents;
