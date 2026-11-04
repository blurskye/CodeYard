//! JWT discovery and installation management, backed by native account consent.
use crate::apps::access_token;
use crate::apps::application;
use crate::optional_json::OptionalJson;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_webhooks::payloads;
use codeyard_db_core::models::PersonalAccessToken;
use codeyard_db_identity::github_apps::App;
use codeyard_db_identity::github_apps::Installation;
use codeyard_db_identity::github_apps::InstallationFilter;
use codeyard_db_identity::github_apps::RepositoryChange;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_model::urls::api_url;
use codeyard_support::token_hash::token_hash;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub(super) async fn jwt(state: &AppState, headers: &HeaderMap) -> Result<App> {
    let value = codeyard_app::api_shared::github::auth::credential(headers)
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    codeyard_identity::github_apps::authenticate(&state.pool, value)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)
}
async fn account(state: &AppState, installation: &Installation) -> Result<Value> {
    if let Some(user) = installation.user_id {
        return Ok(codeyard_webhooks::payloads::user(
            &codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, user)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?,
        ));
    }
    let org = codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(
        &state.pool,
        installation.org_id.ok_or_else(Error::missing)?,
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(Error::missing)?;
    Ok(
        json!({"login":org.name,"id":org.id,"node_id":codeyard_model::node_id::encode("O",org.id),"avatar_url":payloads::organization_avatar(&org),"gravatar_id":"","url":api_url(&format!("/orgs/{}",org.name)),"html_url":format!("{}{}",codeyard_model::http_config::link_origin(),codeyard_webui::profile_path(&org.name)),"followers_url":api_url(&format!("/users/{}/followers",org.name)),"following_url":api_url(&format!("/users/{}/following{{/other_user}}",org.name)),"gists_url":api_url(&format!("/users/{}/gists{{/gist_id}}",org.name)),"starred_url":api_url(&format!("/users/{}/starred{{/owner}}{{/repo}}",org.name)),"subscriptions_url":api_url(&format!("/users/{}/subscriptions",org.name)),"organizations_url":api_url(&format!("/users/{}/orgs",org.name)),"repos_url":api_url(&format!("/orgs/{}/repos",org.name)),"events_url":api_url(&format!("/users/{}/events{{/privacy}}",org.name)),"received_events_url":api_url(&format!("/users/{}/received_events",org.name)),"type":"Organization","site_admin":false}),
    )
}
pub(crate) async fn app_document(state: &AppState, app: &App) -> Result<Value> {
    let owner = codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, app.owner_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    let filter = InstallationFilter {
        app: Some(app.id),
        ..InstallationFilter::default()
    };
    let (installations, _) =
        codeyard_db_identity::github_apps::installations(&state.pool, &filter, 0, 0)
            .await
            .map_err(Error::unavailable)?;
    app_value(app, &owner, installations)
}
/// An app's document from rows already loaded, for responses that cannot read the database.
pub(crate) fn app_value(
    app: &App,
    owner: &codeyard_db_core::models::User,
    installations: i64,
) -> Result<Value> {
    Ok(
        json!({"id":app.id,"slug":app.slug,"node_id":codeyard_model::node_id::encode("APP",app.id),"owner":payloads::user(owner),"name":app.name,"description":app.description,"external_url":app.homepage,"html_url":format!("{}/apps/{}",codeyard_model::http_config::link_origin(),app.slug),"created_at":app.created_at,"updated_at":app.created_at,"permissions":serde_json::from_str::<Value>(&app.permissions).map_err(Error::unavailable)?,"events":app.events.split(',').filter(|s|!s.is_empty()).collect::<Vec<_>>(),"installations_count":installations,"client_id":app.client_id}),
    )
}
pub(crate) async fn document(state: &AppState, installation: &Installation) -> Result<Value> {
    // The request already sees this installation, so an app whose owner is no longer
    // active is still described rather than failing the page.
    let app = codeyard_db_identity::github_apps::app_by_id(&state.pool, installation.app_id)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(
        json!({"id":installation.id,"account":account(state,installation).await?,"repository_selection":if installation.all_repos{"all"}else{"selected"},"access_tokens_url":api_url(&format!("/app/installations/{}/access_tokens",installation.id)),"repositories_url":api_url("/installation/repositories"),"html_url":format!("{}/settings/apps/installations",codeyard_model::http_config::link_origin()),"app_id":app.id,"app_slug":app.slug,"target_id":installation.user_id.or(installation.org_id),"target_type":if installation.org_id.is_some(){"Organization"}else{"User"},"permissions":serde_json::from_str::<Value>(&app.permissions).map_err(Error::unavailable)?,"events":app.events.split(',').filter(|s|!s.is_empty()).collect::<Vec<_>>(),"created_at":installation.created_at,"updated_at":installation.updated_at,"single_file_name":null,"has_multiple_single_files":false,"single_file_paths":[],"suspended_at":installation.suspended_at,"suspended_by":if let Some(id)=installation.suspended_by{codeyard_db_identity::users::accounts::get_user_by_id(&state.pool,id).await.map_err(Error::unavailable)?.map(|user|codeyard_webhooks::payloads::user(&user))}else{None}}),
    )
}
async fn owned(state: &AppState, app: i64, id: i64) -> Result<Installation> {
    codeyard_db_identity::github_apps::installation(&state.pool, id)
        .await
        .map_err(Error::unavailable)?
        .filter(|i| i.app_id == app)
        .ok_or_else(Error::missing)
}
pub(super) async fn installation_requests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let app = jwt(&state, &headers).await?;
    let rows = codeyard_db_identity::github_apps::requests(&state.pool, app.id)
        .await
        .map_err(Error::unavailable)?;
    let (offset, limit) = page.window()?;
    let has_next = rows.len() > offset + limit;
    let mut values = vec![];
    for request in rows.into_iter().skip(offset).take(limit) {
        let installation = Installation {
            id: 0,
            app_id: request.app_id,
            user_id: None,
            org_id: Some(request.org_id),
            all_repos: false,
            suspended_at: None,
            suspended_by: None,
            created_at: request.created_at,
            updated_at: request.created_at,
        };
        let requester =
            codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, request.user_id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        values.push(json!({"id":request.id,"node_id":codeyard_model::node_id::encode("AIR",request.id),"account":account(&state,&installation).await?,"requester":payloads::user(&requester),"created_at":request.created_at}));
    }
    page.response(&uri, values, has_next)
}

pub(super) async fn get_authenticated(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    Ok(Json(app_document(&state, &app).await?))
}
pub(super) async fn get_by_slug(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Json<Value>> {
    let is_jwt = codeyard_app::api_shared::github::auth::credential(&headers)
        .is_some_and(|value| value.split('.').count() == 3);
    let jwt_app = if is_jwt {
        Some(jwt(&state, &headers).await?)
    } else {
        None
    };
    let authenticated = if is_jwt {
        None
    } else {
        codeyard_app::api_shared::github::auth::authenticate(&state, &headers, false).await?
    };
    let app = codeyard_db_identity::github_apps::app(&state.pool, &slug)
        .await
        .map_err(Error::unavailable)?
        .filter(|app| app.slug == slug)
        .ok_or_else(Error::missing)?;
    let owner = match &authenticated {
        // A credential bound to another app does not carry its user's view of this one.
        Some(auth) if auth.user.id == app.owner_id => user_app(&state, auth)
            .await?
            .is_none_or(|bound| bound == app.id),
        _ => false,
    };
    if !app.is_public && !jwt_app.as_ref().is_some_and(|jwt| jwt.id == app.id) && !owner {
        return Err(Error::missing());
    }
    Ok(Json(app_document(&state, &app).await?))
}
pub(super) async fn list_installations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let app = jwt(&state, &headers).await?;
    let filter = InstallationFilter {
        app: Some(app.id),
        ..InstallationFilter::default()
    };
    let (offset, limit) = page.window()?;
    let (total, rows) = installation_page(&state, &filter, offset, limit).await?;
    let mut values = vec![];
    for row in &rows {
        values.push(document(&state, row).await?);
    }
    page.response(&uri, values, more(total, offset, limit))
}
/// A page of installations from `installations`, with GitHub's usize window.
pub(crate) async fn installation_page(
    state: &AppState,
    filter: &InstallationFilter,
    offset: usize,
    limit: usize,
) -> Result<(i64, Vec<Installation>)> {
    codeyard_db_identity::github_apps::installations(
        &state.pool,
        filter,
        i64::try_from(offset).map_err(Error::unavailable)?,
        i64::try_from(limit).map_err(Error::unavailable)?,
    )
    .await
    .map_err(Error::unavailable)
}
/// Whether rows remain past a page.
pub(crate) fn more(total: i64, offset: usize, limit: usize) -> bool {
    usize::try_from(total).is_ok_and(|total| total > offset.saturating_add(limit))
}
pub(super) async fn get_installation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    Ok(Json(
        document(&state, &owned(&state, app.id, id).await?).await?,
    ))
}
pub(super) async fn find_installation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let app = jwt(&state, &headers).await?;
    let found = if let (Some(owner), Some(repo)) = (params.get("owner"), params.get("repo")) {
        let repo =
            codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, repo)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        codeyard_db_identity::github_apps::repository_installation(
            &state.pool,
            app.id,
            repo.id,
            false,
            None,
        )
        .await
    } else if let Some(org) = params.get("org") {
        let org =
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, org)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        codeyard_db_identity::github_apps::account_installation(
            &state.pool,
            app.id,
            None,
            Some(org.id),
        )
        .await
    } else {
        // The generated router canonicalizes this username parameter to user_id.
        let name = params.get("user_id").ok_or_else(Error::missing)?;
        let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, name)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        codeyard_db_identity::github_apps::account_installation(
            &state.pool,
            app.id,
            Some(user.id),
            None,
        )
        .await
    };
    let installation = found
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(Json(document(&state, &installation).await?))
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    id: i64,
    action: &str,
) -> Result<StatusCode> {
    let app = jwt(state, headers).await?;
    if codeyard_db_identity::github_apps::change_installation(&state.pool, app.id, id, action)
        .await
        .map_err(Error::unavailable)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::missing())
    }
}
pub(super) async fn delete_installation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    change(&state, &headers, id, "delete").await
}
pub(super) async fn suspend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    change(&state, &headers, id, "suspend").await
}
pub(super) async fn unsuspend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    change(&state, &headers, id, "unsuspend").await
}

/// A token `issue` stored, before any response is built from it.
struct Issued {
    value: String,
    token: PersonalAccessToken,
    permissions: Value,
    repositories: Vec<i64>,
    all_repositories: bool,
}
async fn issue(
    state: &AppState,
    app: &App,
    installation: &Installation,
    actor: Option<i64>,
    body: &Value,
    parent: Option<&codeyard_db_core::queries::github_apps::Token>,
) -> Result<Issued> {
    if installation.suspended_at.is_some() {
        return Err(Error::new(StatusCode::FORBIDDEN, "Installation suspended"));
    }
    if !body.is_object() {
        return Err(Error::invalid("Expected a token configuration object"));
    }
    let grant = serde_json::from_str::<Value>(parent.map_or(&app.permissions, |t| &t.permissions))
        .map_err(Error::unavailable)?;
    let permissions = body
        .get("permissions")
        .map_or_else(
            || Some(grant.clone()),
            |value| codeyard_identity::github_apps::subset(value, &grant),
        )
        .ok_or_else(|| Error::invalid("Permissions exceed the app grant"))?;
    let parent_repos = parent.map(selection).transpose()?.unwrap_or_default();
    if body.get("repository_ids").is_some() && body.get("repositories").is_some() {
        return Err(Error::invalid("Choose repository names or IDs, not both"));
    }
    let mut ids = if let Some(ids) = body.get("repository_ids") {
        serde_json::from_value::<Vec<i64>>(ids.clone())
            .map_err(|_| Error::invalid("Invalid repository IDs"))?
    } else if let Some(names) = body.get("repositories") {
        let names = serde_json::from_value::<Vec<String>>(names.clone())
            .map_err(|_| Error::invalid("Invalid repository names"))?;
        if names.len() > MAX_SELECTED {
            return Err(Error::invalid("Select at most 500 repositories"));
        }
        let owner = account(state, installation).await?;
        let owner = owner["login"].as_str().ok_or_else(Error::missing)?;
        let mut ids = vec![];
        for name in names {
            let repo =
                codeyard_db_repos::repositories::records::get_repository(&state.pool, owner, &name)
                    .await
                    .map_err(Error::unavailable)?
                    .ok_or_else(|| Error::invalid("Repository is outside this installation"))?;
            ids.push(repo.id);
        }
        ids
    } else {
        parent_repos.clone()
    };
    if ids.len() > MAX_SELECTED {
        return Err(Error::invalid("Select at most 500 repositories"));
    }
    ids.sort_unstable();
    ids.dedup();
    if ids
        .iter()
        .any(|id| !parent_repos.is_empty() && !parent_repos.contains(id))
    {
        return Err(Error::invalid("Repositories exceed the installation grant"));
    }
    if (body.get("repository_ids").is_some() || body.get("repositories").is_some())
        && ids.is_empty()
    {
        return Err(Error::invalid("Select at least one repository"));
    }
    let value = if actor.is_some() {
        format!("ghu_{}", codeyard_support::crypto::random_hex())
    } else {
        codeyard_identity::tokens::installation_token()
    };
    let source = body
        .get("access_token")
        .and_then(Value::as_str)
        .map(token_hash);
    let token = codeyard_db_identity::github_apps::issue(
        &state.pool,
        app.id,
        installation.id,
        actor,
        source.as_deref(),
        &token_hash(&value),
        &permissions.to_string(),
        &serde_json::to_string(&ids).map_err(Error::unavailable)?,
        codeyard_identity::github_apps::write(&permissions),
    )
    .await
    .map_err(Error::unavailable)?
    .ok_or_else(|| Error::invalid("Installation changed or repositories are unavailable"))?;
    Ok(Issued {
        value,
        token,
        permissions,
        all_repositories: installation.all_repos && ids.is_empty(),
        repositories: ids,
    })
}
/// The most repositories one token may select, as on GitHub.
const MAX_SELECTED: usize = 500;
/// The repositories a token's bounds select; empty means its whole installation.
fn selection(token: &codeyard_db_core::queries::github_apps::Token) -> Result<Vec<i64>> {
    serde_json::from_str(&token.repositories).map_err(Error::unavailable)
}
pub(super) async fn create_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    OptionalJson(body): OptionalJson<Value>,
) -> Result<(StatusCode, Json<Value>)> {
    let app = jwt(&state, &headers).await?;
    let installation = owned(&state, app.id, id).await?;
    let issued = issue(
        &state,
        &app,
        &installation,
        None,
        &body.unwrap_or_else(|| json!({})),
        None,
    )
    .await?;
    let mut response = json!({"token":issued.value,"expires_at":issued.token.expires_at,"permissions":issued.permissions,"repository_selection":if issued.all_repositories{"all"}else{"selected"}});
    if !issued.repositories.is_empty() {
        let mut repositories = vec![];
        for repo in codeyard_db_repos::repositories::records::get_repositories_by_ids(
            &state.pool,
            &issued.repositories,
        )
        .await
        .map_err(Error::unavailable)?
        {
            repositories.push(
                codeyard_app::api_shared::github::repos::document(&state, &repo, None).await?,
            );
        }
        response["repositories"] = json!(repositories);
    }
    Ok((StatusCode::CREATED, Json(response)))
}

pub(super) async fn installation_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(
    codeyard_app::api_shared::github::auth::Authentication,
    codeyard_db_core::queries::github_apps::Token,
)> {
    let auth = codeyard_app::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let token = codeyard_db_identity::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
        .filter(|token| !token.user_token)
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    Ok((auth, token))
}
pub(super) async fn list_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (auth, token) = installation_token(&state, &headers).await?;
    let installation =
        codeyard_db_identity::github_apps::installation(&state.pool, token.installation_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
    repository_list(
        &state,
        &installation,
        &selection(&token)?,
        &auth,
        None,
        &uri,
        &page,
    )
    .await
}
/// A page of an installation's repositories among `selected`, or all of them when that is
/// empty. A user `viewer` sees only the ones they reach explicitly, as on GitHub.
async fn repository_list(
    state: &AppState,
    installation: &Installation,
    selected: &[i64],
    auth: &codeyard_app::api_shared::github::auth::Authentication,
    viewer: Option<i64>,
    uri: &axum::http::Uri,
    page: &Pagination,
) -> Result<Response> {
    let (offset, limit) = page.window()?;
    let (total, rows) = codeyard_db_identity::github_apps::installation_repositories(
        &state.pool,
        installation.id,
        viewer,
        selected,
        i64::try_from(offset).map_err(Error::unavailable)?,
        i64::try_from(limit).map_err(Error::unavailable)?,
    )
    .await
    .map_err(Error::unavailable)?;
    let mut values = vec![];
    for repo in &rows {
        values.push(
            codeyard_app::api_shared::github::repos::document(state, repo, Some(auth)).await?,
        );
    }
    let all_repositories = installation.all_repos && selected.is_empty();
    Ok((page.headers(uri,more(total, offset, limit))?,Json(json!({"total_count":total,"repository_selection":if all_repositories{"all"}else{"selected"},"repositories":values}))).into_response())
}
pub(super) async fn revoke_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<StatusCode> {
    let (auth, _) = installation_token(&state, &headers).await?;
    codeyard_db_identity::github_apps::revoke(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The app a credential is bound to: an App token's, or a GitHub App's user token's.
async fn user_app(
    state: &AppState,
    auth: &codeyard_app::api_shared::github::auth::Authentication,
) -> Result<Option<i64>> {
    if let Some(token) = codeyard_db_identity::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(codeyard_db_identity::github_apps::installation(
            &state.pool,
            token.installation_id,
        )
        .await
        .map_err(Error::unavailable)?
        .map(|i| i.app_id));
    }
    if let Some(token) = codeyard_db_identity::oauth_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(
            codeyard_db_identity::github_apps::app(&state.pool, &token.client_id)
                .await
                .map_err(Error::unavailable)?
                .map(|app| app.id),
        );
    }
    Ok(None)
}
pub(super) async fn user_installations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let auth = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let token = codeyard_db_identity::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?;
    let filter = InstallationFilter {
        app: user_app(&state, &auth).await?,
        actor: Some(auth.user.id),
        id: token.map(|token| token.installation_id),
        ..InstallationFilter::default()
    };
    let (offset, limit) = page.window()?;
    let (total, rows) = installation_page(&state, &filter, offset, limit).await?;
    let mut values = vec![];
    for row in &rows {
        values.push(document(&state, row).await?);
    }
    Ok((
        page.headers(&uri, more(total, offset, limit))?,
        Json(json!({"total_count":total,"installations":values})),
    )
        .into_response())
}
pub(super) async fn user_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let auth = codeyard_app::api_shared::github::auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    let token = codeyard_db_identity::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?;
    if token
        .as_ref()
        .is_some_and(|token| token.installation_id != id)
    {
        return Err(Error::missing());
    }
    let filter = InstallationFilter {
        app: user_app(&state, &auth).await?,
        actor: Some(auth.user.id),
        id: Some(id),
        ..InstallationFilter::default()
    };
    let (_, rows) = installation_page(&state, &filter, 0, 1).await?;
    let installation = rows
        .into_iter()
        .next()
        .filter(|installation| installation.suspended_at.is_none())
        .ok_or_else(Error::missing)?;
    // An organization that refuses classic tokens refuses this listing of its repositories.
    if let Some(org) = installation.org_id {
        crate::auth::refuse_classic(&state, auth.token.id, org).await?;
    }
    let selected = token
        .as_ref()
        .map(selection)
        .transpose()?
        .unwrap_or_default();
    repository_list(
        &state,
        &installation,
        &selected,
        &auth,
        Some(auth.user.id),
        &uri,
        &page,
    )
    .await
}
async fn change_repository(
    state: &AppState,
    headers: &HeaderMap,
    id: i64,
    repo: i64,
    add: bool,
) -> Result<StatusCode> {
    let auth = codeyard_app::api_shared::github::auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    auth.require_write()?;
    // A narrowed app token may not widen its own repository grant.
    if codeyard_db_identity::github_apps::token(&state.pool, auth.token.id)
        .await
        .map_err(Error::unavailable)?
        .is_some()
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Use an account authorization to change installation access",
        ));
    }
    if let Some(app) = user_app(state, &auth).await? {
        owned(state, app, id).await?;
    }
    match codeyard_db_identity::github_apps::change_repository(
        &state.pool,
        auth.database_actor(),
        id,
        repo,
        add,
    )
    .await
    .map_err(Error::unavailable)?
    {
        RepositoryChange::Changed => Ok(StatusCode::NO_CONTENT),
        RepositoryChange::Missing => Err(Error::missing()),
        RepositoryChange::AllRepositories => Err(Error::invalid(
            "The installation already covers all repositories",
        )),
        RepositoryChange::LastRepository => Err(Error::invalid(
            "An organization installation keeps at least one repository",
        )),
    }
}
pub(super) async fn add_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, repo)): Path<(i64, i64)>,
) -> Result<StatusCode> {
    change_repository(&state, &headers, id, repo, true).await
}
pub(super) async fn remove_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, repo)): Path<(i64, i64)>,
) -> Result<StatusCode> {
    change_repository(&state, &headers, id, repo, false).await
}
/// The installation a scoped token targets, named by account ID, login or both. User and
/// organization IDs can coincide, so an ID that names both needs the login too.
async fn target_installation(state: &AppState, app: i64, body: &Value) -> Result<Installation> {
    let id = body
        .get("target_id")
        .or_else(|| body.get("account_id"))
        .map(|id| {
            id.as_i64()
                .ok_or_else(|| Error::invalid("Invalid target ID"))
        })
        .transpose()?;
    let login = body
        .get("target")
        .or_else(|| body.get("account"))
        .map(|login| {
            login
                .as_str()
                .ok_or_else(|| Error::invalid("Invalid target"))
        })
        .transpose()?;
    let user = match (login, id) {
        (Some(login), _) => {
            codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, login).await
        }
        (None, Some(id)) => {
            codeyard_db_identity::users::accounts::get_user_by_id(&state.pool, id).await
        }
        (None, None) => Ok(None),
    }
    .map_err(Error::unavailable)?
    .filter(|user| id.is_none_or(|id| id == user.id));
    let org = match (login, id) {
        (Some(login), _) => {
            codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, login)
                .await
        }
        (None, Some(id)) => {
            codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(
                &state.pool,
                id,
            )
            .await
        }
        (None, None) => Ok(None),
    }
    .map_err(Error::unavailable)?
    .filter(|org| id.is_none_or(|id| id == org.id));
    let mut found = vec![];
    if let Some(user) = user {
        found.extend(
            codeyard_db_identity::github_apps::account_installation(
                &state.pool,
                app,
                Some(user.id),
                None,
            )
            .await
            .map_err(Error::unavailable)?,
        );
    }
    if let Some(org) = org {
        found.extend(
            codeyard_db_identity::github_apps::account_installation(
                &state.pool,
                app,
                None,
                Some(org.id),
            )
            .await
            .map_err(Error::unavailable)?,
        );
    }
    match found.len() {
        0 => Err(Error::invalid("Choose an accessible target account")),
        1 => Ok(found.remove(0)),
        _ => Err(Error::invalid(
            "Account IDs overlap; select the target by login",
        )),
    }
}
pub(super) async fn scope_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(client): Path<String>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>)> {
    let oauth_app = application(&state, &headers, &client).await?;
    let source =
        crate::apps::owned_token(&state, &client, &token_hash(access_token(&body)?)).await?;
    let app = codeyard_db_identity::github_apps::app(&state.pool, &client)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    // A scoped source keeps its bounds even after its installation is gone, so it can never
    // be scoped again as if it were unbounded.
    let parent = codeyard_db_identity::github_apps::token_bounds(&state.pool, source.id)
        .await
        .map_err(Error::unavailable)?;
    let installation = target_installation(&state, app.id, &body).await?;
    if parent
        .as_ref()
        .is_some_and(|parent| parent.installation_id != installation.id)
    {
        return Err(Error::invalid("Target exceeds the original grant"));
    }
    let issued = issue(
        &state,
        &app,
        &installation,
        Some(source.user_id),
        &body,
        parent.as_ref(),
    )
    .await?;
    let details = crate::apps::authorization(&state, &issued.token).await?;
    Ok((
        StatusCode::OK,
        Json(crate::apps::document(
            &oauth_app,
            &issued.token,
            &issued.value,
            &details,
        )),
    ))
}
