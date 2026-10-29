//! Repository lifecycle and metadata use the same authority checks as the settings forms.
use super::{
    auth, contents::store_error, pagination::Pagination, repo_url, repository, Error, Result,
};
use crate::backend::{
    app::AppState,
    db::{
        self,
        models::Repository,
        repositories::{creation, records, topics},
    },
    repositories::{lifecycle, store::repo_store, templates},
    webhooks::payloads,
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(in crate::backend::api) fn lifecycle_error(error: lifecycle::StorageError) -> Error {
    use lifecycle::LifecycleFailure::*;
    let status = match error.kind {
        Invalid | OwnRepositoryFork | Conflict => StatusCode::UNPROCESSABLE_ENTITY,
        Denied => StatusCode::FORBIDDEN,
        Unavailable | InitializationCleanupFailed | DeletedStorageRemains => {
            StatusCode::SERVICE_UNAVAILABLE
        }
    };
    Error::new(status, error.message)
}

pub(in crate::backend::api) fn settings_error(error: anyhow::Error) -> Error {
    use records::RepositorySettingsRefusal::*;
    match error.downcast_ref::<records::RepositorySettingsRefusal>() {
        Some(Denied | VisibilityLocked) => {
            Error::new(StatusCode::FORBIDDEN, "Repository mutation denied")
        }
        Some(InvalidTopics) => Error::invalid_simple(error.to_string()),
        Some(_) => Error::invalid(error.to_string()),
        None => Error::unavailable(error),
    }
}

pub(super) async fn search_document(
    state: &AppState,
    repo: &Repository,
    viewer: Option<&auth::Authentication>,
) -> Result<Value> {
    let mut value = document(state, repo, viewer).await?;
    let git = crate::backend::repositories::catalog_search::git_facts(
        repo,
        state.cluster.as_ref(),
        false,
    )
    .await
    .map_err(Error::unavailable)?;
    value["license"] = git
        .license
        .and_then(crate::backend::repositories::templates::license)
        .map(|template| super::meta::license_document(template, false))
        .unwrap_or(Value::Null);
    Ok(value)
}

/// A repository's REST document for `viewer`. Its fork parent and source appear only when the
/// viewer's credential may read them too.
pub(super) async fn document(
    state: &AppState,
    repo: &Repository,
    viewer: Option<&auth::Authentication>,
) -> Result<Value> {
    let owner = records::repository_owner_name(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let mut value = payloads::repository(repo, &owner);
    let facts = db::repository_stats::repository_document_facts(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?;
    value["language"] = json!(facts.language);
    value["open_issues_count"] = json!(facts.open_items);
    value["pushed_at"] = json!(db::repositories::github::repository_pushed_at(
        &state.pool,
        repo.id
    )
    .await
    .map_err(Error::unavailable)?);
    value["topics"] = json!(topics::repository_topics(&state.pool, &[repo.id])
        .await
        .map_err(Error::unavailable)?
        .remove(&repo.id)
        .unwrap_or_default());
    value["has_wiki"] = json!(records::repository_wiki(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?);
    value["has_discussions"] = json!(db::repository_discussions::enabled(&state.pool, repo.id)
        .await
        .map_err(Error::unavailable)?);
    if repo.is_fork {
        let mut parent = repo.forked_from_id;
        let mut seen = std::collections::HashSet::from([repo.id]);
        while let Some(id) = parent {
            if seen.len() > 64 || !seen.insert(id) {
                return Err(Error::unavailable(anyhow::anyhow!("Invalid fork ancestry")));
            }
            let ancestor = records::get_repository_by_id(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            if !auth::readable_repositories(state, viewer, &[id], "metadata")
                .await?
                .contains(&id)
            {
                break;
            }
            let owner = records::repository_owner_name(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
            let document = payloads::repository(&ancestor, &owner);
            if repo.forked_from_id == Some(id) {
                value["parent"] = document.clone();
            }
            parent = ancestor.forked_from_id;
            if parent.is_none() {
                value["source"] = document;
            }
        }
    }
    if let Some(viewer) = viewer.map(|auth| &auth.user) {
        let level = crate::backend::repositories::access::user_access(&state.pool, viewer, repo)
            .await
            .map_err(Error::unavailable)?;
        if level.can_admin() {
            if let Some(settings) = db::code_security::repository_settings(&state.pool, repo.id)
                .await
                .map_err(Error::unavailable)?
            {
                value["security_and_analysis"] =
                    serde_json::from_str(&settings.security_and_analysis)
                        .map_err(Error::unavailable)?;
            }
            let security = db::secret_scanning::settings(&state.pool, repo.id)
                .await
                .map_err(Error::unavailable)?;
            let status = |enabled| if enabled { "enabled" } else { "disabled" };
            if !value["security_and_analysis"].is_object() {
                value["security_and_analysis"] = json!({});
            }
            value["security_and_analysis"]["secret_scanning"] =
                json!({"status":status(security.enabled)});
            value["security_and_analysis"]["secret_scanning_push_protection"] =
                json!({"status":status(security.push_protection)});
        }
        value["permissions"] = json!({
            "admin": level.can_admin(), "maintain": level.can_maintain(),
            "push": level.can_write(), "triage": level >= db::models::AccessLevel::Triage,
            "pull": !repo.is_private || level.can_read(),
        });
    }
    Ok(value)
}

#[derive(Deserialize)]
struct Create {
    name: String,
    description: Option<String>,
    homepage: Option<String>,
    #[serde(default)]
    private: bool,
    visibility: Option<String>,
    #[serde(default)]
    auto_init: bool,
    gitignore_template: Option<String>,
    license_template: Option<String>,
    is_template: Option<bool>,
    has_wiki: Option<bool>,
    has_discussions: Option<bool>,
    custom_properties:
        Option<HashMap<String, Option<db::organizations::properties::PropertyValue>>>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    params: Option<Path<HashMap<String, String>>>,
    Json(input): Json<Create>,
) -> Result<Response> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    lifecycle::validate_description(input.description.as_deref().unwrap_or(""))
        .map_err(lifecycle_error)?;
    let website = input
        .homepage
        .as_deref()
        .map(db::repositories::topics::parse_website)
        .transpose()
        .map_err(|error| Error::invalid(error.to_string()))?;
    let target = if let Some(org) = params.and_then(|Path(params)| params.get("org").cloned()) {
        let org = db::organizations::profiles::get_public_organization(&state.pool, &org)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        creation::RepositoryTarget::Organization(org.id)
    } else {
        creation::RepositoryTarget::Personal(actor.user.id)
    };
    let private = match input.visibility.as_deref() {
        Some("public") => false,
        Some("private") => true,
        None => input.private,
        _ => return Err(Error::invalid("Invalid visibility")),
    };
    let gitignore = input
        .gitignore_template
        .as_deref()
        .map(|name| {
            templates::gitignore(name).ok_or_else(|| Error::invalid("Unknown gitignore template"))
        })
        .transpose()?;
    let license = input
        .license_template
        .as_deref()
        .map(|name| {
            templates::license(name).ok_or_else(|| Error::invalid("Unknown license template"))
        })
        .transpose()?;
    let properties = input
        .custom_properties
        .unwrap_or_default()
        .into_iter()
        .map(
            |(property_name, value)| db::organizations::properties::ValueInput {
                property_name,
                value,
            },
        )
        .collect::<Vec<_>>();
    let repo = lifecycle::provision_with_initial_properties(
        (&state.pool, state.cluster.as_ref()),
        &state.repos_path,
        lifecycle::CreationInput {
            owner: creation::RepositoryCreationOwner {
                actor_id: actor.user.id,
                target,
            },
            properties: &properties,
        },
        &input.name,
        input.description.as_deref(),
        private,
        lifecycle::InitialFiles {
            readme: input.auto_init,
            gitignore,
            license,
        },
    )
    .await
    .map_err(lifecycle_error)?;
    features(
        &state,
        actor.user.id,
        repo.id,
        Some(input.has_wiki.unwrap_or(true)),
        input.has_discussions,
    )
    .await?;
    let repo = if input.is_template.is_some() || website.is_some() {
        records::patch_repository(
            &state.pool,
            actor.user.id,
            repo.id,
            &records::RepositoryChanges {
                is_template: input.is_template,
                website: website.as_ref().map(|website| website.as_deref()),
                ..Default::default()
            },
        )
        .await
        .map_err(settings_error)?
    } else {
        repo
    };
    let value = document(&state, &repo, Some(&actor)).await?;
    Ok((
        StatusCode::CREATED,
        [(
            axum::http::header::LOCATION,
            value["url"].as_str().unwrap().to_owned(),
        )],
        Json(value),
    )
        .into_response())
}

#[derive(Deserialize)]
struct Update {
    name: Option<String>,
    description: Option<String>,
    homepage: Option<String>,
    private: Option<bool>,
    visibility: Option<String>,
    default_branch: Option<String>,
    archived: Option<bool>,
    allow_merge_commit: Option<bool>,
    allow_squash_merge: Option<bool>,
    allow_rebase_merge: Option<bool>,
    is_template: Option<bool>,
    has_wiki: Option<bool>,
    has_discussions: Option<bool>,
}

/// Wiki and discussions live outside the repository row, so both create and update set them here.
async fn features(
    state: &AppState,
    actor: i64,
    repo: i64,
    wiki: Option<bool>,
    discussions: Option<bool>,
) -> Result<()> {
    if let Some(enabled) = wiki {
        records::set_repository_wiki(&state.pool, actor, repo, enabled)
            .await
            .map_err(settings_error)?;
    }
    if let Some(enabled) = discussions {
        use db::repository_discussions::Refusal;
        db::repository_discussions::write(
            &state.pool,
            actor,
            repo,
            None,
            &db::repository_discussions::Write::Enabled(enabled),
        )
        .await
        .map_err(|error| match error.downcast_ref::<Refusal>() {
            Some(Refusal::Denied) => {
                Error::new(StatusCode::FORBIDDEN, "Repository mutation denied")
            }
            Some(Refusal::Missing) => Error::missing(),
            Some(Refusal::Invalid) => Error::invalid(error.to_string()),
            None => Error::unavailable(error),
        })?;
    }
    Ok(())
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(value): Json<Value>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let actor = &auth.user;
    let input: Update = serde_json::from_value(value.clone())
        .map_err(|_| Error::invalid("Invalid repository fields"))?;
    for (field, enabled) in [
        ("has_issues", true),
        ("has_pull_requests", true),
        ("has_projects", false),
        ("allow_forking", true),
        ("allow_update_branch", true),
        ("allow_auto_merge", true),
        ("delete_branch_on_merge", false),
        ("web_commit_signoff_required", false),
    ] {
        if value
            .get(field)
            .is_some_and(|value| value.as_bool() != Some(enabled))
        {
            return Err(Error::invalid(format!(
                "{field} cannot be changed on this server"
            )));
        }
    }
    if value.get("security_and_analysis").is_some_and(|value| {
        !value.is_null() && value.as_object().is_none_or(|value| !value.is_empty())
    }) {
        return Err(Error::invalid(
            "Security analysis services are not available",
        ));
    }
    if let Some(name) = input.name.as_deref() {
        lifecycle::validate_name(name).map_err(lifecycle_error)?;
    }
    if value.get("description").is_some() {
        lifecycle::validate_description(input.description.as_deref().unwrap_or(""))
            .map_err(lifecycle_error)?;
    }
    let private = match input.visibility.as_deref() {
        Some("public") => Some(false),
        Some("private") => Some(true),
        None => input.private,
        _ => return Err(Error::invalid("Invalid visibility")),
    };
    let branch_changed = input
        .default_branch
        .as_ref()
        .is_some_and(|branch| branch != &repo.default_branch);
    if branch_changed {
        let branch = input.default_branch.as_deref().unwrap();
        let reference = crate::backend::git::branch_ref(branch)
            .ok_or_else(|| Error::invalid("Invalid default branch"))?;
        let refs = repo_store(&repo, state.cluster.as_ref())
            .refs_from_primary()
            .await
            .map_err(store_error)?;
        if !refs.contains_key(&reference) {
            return Err(Error::invalid("The default branch must already exist"));
        }
    }
    let changes = records::RepositoryChanges {
        name: input.name.as_deref(),
        description: value
            .get("description")
            .map(|_| input.description.as_deref()),
        website: value.get("homepage").map(|_| input.homepage.as_deref()),
        is_private: private,
        archived: input.archived,
        is_template: input.is_template,
        merge_methods: records::MergeMethodUpdate {
            merge: input.allow_merge_commit,
            squash: input.allow_squash_merge,
            rebase: input.allow_rebase_merge,
        },
        ..Default::default()
    };
    let repo = if branch_changed {
        repo_store(&repo, state.cluster.as_ref())
            .patch_repository_as(
                &state.pool,
                repo.id,
                actor.id,
                input.default_branch.as_deref().unwrap(),
                &changes,
            )
            .await
            .map_err(store_error)?
    } else {
        records::patch_repository(&state.pool, actor.id, repo.id, &changes)
            .await
            .map_err(settings_error)?
    };
    features(
        &state,
        actor.id,
        repo.id,
        input.has_wiki,
        input.has_discussions,
    )
    .await?;
    Ok(Json(document(&state, &repo, Some(&auth)).await?))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let actor = actor.ok_or_else(auth::required)?;
    lifecycle::delete(
        &state.pool,
        state.cluster.as_ref(),
        db::users::accounts::AccountActor::Token { user_id: actor.user.id, token_id: actor.token.id },
        repo.id,
    )
    .await
    .map_err(lifecycle_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Transfer {
    new_owner: String,
}

async fn transfer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Transfer>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = actor.ok_or_else(auth::required)?;
    let actor = &auth.user;
    let repo = lifecycle::transfer(
        &state.pool,
        state.cluster.as_ref(),
        actor.id,
        repo.id,
        &input.new_owner,
    )
    .await
    .map_err(|error| match error.kind {
        lifecycle::LifecycleFailure::Invalid | lifecycle::LifecycleFailure::Conflict => {
            Error::invalid(error.message)
        }
        _ => lifecycle_error(error),
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(document(&state, &repo, Some(&auth)).await?),
    )
        .into_response())
}

async fn get_topics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let names = topics::repository_topics(&state.pool, &[repo.id])
        .await
        .map_err(Error::unavailable)?
        .remove(&repo.id)
        .unwrap_or_default();
    Ok(Json(json!({"names": names})))
}

#[derive(Deserialize)]
struct Topics {
    names: Vec<String>,
}

async fn set_topics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Topics>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let names =
        topics::parse_topics(input.names.iter().map(String::as_str)).map_err(settings_error)?;
    topics::replace_topics(
        &state.pool,
        actor.ok_or_else(auth::required)?.user.id,
        repo.id,
        &names,
    )
    .await
    .map_err(settings_error)?;
    Ok(Json(json!({"names": names})))
}

async fn languages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let store = repo_store(&repo, state.cluster.as_ref());
    if store.refs().await.map_err(store_error)?.is_empty() {
        return Ok(Json(json!({})));
    }
    let languages = store
        .languages(&repo.default_branch)
        .await
        .map_err(store_error)?;
    let mut value = json!({});
    for language in languages {
        value[language.language] = json!(language.bytes);
    }
    Ok(Json(value))
}

async fn tags(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, _) = repository(&state, &headers, &owner, &name, false).await?;
    let (offset, limit) = page.window()?;
    let tags = repo_store(&repo, state.cluster.as_ref())
        .tags()
        .await
        .map_err(store_error)?;
    let next = tags.len() > offset + limit;
    let base = repo_url(&owner, &name);
    let values = tags.into_iter().skip(offset).take(limit).map(|tag| {
        let reference = super::contents::path_url(&tag.name);
        json!({"name": tag.name,
            "node_id": super::node_id::reference(repo.id, &format!("refs/tags/{}", tag.name)),
            "commit": {"sha": tag.commit, "url": format!("{base}/commits/{}", tag.commit)},
            "zipball_url": format!("{base}/zipball/{reference}"), "tarball_url": format!("{base}/tarball/{reference}"),
        })
    }).collect();
    page.response(&uri, values, next)
}

async fn hash_algorithm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    repository(&state, &headers, &owner, &name, false).await?;
    // RepoStore initializes SHA-1 repositories and libgit2 reads that object format.
    Ok(Json(json!({"hash_algorithm": "sha1"})))
}

#[derive(Deserialize)]
struct ListQuery {
    page: Option<usize>,
    per_page: Option<usize>,
    #[serde(rename = "type")]
    kind: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    visibility: Option<String>,
    affiliation: Option<String>,
    since: Option<i64>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    params: Option<Path<HashMap<String, String>>>,
    Query(query): Query<ListQuery>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    use db::repositories::github::{self, RepositoryList, RepositoryListQuery};
    let authenticated = uri.path() == "/api/v3/user/repos";
    let auth = auth::authenticate(&state, &headers, authenticated).await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let params = params.map(|Path(params)| params).unwrap_or_default();
    let public = uri.path() == "/api/v3/repositories";
    let list = if let Some(username) = params.get("user_id") {
        let user = db::users::accounts::get_user_by_username(&state.pool, username)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        RepositoryList::User(user.id)
    } else if let Some(org) = params.get("org") {
        let org = db::organizations::profiles::get_public_organization(&state.pool, org)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        RepositoryList::Organization(org.id)
    } else if let (Some(owner), Some(name)) = (params.get("owner"), params.get("repo")) {
        let (repo, _) = repository(&state, &headers, owner, name, false).await?;
        RepositoryList::Forks(repo.id)
    } else if public {
        RepositoryList::Public
    } else {
        let affiliation = query
            .affiliation
            .as_deref()
            .unwrap_or("owner,collaborator,organization_member");
        if affiliation
            .split(',')
            .any(|value| !matches!(value, "owner" | "collaborator" | "organization_member"))
        {
            return Err(Error::invalid("Invalid affiliation"));
        }
        if query.kind.is_some() && (query.visibility.is_some() || query.affiliation.is_some()) {
            return Err(Error::invalid(
                "type cannot be combined with visibility or affiliation",
            ));
        }
        RepositoryList::Accessible {
            owner: affiliation.split(',').any(|value| value == "owner"),
            collaborator: affiliation.split(',').any(|value| value == "collaborator"),
            organization: affiliation
                .split(',')
                .any(|value| value == "organization_member"),
        }
    };
    let forks = matches!(list, RepositoryList::Forks(_));
    let kind = query.kind.as_deref().unwrap_or("all");
    if !matches!(
        kind,
        "all" | "owner" | "public" | "private" | "member" | "forks" | "sources"
    ) {
        return Err(Error::invalid("Invalid type"));
    }
    let visibility = query.visibility.as_deref().unwrap_or("all");
    if !matches!(visibility, "all" | "public" | "private") {
        return Err(Error::invalid("Invalid visibility"));
    }
    let sort = if public {
        "id"
    } else if forks {
        match query.sort.as_deref().unwrap_or("newest") {
            "newest" | "oldest" => "created",
            "stargazers" => "stars",
            "watchers" => "watchers",
            _ => return Err(Error::invalid("Invalid sort")),
        }
    } else {
        query
            .sort
            .as_deref()
            .unwrap_or(if params.contains_key("org") {
                "created"
            } else {
                "full_name"
            })
    };
    if !matches!(
        sort,
        "id" | "created" | "updated" | "pushed" | "full_name" | "stars" | "watchers"
    ) {
        return Err(Error::invalid("Invalid sort"));
    }
    let descending = match query.direction.as_deref() {
        Some("asc") => false,
        Some("desc") => true,
        None => !public && sort != "full_name" && query.sort.as_deref() != Some("oldest"),
        _ => return Err(Error::invalid("Invalid direction")),
    };
    let page = Pagination {
        page: if public { None } else { query.page },
        per_page: if public { Some(100) } else { query.per_page },
    };
    let (offset, limit) = page.window()?;
    let repos = github::list_repositories(
        &state.pool,
        RepositoryListQuery {
            viewer,
            list,
            since: query.since.unwrap_or(0),
            visibility,
            kind,
            sort,
            descending,
            limit: (limit + 1) as i64,
            offset: offset as i64,
        },
    )
    .await
    .map_err(Error::unavailable)?;
    let next = repos.len() > limit;
    let ids: Vec<i64> = repos.iter().take(limit).map(|repo| repo.id).collect();
    let readable = auth::readable_repositories(&state, auth.as_ref(), &ids, "metadata").await?;
    let mut values = Vec::new();
    for repo in repos.iter().take(limit) {
        if !readable.contains(&repo.id) {
            continue;
        }
        values.push(document(&state, repo, auth.as_ref()).await?);
    }
    if public {
        let mut headers = HeaderMap::new();
        if next {
            if let Some(last) = repos.get(limit - 1) {
                headers.insert(
                    axum::http::header::LINK,
                    format!(
                        "<{}?since={}>; rel=\"next\"",
                        super::api_url("/repositories"),
                        last.id
                    )
                    .parse()
                    .map_err(Error::unavailable)?,
                );
            }
        }
        return Ok((headers, Json(values)).into_response());
    }
    page.response(&uri, values, next)
}

#[derive(Deserialize)]
struct Dispatch {
    event_type: String,
    #[serde(default = "empty_payload")]
    client_payload: Value,
}

fn empty_payload() -> Value {
    json!({})
}

async fn dispatch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<Dispatch>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(auth::required)?;
    let actor = auth.workflow_actor(&state, repo.id).await?;
    let Json(input) = input.map_err(|_| Error::invalid("Invalid repository dispatch"))?;
    if !crate::backend::ci::events::valid_dispatch(&input.event_type, &input.client_payload) {
        return Err(Error::invalid("Invalid repository dispatch"));
    }
    let access = db::repositories::access::get_user_repo_access(&state.pool, actor, repo.id)
        .await
        .map_err(Error::unavailable)?;
    if !access.can_write() {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Repository write access required",
        ));
    }
    state
        .ci_runs
        .dispatch_repository_event(
            &repo,
            actor,
            auth.user.id,
            input.event_type,
            input.client_payload,
        )
        .await
        .map_err(super::actions::admission_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/create-for-authenticated-user", post(create)),
        ("repos/create-dispatch-event", post(dispatch)),
        ("repos/create-in-org", post(create)),
        ("repos/delete", delete(remove)),
        ("repos/update", patch(update)),
        ("repos/transfer", post(transfer)),
        ("repos/get-all-topics", get(get_topics)),
        ("repos/replace-all-topics", put(set_topics)),
        ("repos/list-languages", get(languages)),
        ("repos/list-tags", get(tags)),
        ("repos/get-hash-algorithm", get(hash_algorithm)),
        ("repos/list-for-authenticated-user", get(list)),
        ("repos/list-for-user", get(list)),
        ("repos/list-for-org", get(list)),
        ("repos/list-public", get(list)),
        ("repos/list-forks", get(list)),
        (
            "repos/custom-properties-for-repos-get-repository-values",
            get(property_values),
        ),
        (
            "repos/custom-properties-for-repos-create-or-update-repository-values",
            patch(set_property_values),
        ),
    ]
}

/// Personal repositories have no organization properties, so they list none.
async fn property_values(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, false).await?;
    let metadata = db::organizations::properties::repository(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
    )
    .await
    .map_err(super::orgs::properties::error)?;
    Ok(Json(json!(metadata
        .map(|metadata| metadata.values.properties)
        .unwrap_or_default())))
}

#[derive(Deserialize)]
struct PropertyValues {
    properties: Vec<db::organizations::properties::ValueInput>,
}

async fn set_property_values(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<PropertyValues>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let Json(input) = input.map_err(|_| Error::invalid("Invalid repository property values"))?;
    db::organizations::properties::set_repository_values(
        &state.pool,
        auth.ok_or_else(auth::required)?.user.id,
        repo.id,
        &input.properties,
    )
    .await
    .map_err(super::orgs::properties::error)?;
    Ok(StatusCode::NO_CONTENT)
}
