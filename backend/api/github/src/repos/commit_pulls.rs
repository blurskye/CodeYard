//! Commit association reads share the native ancestry query and saved discussion metadata.
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::routing::MethodRouter;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_repos_pulls::repositories::commit_pulls::AssociationError;
use codeyard_repos_pulls::repositories::commit_pulls::CommitPullQuery;
use codeyard_db_core::models::PullRequest;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

fn operation_error(error: AssociationError) -> Error {
    match error {
        AssociationError::Git(error) => store_error(error),
        AssociationError::Access(
            codeyard_identity::repository_access::AccessError::Unavailable,
        )
        | AssociationError::Unavailable => Error::unavailable(()),
        AssociationError::Access(_) => Error::missing(),
        AssociationError::Limit => {
            Error::invalid("Commit association query exceeds its read budget")
        }
    }
}

fn user(user: &User) -> Value {
    let mut value =
        codeyard_webhooks::payloads::person(&codeyard_webhooks::payloads::Person::of(user));
    value["site_admin"] = json!(user.is_admin);
    value
}

async fn repository_document(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    documents: &mut HashMap<i64, Value>,
) -> Result<Value> {
    if let Some(value) = documents.get(&repo.id) {
        return Ok(value.clone());
    }
    let facts =
        codeyard_db_repos::repository_stats::repository_document_facts(&state.pool, repo.id)
            .await
            .map_err(Error::unavailable)?;
    let mut value = codeyard_webhooks::payloads::repository(repo, owner);
    codeyard_webhooks::payloads::set_fork_counts(
        &mut value,
        codeyard_app::api_shared::github::repos::public_fork_count(state, repo.id).await?,
    );
    value["open_issues"] = json!(facts.open_items);
    value["open_issues_count"] = json!(facts.open_items);
    value["pushed_at"] = json!(facts.pushed_at);
    value["language"] = json!(facts.language);
    if let Some(id) = repo.personal_owner_id() {
        let owner = codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        value["owner"] = user(&owner);
    } else {
        value["owner"]["url"] = json!(codeyard_model::urls::api_url(&format!(
            "/orgs/{}",
            codeyard_webui::encode_component(owner)
        )));
    }
    documents.insert(repo.id, value.clone());
    Ok(value)
}

async fn milestone(state: &AppState, repo: &Repository, owner: &str, id: i64) -> Result<Value> {
    let summary = codeyard_db_repos::milestones::get_milestone_by_id(&state.pool, repo.id, id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let creator = match summary.milestone.creator_id {
        Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?,
        None => None,
    };
    Ok(codeyard_webhooks::payloads::milestone(
        &summary,
        creator.as_ref(),
        owner,
        &repo.name,
    ))
}

async fn document(
    state: &AppState,
    repo: &Repository,
    owner: &str,
    pull: &PullRequest,
    auth: Option<&codeyard_identity::github_auth::Authentication>,
    documents: &mut HashMap<i64, Value>,
    associations: &HashMap<i64, String>,
) -> Result<Value> {
    let author = codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, pull.author_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut value = codeyard_webhooks::payloads::pull_request_json(
        pull,
        &codeyard_webhooks::payloads::Person::of(&author),
        repo,
        owner,
    );
    value["user"] = user(&author);
    value["author_association"] = json!(associations.get(&author.id).ok_or_else(Error::missing)?);
    let base = repository_document(state, repo, owner, documents).await?;
    value["base"] = json!({"label":format!("{owner}:{}",pull.base_branch),"ref":pull.base_branch,"sha":pull.base_sha,"user":base["owner"],"repo":base});
    // A pull's retained commits do not authorize disclosing its private source repository.
    value["head"] = if let Some(head) = crate::pulls::readable_head(state, auth, pull).await? {
        let head_owner =
            codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, head.id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        let head = repository_document(state, &head, &head_owner, documents).await?;
        json!({"label":format!("{head_owner}:{}",pull.head_branch),"ref":pull.head_branch,"sha":pull.head_sha,"user":head["owner"],"repo":head})
    } else {
        crate::pulls::hidden_head(state, pull).await?
    };
    let labels = codeyard_db_repos::labels::get_pull_request_labels(&state.pool, repo.id, pull.id)
        .await
        .map_err(Error::unavailable)?;
    value["labels"] = json!(labels
        .iter()
        .map(|label| codeyard_webhooks::payloads::label(label, owner, &repo.name))
        .collect::<Vec<_>>());
    value["milestone"] = match pull.milestone_id {
        Some(id) => milestone(state, repo, owner, id).await?,
        None => Value::Null,
    };
    let names = codeyard_db_repos::assignees::list_assignees(
        &state.pool,
        codeyard_db_repos::assignees::AssignedItem::PullRequest(pull.id),
    )
    .await
    .map_err(Error::unavailable)?;
    let mut assignees = Vec::new();
    for name in names {
        if let Some(account) =
            codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &name)
                .await
                .map_err(Error::unavailable)?
        {
            assignees.push(user(&account));
        }
    }
    value["assignee"] = assignees.first().cloned().unwrap_or(Value::Null);
    value["assignees"] = json!(assignees);
    Ok(value)
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name, reference)): Path<(String, String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let (offset, limit) = page.window()?;
    let result = codeyard_repos_pulls::repositories::commit_pulls::associated_pulls(
        &state.pool,
        state.cluster.as_ref(),
        CommitPullQuery {
            repository_id: repo.id,
            actor: auth.as_ref().map(|auth| auth.user.id),
            reference: &reference,
            offset,
            limit,
        },
    )
    .await
    .map_err(operation_error)?;
    let owner = codeyard_db_repos::repositories::records::repository_owner_name(
        &state.pool,
        result.repository.id,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    let ids = result
        .pulls
        .iter()
        .map(|pull| pull.author_id)
        .collect::<Vec<_>>();
    let associations = codeyard_repos_pulls::repositories::commit_comments::author_associations(
        &state.pool,
        state.cluster.as_ref(),
        &result.repository,
        &ids,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut values = Vec::new();
    let mut documents = HashMap::new();
    for pull in &result.pulls {
        values.push(
            document(
                &state,
                &result.repository,
                &owner,
                pull,
                auth.as_ref(),
                &mut documents,
                &associations,
            )
            .await?,
        );
    }
    page.response(&uri, values, result.has_more)
}

pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("repos/list-pull-requests-associated-with-commit", get(list))]
}
