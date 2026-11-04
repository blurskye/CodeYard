//! Repository lifecycle and metadata use the same authority checks as the settings forms.
use axum::response::IntoResponse;

use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_repos_core::api_shared::github::contents::store_error;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_db_core::models::Repository;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::repo_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub fn settings_error(error: anyhow::Error) -> Error {
    use codeyard_db_repos::repositories::records::RepositorySettingsRefusal::*;
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return Error::new(
            StatusCode::FORBIDDEN,
            "A live unrestricted credential is required",
        );
    }
    match error
        .downcast_ref::<codeyard_db_repos::repositories::records::RepositorySettingsRefusal>()
    {
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
    viewer: Option<&codeyard_identity::github_auth::Authentication>,
    deadline: tokio::time::Instant,
    cached_license: Option<Option<&str>>,
) -> Result<Value> {
    let mut value = document(state, repo, viewer).await?;
    let license = match cached_license {
        Some(license) => license.and_then(codeyard_repos::templates::license),
        None => tokio::time::timeout_at(
            deadline,
            codeyard_repos_pulls::repositories::catalog_search::git_facts(
                repo,
                state.cluster.as_ref(),
                false,
            ),
        )
        .await
        .ok()
        .and_then(|result| result.ok())
        .and_then(|git| git.license)
        .and_then(codeyard_repos::templates::license),
    };
    value["license"] = license
        .map(|template| crate::meta::license_document(template, false))
        .unwrap_or(Value::Null);
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
    allow_merge_commit: Option<bool>,
    allow_squash_merge: Option<bool>,
    allow_rebase_merge: Option<bool>,
    #[serde(flatten)]
    features: codeyard_db_repos::repositories::records::FeatureUpdate,
    custom_properties: Option<
        HashMap<
            String,
            Option<codeyard_db_core::queries::organizations::properties::PropertyValue>,
        >,
    >,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    params: Option<Path<HashMap<String, String>>>,
    Json(input): Json<Create>,
) -> Result<Response> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    actor.require_write()?;
    let merge_methods = codeyard_db_repos::repositories::records::MergeMethodUpdate {
        merge: input.allow_merge_commit,
        squash: input.allow_squash_merge,
        rebase: input.allow_rebase_merge,
    };
    merge_methods
        .resolve((true, true, true))
        .map_err(settings_error)?;
    codeyard_repos_core::repositories::lifecycle::validate_description(
        input.description.as_deref().unwrap_or(""),
    )
    .map_err(lifecycle_error)?;
    let website = input
        .homepage
        .as_deref()
        .map(codeyard_db_repos::repositories::topics::parse_website)
        .transpose()
        .map_err(|error| Error::invalid(error.to_string()))?;
    let target = if let Some(org) = params.and_then(|Path(params)| params.get("org").cloned()) {
        let org =
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, &org)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(org.id)
    } else {
        codeyard_db_repos::repositories::creation::RepositoryTarget::Personal(actor.user.id)
    };
    let (private, internal) = match input.visibility.as_deref() {
        Some(name) => visibility(name)?,
        None => (input.private, false),
    };
    if internal
        && matches!(
            target,
            codeyard_db_repos::repositories::creation::RepositoryTarget::Personal(_)
        )
    {
        return Err(settings_error(
            codeyard_db_repos::repositories::records::RepositorySettingsRefusal::InternalNeedsOrganization.into(),
        ));
    }
    input.features.validate().map_err(settings_error)?;
    let gitignore = input
        .gitignore_template
        .as_deref()
        .map(|name| {
            codeyard_repos::templates::gitignore(name)
                .ok_or_else(|| Error::invalid("Unknown gitignore template"))
        })
        .transpose()?;
    let license = input
        .license_template
        .as_deref()
        .map(|name| {
            codeyard_repos::templates::license(name)
                .ok_or_else(|| Error::invalid("Unknown license template"))
        })
        .transpose()?;
    let properties = input
        .custom_properties
        .unwrap_or_default()
        .into_iter()
        .map(|(property_name, value)| {
            codeyard_db_core::queries::organizations::properties::ValueInput {
                property_name,
                value,
            }
        })
        .collect::<Vec<_>>();
    let repo = codeyard_repos_core::repositories::lifecycle::provision_with_initial_properties(
        (&state.pool, state.cluster.as_ref()),
        &state.repos_path,
        codeyard_repos_core::repositories::lifecycle::CreationInput {
            owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner {
                actor_id: actor.user.id,
                target,
            },
            properties: &properties,
            admission: codeyard_db_repos::repositories::creation::CreationAdmission {
                authority: Some(&actor.database_actor()),
                advisory: None,
            },
        },
        &input.name,
        input.description.as_deref(),
        private,
        codeyard_repos_core::repositories::lifecycle::InitialFiles {
            readme: input.auto_init,
            gitignore,
            license,
        },
    )
    .await
    .map_err(lifecycle_error)?;
    let repo = codeyard_db_repos::repositories::records::patch_repository(
        &state.pool,
        actor.user.id,
        repo.id,
        &codeyard_db_repos::repositories::records::RepositoryChanges {
            token: Some(actor.token.id),
            is_template: input.is_template,
            website: website.as_ref().map(|website| website.as_deref()),
            wiki: Some(input.has_wiki.unwrap_or(true)),
            discussions: input.has_discussions,
            merge_methods,
            internal: internal.then_some(true),
            features: input.features,
            ..Default::default()
        },
    )
    .await
    .map_err(settings_error)?;
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
    #[serde(flatten)]
    features: codeyard_db_repos::repositories::records::FeatureUpdate,
}

/// GitHub's visibility names as (private, internal).
fn visibility(name: &str) -> Result<(bool, bool)> {
    match name {
        "public" => Ok((false, false)),
        "private" => Ok((true, false)),
        "internal" => Ok((true, true)),
        _ => Err(Error::invalid("Invalid visibility")),
    }
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(value): Json<Value>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let input: Update = serde_json::from_value(value.clone())
        .map_err(|_| Error::invalid("Invalid repository fields"))?;
    for (field, enabled) in [
        ("has_pull_requests", true),
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
        codeyard_repos_core::repositories::lifecycle::validate_name(name).map_err(lifecycle_error)?;
    }
    if value.get("description").is_some() {
        codeyard_repos_core::repositories::lifecycle::validate_description(
            input.description.as_deref().unwrap_or(""),
        )
        .map_err(lifecycle_error)?;
    }
    let (private, internal) = match input.visibility.as_deref().map(visibility).transpose()? {
        Some((private, internal)) => (Some(private), Some(internal)),
        None => (input.private, None),
    };
    let branch_changed = input
        .default_branch
        .as_ref()
        .is_some_and(|branch| branch != &repo.default_branch);
    if branch_changed {
        let branch = input.default_branch.as_deref().unwrap();
        let reference = codeyard_git::branch_ref(branch)
            .ok_or_else(|| Error::invalid("Invalid default branch"))?;
        let refs = repo_store(&repo, state.cluster.as_ref())
            .refs_from_primary()
            .await
            .map_err(store_error)?;
        if !refs.contains_key(&reference) {
            return Err(Error::invalid("The default branch must already exist"));
        }
    }
    let changes = codeyard_db_repos::repositories::records::RepositoryChanges {
        token: Some(auth.token.id),
        name: input.name.as_deref(),
        description: value
            .get("description")
            .map(|_| input.description.as_deref()),
        website: value.get("homepage").map(|_| input.homepage.as_deref()),
        is_private: private,
        archived: input.archived,
        wiki: input.has_wiki,
        discussions: input.has_discussions,
        is_template: input.is_template,
        merge_methods: codeyard_db_repos::repositories::records::MergeMethodUpdate {
            merge: input.allow_merge_commit,
            squash: input.allow_squash_merge,
            rebase: input.allow_rebase_merge,
        },
        internal,
        features: input.features,
        ..Default::default()
    };
    let repo = if branch_changed {
        repo_store(&repo, state.cluster.as_ref())
            .patch_repository_as(
                &state.pool,
                repo.id,
                auth.database_actor(),
                input.default_branch.as_deref().unwrap(),
                &changes,
            )
            .await
            .map_err(store_error)?
    } else {
        codeyard_db_repos::repositories::records::patch_repository(
            &state.pool,
            auth.database_actor(),
            repo.id,
            &changes,
        )
        .await
        .map_err(settings_error)?
    };
    Ok(Json(document(&state, &repo, Some(&auth)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Transfer {
    new_owner: String,
}

async fn transfer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<Transfer>,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let repo = codeyard_repos_core::repositories::lifecycle::transfer(
        &state.pool,
        state.cluster.as_ref(),
        auth.database_actor(),
        repo.id,
        &input.new_owner,
    )
    .await
    .map_err(|error| match error.kind {
        codeyard_repos_core::repositories::lifecycle::LifecycleFailure::Invalid
        | codeyard_repos_core::repositories::lifecycle::LifecycleFailure::Conflict => {
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
    let names = codeyard_db_repos::repositories::topics::repository_topics(&state.pool, &[repo.id])
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
    let names = codeyard_db_repos::repositories::topics::parse_topics(
        input.names.iter().map(String::as_str),
    )
    .map_err(settings_error)?;
    codeyard_db_repos::repositories::topics::replace_topics(
        &state.pool,
        actor
            .ok_or_else(codeyard_identity::github_auth::required)?
            .user
            .id,
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
    let branch = format!("refs/heads/{}", repo.default_branch);
    if !store
        .refs()
        .await
        .map_err(store_error)?
        .contains_key(&branch)
    {
        return Ok(Json(json!({})));
    }
    let languages = store.languages(&branch).await.map_err(store_error)?;
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
        let reference = codeyard_webui::pages::repos::ref_path::encode_path(&format!("refs/tags/{}", tag.name));
        json!({"name": tag.name,
            "node_id": codeyard_model::node_id::reference(repo.id, &format!("refs/tags/{}", tag.name)),
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
    use codeyard_db_repos::repositories::github;
    use codeyard_db_repos::repositories::github::RepositoryList;
    use codeyard_db_repos::repositories::github::RepositoryListQuery;
    let authenticated = uri.path() == "/api/v3/user/repos";
    let auth =
        codeyard_identity::github_auth::authenticate(&state, &headers, authenticated)
            .await?;
    let viewer = auth.as_ref().map(|auth| auth.user.id);
    let params = params.map(|Path(params)| params).unwrap_or_default();
    let public = uri.path() == "/api/v3/repositories";
    let list = if let Some(username) = params.get("user_id") {
        let user =
            codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, username)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        RepositoryList::User(user.id)
    } else if let Some(org) = params.get("org") {
        let org =
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, org)
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
    // A user's profile lists what the user owns unless asked for more, as GitHub does.
    let default_kind = if matches!(list, RepositoryList::User(_)) {
        "owner"
    } else {
        "all"
    };
    let kind = query.kind.as_deref().unwrap_or(default_kind);
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
    let mut repos = Vec::with_capacity(limit + 1);
    // The query already applies anonymous visibility, so anonymous listings start at the page.
    // Token restrictions are applied below, so authenticated listings count from the start.
    let (mut scanned, mut accepted) = if auth.is_none() {
        (offset as i64, offset)
    } else {
        (0, 0)
    };
    'pages: loop {
        let rows = github::list_repositories(
            &state.pool,
            RepositoryListQuery {
                viewer,
                list,
                since: query.since.unwrap_or(0),
                visibility,
                kind,
                sort,
                descending,
                limit: 100,
                offset: scanned,
            },
        )
        .await
        .map_err(Error::unavailable)?;
        scanned += rows.len() as i64;
        let done = rows.len() < 100;
        let ids: Vec<_> = rows.iter().map(|repo| repo.id).collect();
        let readable = codeyard_identity::github_auth::readable_repositories(
            &state,
            auth.as_ref(),
            &ids,
            "metadata",
        )
        .await?;
        for repo in rows {
            if !readable.contains(&repo.id) {
                continue;
            }
            if accepted < offset {
                accepted += 1;
                continue;
            }
            repos.push(repo);
            if repos.len() > limit {
                break 'pages;
            }
        }
        if done {
            break;
        }
    }
    let next = repos.len() > limit;
    let mut values = documents(&state, &repos[..repos.len().min(limit)], auth.as_ref()).await?;
    if auth.is_none() {
        let ids: Vec<_> = values
            .iter()
            .filter_map(|value| value["id"].as_i64())
            .collect();
        let readable = codeyard_identity::github_auth::readable_repositories(
            &state, None, &ids, "metadata",
        )
        .await?;
        values.retain(|value| {
            value["id"]
                .as_i64()
                .is_some_and(|id| readable.contains(&id))
        });
    }
    if public {
        let mut headers = HeaderMap::new();
        if next {
            if let Some(last) = repos.get(limit - 1) {
                headers.insert(
                    axum::http::header::LINK,
                    format!(
                        "<{}?since={}>; rel=\"next\"",
                        codeyard_model::urls::api_url("/repositories"),
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
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let actor = auth.workflow_actor(&state, repo.id).await?;
    let Json(input) = input.map_err(codeyard_model::github_error::json_error)?;
    if !codeyard_repos_core::ci::events::valid_dispatch(&input.event_type, &input.client_payload) {
        return Err(Error::invalid("Invalid repository dispatch"));
    }
    let access =
        codeyard_db_repos::repositories::access::get_user_repo_access(&state.pool, actor, repo.id)
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
            auth.ci_credential(&headers, "contents")?,
        )
        .await
        .map_err(crate::actions::workflows::admission_error)?;
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
    let metadata = codeyard_db_orgs::organizations::properties::repository(
        &state.pool,
        auth.as_ref().map(|auth| auth.user.id),
        repo.id,
    )
    .await
    .map_err(crate::orgs::properties::error)?;
    Ok(Json(json!(metadata
        .map(|metadata| metadata.values.properties)
        .unwrap_or_default())))
}

#[derive(Deserialize)]
struct PropertyValues {
    properties: Vec<codeyard_db_core::queries::organizations::properties::ValueInput>,
}

async fn set_property_values(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<PropertyValues>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &owner, &name, true).await?;
    let Json(input) = input.map_err(codeyard_model::github_error::json_error)?;
    let auth = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    codeyard_db_orgs::organizations::properties::set_repository_values(
        &state.pool,
        auth.database_actor(),
        repo.id,
        &input.properties,
    )
    .await
    .map_err(crate::orgs::properties::error)?;
    Ok(StatusCode::NO_CONTENT)
}

use codeyard_app::api_shared::github::repos::document;

use codeyard_app::api_shared::github::repos::documents;

use codeyard_app::api_shared::github::repos::lifecycle_error;

use codeyard_app::api_shared::github::repos::remove;

pub mod repository_activity;

pub mod merges;

pub mod branches;

pub mod environments;

pub mod push_teams;

pub mod repository_teams;

pub mod deployment_protection;

pub mod deploy_keys;

pub mod dependencies;

pub mod copy;

pub mod deployments;

pub mod community;

pub mod protection;

pub mod immutable_releases;

pub mod releases;

pub mod traffic;

pub mod branch_operations;

pub mod collaborators;

pub mod statuses;

pub mod rulesets;

pub mod commit_comments;

pub mod private_reporting;

pub mod commit_pulls;

pub mod autolinks;

pub mod contents;

pub mod webhooks;

pub mod pages;

pub mod commits;

pub mod repository_attestations;

pub mod code_owners;

pub mod statistics;

pub mod archives;

pub mod comparison;

pub mod protection_operations;
