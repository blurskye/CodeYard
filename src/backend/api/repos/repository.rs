use crate::backend::api::admin::actions as admin_actions;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::{repository_settings_error, ApiError};
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::{
    access_error, check_repo_admin_access, check_repo_read_access, check_repo_write_access,
    find_repository, readable_repository,
};
use crate::backend::api::repos::chrome::ApiResult;
use crate::backend::api::repos::chrome::{
    git_unavailable_response, load_metadata, repository_default_branch_or_db, unavailable, Loaded,
};
use crate::backend::api::repos::code::contents::{
    delete_form, replaced_repository_editor, write_form,
};
use crate::backend::api::repos::forms::{checkbox, field, repeated, with_status};
use crate::backend::api::repos::labels::{label_write_error, parse_label_color};
use crate::backend::api::repos::responses::{
    lifecycle_error, lifecycle_status, repository_owner, repository_topics, ForkParent,
    RepoResponse,
};
use crate::backend::api::repos::settings::branch_protection;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::repositories::access::AccessError;
use crate::backend::repositories::lifecycle;
use crate::backend::webhooks::{panel, settings::HookError};
use crate::webui::pages::repos as view;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::{Form, Json};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const REPO_ADMIN_REQUIRED: &str = "Only repository administrators can manage these settings.";

fn autolink_error(error: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    use db::repositories::autolinks::Refusal;
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => ApiError::failure(
            StatusCode::FORBIDDEN,
            "permission_denied",
            REPO_ADMIN_REQUIRED,
        ),
        Some(Refusal::Missing) => {
            ApiError::failure(StatusCode::NOT_FOUND, "not_found", "Autolink not found")
        }
        Some(error) => ApiError::failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            &error.to_string(),
        ),
        None => ApiError::failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Autolink storage unavailable",
        ),
    }
}

fn immutable_releases_error(error: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    use db::repositories::immutable_releases::Refusal;
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => ApiError::failure(
            StatusCode::FORBIDDEN,
            "permission_denied",
            REPO_ADMIN_REQUIRED,
        ),
        Some(Refusal::Archived) => ApiError::failure(
            StatusCode::CONFLICT,
            "archived",
            "Archived repositories cannot change immutable release settings",
        ),
        None => ApiError::failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Release policy storage unavailable",
        ),
    }
}

fn invitation_error(
    error: crate::backend::repositories::collaborators::Error,
) -> (StatusCode, Json<ApiError>) {
    use crate::backend::repositories::collaborators::Error;
    match error {
        Error::Denied => ApiError::failure(
            StatusCode::FORBIDDEN,
            "permission_denied",
            REPO_ADMIN_REQUIRED,
        ),
        Error::NotFound => ApiError::failure(
            StatusCode::NOT_FOUND,
            "not_found",
            "Invitation or user not found",
        ),
        Error::Invalid(message) => ApiError::failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            &message,
        ),
        Error::Unavailable => ApiError::failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Invitation storage unavailable",
        ),
    }
}

pub async fn get_repo(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let repository = readable_repository(&state.pool, &owner, &repo, &headers).await?;
    repository_response(&state, &headers, &repository)
        .await
        .map(Json)
}

/// The repository as GET shows it. Mutations build their responses from the row they
/// persisted through this same function, so every field matches GET.
async fn repository_response(
    state: &AppState,
    headers: &HeaderMap,
    repository: &db::models::Repository,
) -> Result<RepoResponse, (StatusCode, Json<ApiError>)> {
    let owner = repository_owner(&state.pool, repository).await?;
    let default_branch = repository_default_branch_or_db(state, repository).await;
    let topics = repository_topics(&state.pool, &[repository.id])
        .await?
        .remove(&repository.id)
        .unwrap_or_default();

    let forked_from = if let Some(parent_id) = repository.forked_from_id {
        match db::repositories::records::get_repository_by_id(&state.pool, parent_id)
            .await
            .map_err(|_| access_error(AccessError::Unavailable))?
        {
            Some(parent) => match check_repo_read_access(&state.pool, &parent, headers).await {
                Ok(()) => {
                    let parent_owner = repository_owner(&state.pool, &parent).await?;
                    Some(ForkParent {
                        id: parent.id,
                        owner: parent_owner.clone(),
                        name: parent.name.clone(),
                        full_name: format!("{}/{}", parent_owner, parent.name),
                    })
                }
                Err((StatusCode::NOT_FOUND, _)) => None,
                Err(error) => return Err(error),
            },
            None => None,
        }
    } else {
        None
    };

    let mut response = RepoResponse::new(repository, &owner, topics, forked_from);
    response.default_branch = default_branch;
    Ok(response)
}

#[derive(Deserialize)]
pub struct RenameRepoRequest {
    pub new_name: String,
}

#[derive(Deserialize)]
pub struct UpdateRepoRequest {
    pub description: Option<String>,
    pub is_private: bool,
    pub allow_merge_commit: Option<bool>,
    pub allow_squash_merge: Option<bool>,
    pub allow_rebase_merge: Option<bool>,
}

pub async fn rename_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<RenameRepoRequest>,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    lifecycle::validate_name(&req.new_name).map_err(lifecycle_error)?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &user).await?;

    // Unique owner/name reservation is authoritative. A rename only changes the
    // DB alias; stable operation-owned storage is never moved or compensated.
    let updated = db::repositories::records::rename_repository(
        &state.pool,
        user.id,
        repository.id,
        &req.new_name,
    )
    .await
    .map_err(repository_settings_error)?;

    Ok(Json(repository_response(&state, &headers, &updated).await?))
}

#[derive(Deserialize)]
pub struct TransferRepoRequest {
    pub new_owner: String,
}

pub async fn transfer_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<TransferRepoRequest>,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &user).await?;
    let updated = lifecycle::transfer(
        &state.pool,
        state.cluster.as_ref(),
        user.id,
        repository.id,
        &req.new_owner,
    )
    .await
    .map_err(lifecycle_error)?;
    Ok(Json(repository_response(&state, &headers, &updated).await?))
}

#[derive(Deserialize)]
pub struct AboutRequest {
    pub description: Option<String>,
    pub website: Option<String>,
    /// The complete topic list. Each name is lowercased, and the list replaces the old one.
    pub topics: Vec<String>,
}

/// Saves the About text: description, website and topics. Anyone with write access may save
/// it, and an archived repository refuses it.
pub async fn about_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<AboutRequest>,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_write_access(&state.pool, &repository, &user).await?;
    let description = req.description.unwrap_or_default();
    lifecycle::validate_description(&description).map_err(lifecycle_error)?;
    let website =
        db::repositories::topics::parse_website(req.website.as_deref().unwrap_or_default())
            .map_err(repository_settings_error)?;
    let topics = db::repositories::topics::parse_topics(req.topics.iter().map(String::as_str))
        .map_err(repository_settings_error)?;
    db::repositories::topics::update_about(
        &state.pool,
        user.id,
        repository.id,
        &description,
        website.as_deref(),
        &topics,
    )
    .await
    .map_err(repository_settings_error)?;
    let updated = db::repositories::records::get_repository_by_id(&state.pool, repository.id)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;
    Ok(Json(repository_response(&state, &headers, &updated).await?))
}

pub async fn update_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<UpdateRepoRequest>,
) -> Result<Json<RepoResponse>, (StatusCode, Json<ApiError>)> {
    let user = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &user).await?;
    lifecycle::validate_description(req.description.as_deref().unwrap_or(""))
        .map_err(lifecycle_error)?;
    if !req.is_private
        && lifecycle::visibility_locked(&state.pool, &repository)
            .await
            .map_err(|error| {
                (
                    lifecycle_status(&error),
                    Json(ApiError::new("storage_unavailable", error.message)),
                )
            })?
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError::new(
                "visibility_locked",
                "A fork of nonpublic history cannot be made public.",
            )),
        ));
    }
    // The merge methods, description and visibility save in one transaction, so a refusal or a
    // failed write leaves all of them unchanged.
    db::repositories::records::update_repository(
        &state.pool,
        user.id,
        repository.id,
        req.description.as_deref(),
        req.is_private,
        db::repositories::records::MergeMethodUpdate {
            merge: req.allow_merge_commit,
            squash: req.allow_squash_merge,
            rebase: req.allow_rebase_merge,
        },
    )
    .await
    .map_err(repository_settings_error)?;
    let updated = db::repositories::records::get_repository_by_id(&state.pool, repository.id)
        .await
        .map_err(repository_settings_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ApiError::new("not_found", "Repository not found")),
            )
        })?;

    Ok(Json(repository_response(&state, &headers, &updated).await?))
}

pub async fn delete_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let actor = get_current_user(&state.pool, &headers).await?;
    let repository = find_repository(&state.pool, &owner, &repo).await?;
    check_repo_admin_access(&state.pool, &repository, &actor).await?;
    lifecycle::delete(&state.pool, state.cluster.as_ref(), actor.id, repository.id)
        .await
        .map_err(lifecycle_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn settings_model(
    ctx: &PageContext,
    loaded: &Loaded,
    tab: &str,
) -> Result<view::settings::Settings, Response> {
    let viewer = ctx.require_user()?;
    if !loaded.chrome.can_admin {
        return Err(ctx.repo_error(&loaded.chrome, StatusCode::FORBIDDEN, REPO_ADMIN_REQUIRED));
    }
    // The branches the default-branch picker offers, and the one the repository opens on.
    let mut branches: Vec<String> = loaded
        .refs
        .keys()
        .filter_map(|name| name.strip_prefix("refs/heads/").map(str::to_owned))
        .collect();
    branches.sort();
    let mut settings = view::settings::Settings {
        tab: tab.to_owned(),
        branches,
        default_branch: loaded.chrome.default_branch.clone(),
        allow_merge_commit: loaded.repository.allow_merge_commit,
        allow_squash_merge: loaded.repository.allow_squash_merge,
        allow_rebase_merge: loaded.repository.allow_rebase_merge,
        viewer: viewer.username.clone(),
        ..view::settings::Settings::default()
    };
    if tab == "general" {
        settings.interactions = Some(
            crate::backend::api::user::interactions::panel(
                &ctx.state.pool,
                viewer.id,
                db::interactions::InteractionScope::Repository(loaded.repository.id),
                &loaded.chrome.csrf_token,
                true,
            )
            .await
            .map_err(|_| unavailable(ctx))?,
        );
    }
    // Only the branch-protection section uses rules and their user choices.
    if tab == "branches" {
        settings.force_push_suggestions = db::repositories::access::list_write_access_users(
            &ctx.state.pool,
            loaded.repository.id,
            viewer.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(|(username, level)| view::settings::Collaborator {
            username,
            access: level.as_str().into(),
        })
        .collect();
        settings.branch_rules = branch_rules(ctx, loaded).await?;
    }
    if tab == "actions" {
        let pool = &ctx.state.pool;
        let (site, own) = tokio::try_join!(
            db::actions::get_actions_settings(pool),
            db::actions::get_repo_actions_settings(pool, loaded.repository.id),
        )
        .map_err(|_| unavailable(ctx))?;
        let text = |value: Option<i32>| value.map(|value| value.to_string()).unwrap_or_default();
        let server_minutes = ctx.state.ci_runs.config.run_timeout_seconds.div_ceil(60);
        let oidc = db::oidc::settings(
            pool,
            viewer.id,
            db::oidc::Scope::Repository(loaded.repository.id),
        )
        .await
        .map_err(|_| unavailable(ctx))?;
        settings.actions = Some(Box::new(view::settings::RepoActions {
            oidc_use_default: oidc.use_default,
            oidc_claim_keys: oidc.include_claim_keys.unwrap_or_default().join(", "),
            engine: loaded.repository.actions_engine.clone().unwrap_or_default(),
            enabled_engines: ["codeyard-actions", "act"]
                .into_iter()
                .filter(|engine| site.engine_enabled(engine))
                .map(str::to_owned)
                .collect(),
            site_engine: site.default_engine.clone(),
            engine_fallback: loaded
                .repository
                .actions_engine
                .as_deref()
                .is_some_and(|engine| !site.engine_enabled(engine)),
            enabled: own.enabled,
            run_timeout_minutes: text(own.run_timeout_minutes),
            job_cpus: text(own.job_cpus),
            job_memory_gib: text(own.job_memory_gib),
            site_enabled: site.enabled,
            site_timeout_minutes: site
                .run_timeout_minutes
                .and_then(|minutes| u32::try_from(minutes).ok())
                .unwrap_or(server_minutes),
            site_cpus: site.job_cpus,
            site_memory_gib: site.job_memory_gib,
            effective_cache_size_gib: Some(site.cache_size_bytes(&own) / (1024 * 1024 * 1024)),
            effective_cache_retention_days: Some(site.cache_expiry_days(&own)),
            cache_size_gib: text(own.cache_size_gib),
            cache_retention_days: text(own.cache_retention_days),
        }));
    }
    if tab == "security" {
        let security = db::secret_scanning::settings(&ctx.state.pool, loaded.repository.id)
            .await
            .map_err(|_| unavailable(ctx))?;
        settings.secret_scanning = security.enabled;
        settings.push_protection = security.push_protection;
        use crate::backend::repositories::secret_scanning::patterns::Owner;
        let mut scopes = vec![(Owner::Repository(loaded.repository.id), false)];
        if let Some(org) = loaded.repository.owner_org_id {
            scopes.push((Owner::Organization(org), true));
        }
        for (scope, organization) in scopes {
            let patterns =
                match db::secret_scanning::list_patterns(&ctx.state.pool, viewer.id, scope).await {
                    Ok(patterns) => patterns,
                    Err(error)
                        if matches!(
                            error.downcast_ref::<db::secret_scanning::Refusal>(),
                            Some(db::secret_scanning::Refusal::Denied)
                        ) =>
                    {
                        continue
                    }
                    Err(_) => return Err(unavailable(ctx)),
                };
            settings
                .secret_patterns
                .extend(
                    patterns
                        .into_iter()
                        .map(|pattern| view::secret_scanning::CustomPattern {
                            id: pattern.id,
                            name: pattern.name,
                            state: pattern.state,
                            organization,
                        }),
                );
        }
    }
    if tab == "secrets" {
        settings.environments =
            db::environment_values::environments(&ctx.state.pool, viewer.id, loaded.repository.id)
                .await
                .map_err(|_| unavailable(ctx))?;
        if let Some(name) = ctx.query.get("environment").filter(|name| !name.is_empty()) {
            let key = db::environment_values::environment_key(name).map_err(|_| {
                ctx.repo_error(
                    &loaded.chrome,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Invalid environment name.",
                )
            })?;
            let name = settings
                .environments
                .iter()
                .find(|name| {
                    db::environment_values::environment_key(name).ok().as_ref() == Some(&key)
                })
                .cloned()
                .ok_or_else(|| {
                    ctx.repo_error(
                        &loaded.chrome,
                        StatusCode::NOT_FOUND,
                        "Environment not found.",
                    )
                })?;
            let (secrets, variables) = tokio::try_join!(
                db::environment_values::list(
                    &ctx.state.pool,
                    viewer.id,
                    loaded.repository.id,
                    &name,
                    db::environment_values::Kind::Secret
                ),
                db::environment_values::list(
                    &ctx.state.pool,
                    viewer.id,
                    loaded.repository.id,
                    &name,
                    db::environment_values::Kind::Variable
                ),
            )
            .map_err(|_| unavailable(ctx))?;
            settings.environment = Some(Box::new(view::settings::environments::Environment {
                name,
                secrets: secrets
                    .into_iter()
                    .map(|value| view::settings::secrets::Secret {
                        name: value.name,
                        updated_at: value.updated_at.to_rfc3339(),
                    })
                    .collect(),
                variables: variables
                    .into_iter()
                    .map(|value| view::settings::secrets::Variable {
                        name: value.name,
                        value: value.value,
                    })
                    .collect(),
            }));
        }
        settings.secrets_enabled = ctx.state.secret_encryptor.is_some();
        settings.variables = db::variables::list(&ctx.state.pool, viewer.id, &db::users::accounts::AccountCredential::Session(&crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default()), loaded.repository.id)
            .await
            .map_err(|_| unavailable(ctx))?
            .into_iter()
            .map(|variable| view::settings::secrets::Variable {
                name: variable.name,
                value: variable.value,
            })
            .collect();
        settings.secrets =
            db::secrets::list_secrets(&ctx.state.pool, viewer.id, loaded.repository.id)
                .await
                .map_err(|_| unavailable(ctx))?
                .into_iter()
                .map(|secret| view::settings::secrets::Secret {
                    name: secret.name,
                    updated_at: secret.updated_at.to_rfc3339(),
                })
                .collect();
    }
    if tab == "dependabot-secrets" {
        settings.secrets_enabled = ctx.state.secret_encryptor.is_some();
        settings.dependabot_secrets = db::secrets::list_secrets_for(
            &ctx.state.pool,
            db::secrets::SecretKind::Dependabot,
            viewer.id,
            loaded.repository.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(|secret| view::settings::secrets::Secret {
            name: secret.name,
            updated_at: secret.updated_at.to_rfc3339(),
        })
        .collect();
    }
    if tab == "collaborators"
        && (loaded.chrome.can_manage_collaborators || loaded.chrome.can_manage_invitations)
    {
        // Team-derived access is managed on teams; invitations and direct grants are managed here.
        let accesses = db::repositories::access::list_repo_collaborators(
            &ctx.state.pool,
            loaded.repository.id,
        )
        .await
        .map_err(|_| unavailable(ctx))?;
        let ids: Vec<_> = accesses.iter().map(|access| access.user_id).collect();
        let users = db::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
            .await
            .map_err(|_| unavailable(ctx))?;
        let mut users: HashMap<_, _> = users.into_iter().map(|user| (user.id, user)).collect();
        for access in accesses {
            let user = users
                .remove(&access.user_id)
                .ok_or_else(|| unavailable(ctx))?;
            settings.collaborators.push(view::settings::Collaborator {
                username: user.username,
                access: db::models::AccessLevel::from(access.access_level)
                    .as_str()
                    .into(),
            });
        }
    }
    if tab == "collaborators"
        && (loaded.chrome.can_manage_collaborators || loaded.chrome.can_manage_invitations)
    {
        let invitations = crate::backend::repositories::collaborators::invitations(
            &ctx.state.pool,
            viewer.id,
            Some(loaded.repository.id),
            0,
            350,
        )
        .await
        .map_err(|_| unavailable(ctx))?;
        let ids = invitations
            .iter()
            .map(|invitation| invitation.invitee_id)
            .collect::<Vec<_>>();
        let users = db::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
            .await
            .map_err(|_| unavailable(ctx))?
            .into_iter()
            .map(|user| (user.id, user))
            .collect::<HashMap<_, _>>();
        for invitation in invitations {
            let user = users
                .get(&invitation.invitee_id)
                .ok_or_else(|| unavailable(ctx))?;
            settings
                .invitations
                .push(view::settings::PendingInvitation {
                    id: invitation.id,
                    username: user.username.clone(),
                    access: db::models::AccessLevel::from(invitation.permission)
                        .as_str()
                        .to_owned(),
                });
        }
    }
    if tab == "releases" {
        settings.immutable_releases = Some(
            crate::backend::repositories::releases::immutable::enabled(
                &ctx.state.pool,
                ctx.user.as_ref(),
                &loaded.repository,
            )
            .await
            .map_err(|_| unavailable(ctx))?,
        );
    }
    if tab == "autolinks" {
        settings.autolinks = db::repositories::autolinks::list(
            &ctx.state.pool,
            viewer.id,
            loaded.repository.id,
            50,
            0,
        )
        .await
        .map_err(|_| unavailable(ctx))?
        .into_iter()
        .map(super::refs::references::autolink)
        .collect();
    }
    if tab == "deploy-keys" {
        settings.deploy_keys =
            Some(super::deploy_keys::settings_section(ctx, loaded, viewer).await?);
    }
    if tab == "webhooks" {
        let base = format!("{}/settings?tab=webhooks", loaded.chrome.base());
        let action_base = format!("{}/_forms", loaded.chrome.base());
        let place = panel::Place {
            owner: db::webhooks::HookOwner::Repository(loaded.repository.id),
            base: &base,
            action_base: &action_base,
            csrf_token: &loaded.chrome.csrf_token,
            repository_id: Some(loaded.repository.id),
        };
        let built = panel::build(&ctx.state.pool, viewer, &db::users::accounts::AccountCredential::Session(&crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default()), &place, &ctx.query)
            .await
            .map_err(|error| webhook_page_error(ctx, loaded, error))?;
        settings.webhooks = Some(Box::new(built));
    }
    if tab == "properties" {
        settings.properties = Some(Box::new(
            super::properties::panel(ctx, loaded, ctx.query.get("property").map(String::as_str))
                .await?,
        ));
    }
    Ok(settings)
}

fn webhook_page_error(ctx: &PageContext, loaded: &Loaded, error: HookError) -> Response {
    let (status, message) = panel::refusal(&error);
    ctx.repo_error(&loaded.chrome, status, message)
}

async fn settings_page(ctx: &PageContext, loaded: &Loaded) -> Result<Response, Response> {
    let tab = view::settings::settings_tab(
        ctx.query.get("tab").map_or("", String::as_str),
        loaded.chrome.can_manage_collaborators || loaded.chrome.can_manage_invitations,
    );
    let mut settings = settings_model(ctx, loaded, tab).await?;
    settings.saved = ctx.query.get("saved").map(String::as_str) == Some("general");
    Ok(settings_response(ctx, loaded, settings, StatusCode::OK))
}

/// The settings page on the section that a settings action posts from, so its redirect returns
/// there.
fn settings_section_url(base: &str, action: &str) -> String {
    format!(
        "{base}/settings?tab={}",
        view::settings::settings_section(action).unwrap_or("general")
    )
}

pub(super) fn settings_response(
    ctx: &PageContext,
    loaded: &Loaded,
    settings: view::settings::Settings,
    status: StatusCode,
) -> Response {
    with_status(
        ctx.page_model(
            "Repository settings",
            crate::webui::app::Page::Repositories(view::page::Page::Settings {
                repo: loaded.chrome.clone(),
                settings: Box::new(settings),
            }),
        ),
        status,
    )
}

/// The settings page again, on `tab`, with a refusal at its top.
async fn settings_refusal(
    ctx: &PageContext,
    loaded: &Loaded,
    tab: &str,
    status: StatusCode,
    message: &str,
) -> Response {
    let mut settings = match settings_model(ctx, loaded, tab).await {
        Ok(settings) => settings,
        Err(response) => return response,
    };
    settings.error = Some(message.to_owned());
    settings_response(ctx, loaded, settings, status)
}

/// The repository's branch protection rules, read the way the JSON API reads them.
async fn branch_rules(
    ctx: &PageContext,
    loaded: &Loaded,
) -> Result<Vec<view::settings::BranchRule>, Response> {
    let rules = db::repositories::protection::list_branch_protection_rules(
        &ctx.state.pool,
        loaded.repository.id,
    )
    .await
    .map_err(|_| unavailable(ctx))?;
    let rules = branch_protection::rule_responses(&ctx.state.pool, rules, ctx.user.as_ref().map_or(-1, |user| user.id))
        .await
        .map_err(|_| unavailable(ctx))?;
    let mut listed = Vec::with_capacity(rules.len());
    for rule in rules {
        listed.push(view::settings::BranchRule {
            pattern: rule.pattern,
            require_pull_request: rule.require_pull_request,
            required_approving_reviews: rule.required_approving_reviews.to_string(),
            dismiss_stale_reviews: rule.dismiss_stale_reviews,
            require_status_checks: rule.require_status_checks,
            required_status_checks: rule.required_status_checks,
            require_up_to_date_branch: rule.require_up_to_date_branch,
            require_linear_history: rule.require_linear_history,
            require_conversation_resolution: rule.require_conversation_resolution,
            require_code_owner_review: rule.require_code_owner_reviews,
            enforce_admins: rule.enforce_admins,
            allow_force_pushes: rule.allow_force_pushes,
            allow_deletions: rule.allow_deletions,
            restrict_pushes: rule.restrict_pushes,
            push_allowances: rule.push_allowances,
            push_team_allowances: rule.push_team_allowances,
            force_push_allowances: rule.force_push_allowances,
        });
    }
    Ok(listed)
}

/// Names from a text field, split at any of the separators. Blank pieces are dropped.
fn names(text: &str, separator: impl Fn(char) -> bool) -> Vec<String> {
    text.split(separator)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Who a rule lets force push: the checked people, then the names typed in the extra field. Each
/// name counts once, first spelling kept, and the API still checks every name.
fn force_push_names(form: &HashMap<String, String>, pairs: &[(String, String)]) -> Vec<String> {
    let typed = names(&field(form, "force_push_extra"), |c: char| {
        c == ',' || c.is_whitespace()
    });
    let mut seen = HashSet::new();
    repeated(pairs, "force_push_allowances")
        .into_iter()
        .chain(typed)
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

/// The rule a branch protection form submitted, kept as typed so a refusal can show it again.
fn submitted_rule(
    form: &HashMap<String, String>,
    pairs: &[(String, String)],
) -> view::settings::BranchRule {
    view::settings::BranchRule {
        pattern: field(form, "pattern"),
        require_pull_request: checkbox(form, "require_pull_request"),
        required_approving_reviews: field(form, "required_approving_reviews"),
        dismiss_stale_reviews: checkbox(form, "dismiss_stale_reviews"),
        require_status_checks: checkbox(form, "require_status_checks"),
        required_status_checks: if checkbox(form, "require_status_checks") {
            view::settings::split_status_checks(&field(form, "required_status_checks"))
        } else {
            Vec::new()
        },
        require_up_to_date_branch: checkbox(form, "require_up_to_date_branch"),
        require_linear_history: checkbox(form, "require_linear_history"),
        require_conversation_resolution: checkbox(form, "require_conversation_resolution"),
        require_code_owner_review: checkbox(form, "require_code_owner_review"),
        enforce_admins: checkbox(form, "enforce_admins"),
        // The form asks to block these; the rule stores what they allow.
        allow_force_pushes: !checkbox(form, "block_force_pushes"),
        allow_deletions: !checkbox(form, "block_deletions"),
        restrict_pushes: checkbox(form, "restrict_pushes"),
        push_allowances: names(&field(form, "push_allowances"), |c: char| {
            c == ',' || c.is_whitespace()
        }),
        push_team_allowances: names(&field(form, "push_team_allowances"), |c: char| {
            c == ',' || c.is_whitespace()
        }),
        force_push_allowances: force_push_names(form, pairs),
    }
}

/// The JSON API request for a submitted rule.
fn rule_request(
    rule: &view::settings::BranchRule,
) -> Result<branch_protection::UpsertBranchProtectionRuleRequest, (StatusCode, Json<ApiError>)> {
    let required_approving_reviews = rule
        .required_approving_reviews
        .trim()
        .parse::<i32>()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    "validation_error",
                    "Required approvals must be a whole number.",
                )),
            )
        })?;
    Ok(branch_protection::UpsertBranchProtectionRuleRequest {
        pattern: rule.pattern.clone(),
        require_pull_request: rule.require_pull_request,
        required_approving_reviews,
        dismiss_stale_reviews: rule.dismiss_stale_reviews,
        require_status_checks: rule.require_status_checks,
        required_status_checks: rule.required_status_checks.clone(),
        require_up_to_date_branch: rule.require_up_to_date_branch,
        require_linear_history: rule.require_linear_history,
        require_conversation_resolution: rule.require_conversation_resolution,
        require_code_owner_reviews: rule.require_code_owner_review,
        enforce_admins: rule.enforce_admins,
        allow_force_pushes: rule.allow_force_pushes,
        allow_deletions: rule.allow_deletions,
        restrict_pushes: rule.restrict_pushes,
        push_allowances: rule.push_allowances.clone(),
        push_team_allowances: rule.push_team_allowances.clone(),
        force_push_allowances: rule.force_push_allowances.clone(),
        // The form never offers signed commits, so a submitted rule asks for none.
        require_signed_commits: false,
    })
}

/// The branch protection forms: save a rule (the add form and each rule's edit form) and delete
/// one. A refused save shows the settings page again with the rule as submitted, next to the form
/// that sent it. A refused delete puts its message at the top of the page.
async fn protection_form(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    form: &HashMap<String, String>,
    pairs: &[(String, String)],
) -> Response {
    let actor_id = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let pool = &ctx.state.pool;
    let repository = &loaded.repository;
    let section = view::settings::settings_section(action).unwrap_or("general");
    let settings_url = settings_section_url(&loaded.chrome.base(), action);
    if action == "protection-delete" {
        let outcome = match confirmation(form) {
            Ok(()) => {
                branch_protection::delete_rule(
                    pool,
                    actor_id,
                    repository.id,
                    &field(form, "pattern"),
                )
                .await
            }
            Err(message) => Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::new("validation_error", message)),
            )),
        };
        return match outcome {
            Ok(()) => ctx.redirect(&settings_url),
            Err((status, Json(error))) => {
                settings_refusal(ctx, loaded, section, status, &error.message).await
            }
        };
    }
    let submitted = submitted_rule(form, pairs);
    let outcome = match rule_request(&submitted) {
        Ok(request) => branch_protection::save_rule(pool, actor_id, repository, request)
            .await
            .map(|_| ()),
        Err(refusal) => Err(refusal),
    };
    match outcome {
        Ok(()) => ctx.redirect(&settings_url),
        Err((status, Json(error))) => {
            let mut settings = match settings_model(ctx, loaded, section).await {
                Ok(settings) => settings,
                Err(response) => return response,
            };
            settings.rule_draft = Some(view::settings::RuleDraft {
                rule: submitted,
                message: error.message,
            });
            settings_response(ctx, loaded, settings, status)
        }
    }
}

/// The webhook section's forms. A saved or changed hook returns to the section's list with a
/// notice. A refused save shows the section again with the submitted hook still in its form.
async fn webhook_form(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    form: &HashMap<String, String>,
    pairs: &[(String, String)],
) -> Response {
    let actor = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let pool = &ctx.state.pool;
    let owner = db::webhooks::HookOwner::Repository(loaded.repository.id);
    let base = format!("{}/settings?tab=webhooks", loaded.chrome.base());
    let outcome = panel::apply(
        pool,
        ctx.state.secret_encryptor.as_deref(),
        actor,
        &db::users::accounts::AccountCredential::Session(&crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default()),
        owner,
        action,
        form,
        pairs,
    )
    .await;
    match outcome {
        Ok(notice) => ctx.redirect(&format!("{base}&notice={notice}")),
        Err((error, draft)) => {
            let (status, message) = panel::refusal(&error);
            let mut settings = match settings_model(ctx, loaded, "webhooks").await {
                Ok(settings) => settings,
                Err(response) => return response,
            };
            // The refused draft already shows the credentials the hook still has.
            if let (Some(panel), Some(draft)) = (settings.webhooks.as_mut(), draft) {
                panel.editing = Some(draft);
            }
            if let Some(panel) = settings.webhooks.as_mut() {
                panel.error = Some(message.to_owned());
            }
            settings_response(ctx, loaded, settings, status)
        }
    }
}

/// The Stars list unstars in place: its form names its own address, so the action returns there.
/// Any other `return_to` is ignored.
fn stars_return(form: &HashMap<String, String>) -> Option<String> {
    form.get("return_to")
        .filter(|target| target.as_str() == "/stars" || target.starts_with("/stars?"))
        .filter(|target| crate::backend::api::http::origin::safe_redirect_path(target))
        .cloned()
}

fn confirmation(form: &HashMap<String, String>) -> Result<(), &'static str> {
    if form.get("confirm").map(String::as_str) == Some("yes") {
        Ok(())
    } else {
        Err("Explicit confirmation is required.")
    }
}

/// Only declared non-secret form fields enter typed bootstrap/navigation state.
/// Arbitrary extra POST keys, credentials and the synchronizer token are never reflected.
fn safe_submission(action: &str, values: &HashMap<String, String>) -> HashMap<String, String> {
    if action == "immutable-releases" {
        return HashMap::from([(
            "enabled".to_owned(),
            checkbox(values, "enabled").to_string(),
        )]);
    }
    if action == "autolink-add" {
        let mut fields = HashMap::from([(
            "is_alphanumeric".to_owned(),
            checkbox(values, "is_alphanumeric").to_string(),
        )]);
        for name in ["key_prefix", "url_template"] {
            if let Some(value) = values.get(name) {
                fields.insert(name.to_owned(), value.clone());
            }
        }
        return fields;
    }
    if action == "template" {
        return HashMap::from([(
            "is_template".to_owned(),
            checkbox(values, "is_template").to_string(),
        )]);
    }
    if action == "merge-methods" {
        // An unchecked box is not posted at all, so the retry form takes each box's state from
        // `checkbox`, the same reading the save uses.
        return [
            "allow_merge_commit",
            "allow_squash_merge",
            "allow_rebase_merge",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), checkbox(values, name).to_string()))
        .collect();
    }
    if action == "code-security" {
        return ["secret_scanning", "push_protection"]
            .into_iter()
            .map(|name| (name.to_owned(), checkbox(values, name).to_string()))
            .collect();
    }
    if action == "oidc-settings" {
        let mut fields: HashMap<String, String> = values
            .get("include_claim_keys")
            .map(|keys| ("include_claim_keys".to_owned(), keys.clone()))
            .into_iter()
            .collect();
        fields.insert(
            "use_default".to_owned(),
            checkbox(values, "use_default").to_string(),
        );
        return fields;
    }
    if action == "actions-settings" {
        let mut fields: HashMap<String, String> = [
            "engine",
            "run_timeout_minutes",
            "job_cpus",
            "job_memory_gib",
            "cache_size_gib",
            "cache_retention_days",
        ]
        .into_iter()
        .filter_map(|name| {
            values
                .get(name)
                .map(|value| (name.to_owned(), value.clone()))
        })
        .collect();
        fields.insert(
            "enabled".to_owned(),
            checkbox(values, "enabled").to_string(),
        );
        return fields;
    }
    let fields: &[&str] = match action {
        "create-branch" => &["name", "sha"],
        "create-tag" => &["name", "sha", "message"],
        "delete-branch" | "delete-tag" | "rename" => &["name"],
        "transfer" => &["new_owner", "confirmation"],
        "watch-level" => &["level", "events_issues", "events_pulls", "events_releases"],
        "about" => &["description", "website", "topics"],
        "settings" => &["description", "visibility"],
        "default-branch" => &["branch"],
        "collaborator" | "invite-collaborator" => &["username", "access"],
        "cancel-invitation" => &["invitation_id"],
        "autolink-delete" => &["autolink_id"],
        "remove-collaborator" | "remove-invited-collaborator" => &["username"],
        "label" => &["name", "color", "description"],
        "edit-label" => &["label_id", "name", "color", "description"],
        "delete-label" => &["label_id"],
        "delete" => &["confirmation"],
        _ => &[],
    };
    fields
        .iter()
        .filter_map(|key| {
            values
                .get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect()
}

fn reject(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    values: &HashMap<String, String>,
    status: StatusCode,
    message: &str,
) -> Response {
    with_status(
        ctx.page_model(
            "Action could not be completed",
            crate::webui::app::Page::Repositories(view::page::Page::RejectedForm {
                repo: loaded.chrome.clone(),
                action: action.to_owned(),
                message: message.to_owned(),
                values: safe_submission(action, values),
            }),
        ),
        status,
    )
}

/// What a repository form action needs. `post_form` reads this one table for every check.
struct FormAction {
    name: &'static str,
    /// The repository page the action belongs to, shown again after a refusal.
    page: &'static str,
    flags: u8,
}

/// Reads Git metadata, so it needs readable storage.
const GIT: u8 = 1;
/// The form carries the id of the repository it was rendered for.
const BOUND: u8 = 2;
/// The form must send `confirm=yes`.
const CONFIRM: u8 = 4;
/// Needs write access.
const WRITE: u8 = 8;
/// Needs repository administration.
const ADMIN: u8 = 16;

const fn form_action(name: &'static str, page: &'static str, flags: u8) -> FormAction {
    FormAction { name, page, flags }
}

// `write-file` has no GIT flag: with storage down it still opens its editor.
#[rustfmt::skip]
const FORM_ACTIONS: &[FormAction] = &[
    form_action("star", "", 0),
    form_action("unstar", "", BOUND),
    form_action("watch", "", 0),
    form_action("unwatch", "", 0),
    form_action("watch-level", "", 0),
    form_action("fork", "", GIT),
    form_action("create-branch", "/branches", GIT | BOUND | WRITE),
    form_action("delete-branch", "/branches", GIT | BOUND | WRITE | CONFIRM),
    form_action("create-tag", "/tags", GIT | BOUND | WRITE),
    form_action("delete-tag", "/tags", GIT | BOUND | WRITE | CONFIRM),
    form_action("settings", "/settings", BOUND | ADMIN),
    form_action("merge-methods", "/settings", BOUND | ADMIN),
    form_action("actions-settings", "/settings", BOUND | ADMIN),
    form_action("oidc-settings", "/settings?tab=actions", BOUND | ADMIN),
    form_action("environment-create", "/settings?tab=secrets", BOUND | ADMIN),
    form_action("environment-secret-save", "/settings?tab=secrets", BOUND | ADMIN),
    form_action("environment-secret-delete", "/settings?tab=secrets", BOUND | ADMIN | CONFIRM),
    form_action("environment-variable-save", "/settings?tab=secrets", BOUND | ADMIN),
    form_action("environment-variable-delete", "/settings?tab=secrets", BOUND | ADMIN | CONFIRM),
    form_action("code-security", "/settings", BOUND | ADMIN),
    form_action("secret-pattern-publish", "/settings", BOUND | ADMIN),
    form_action("secret-save", "/settings?tab=secrets", BOUND | ADMIN),
    form_action("variable-save", "/settings?tab=secrets", BOUND | WRITE),
    form_action("variable-delete", "/settings?tab=secrets", BOUND | WRITE | CONFIRM),
    form_action("secret-delete", "/settings?tab=secrets", BOUND | ADMIN | CONFIRM),
    form_action("rename", "/settings", BOUND | ADMIN | CONFIRM),
    form_action("transfer", "/settings", BOUND | ADMIN),
    form_action("delete", "/settings", BOUND | ADMIN),
    form_action("collaborator", "/settings", BOUND | ADMIN),
    form_action("invite-collaborator", "/settings", BOUND | ADMIN),
    form_action("cancel-invitation", "/settings", BOUND | ADMIN),
    form_action("immutable-releases", "/settings", BOUND | ADMIN),
    form_action("autolink-add","/settings",BOUND | ADMIN),
    form_action("autolink-delete","/settings",BOUND | ADMIN | CONFIRM),
    form_action("remove-invited-collaborator","/settings",BOUND | ADMIN | CONFIRM),
    form_action("remove-collaborator", "/settings", BOUND | ADMIN | CONFIRM),
    form_action("label", "/labels", BOUND | ADMIN),
    form_action("edit-label", "/labels", BOUND | ADMIN),
    form_action("delete-label", "/labels", BOUND | ADMIN | CONFIRM),
    form_action("about", "", BOUND | WRITE),
    form_action("write-file", "", 0),
    form_action("delete-file", "", GIT | BOUND),
    form_action("protection-save", "/settings", BOUND | ADMIN),
    form_action("protection-delete", "/settings", BOUND | ADMIN),
    form_action("archive", "/settings", BOUND | ADMIN | CONFIRM),
    form_action("unarchive", "/settings", BOUND | ADMIN | CONFIRM),
    form_action("default-branch", "/settings", GIT | BOUND | ADMIN),
    form_action("template","/settings",BOUND | ADMIN),
    form_action("repository-property-set", "/settings?tab=properties", BOUND | ADMIN),
    form_action("repository-property-unset", "/settings?tab=properties", BOUND | ADMIN),
    form_action("webhook-save", "", BOUND | ADMIN),
    form_action("webhook-delete", "", BOUND | ADMIN),
    form_action("webhook-ping", "", BOUND | ADMIN),
    form_action("webhook-redeliver", "", BOUND | ADMIN),
    form_action("deploy-key-add", "/settings?tab=deploy-keys", BOUND | ADMIN),
    form_action("deploy-key-delete", "/settings?tab=deploy-keys", BOUND | ADMIN | CONFIRM),
];

impl FormAction {
    fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

fn form_page_suffix(name: &str) -> &'static str {
    FORM_ACTIONS
        .iter()
        .find(|row| row.name == name)
        .map_or("", |row| row.page)
}

/// The editor or delete page a rejected save names in `from`, when it is a page of this repository
/// and a browser cannot resolve it anywhere else: no dot segments (in any escaping), empty
/// segments, backslashes or control characters.
fn editor_return<'a>(from: &'a str, base: &str) -> Option<&'a str> {
    if !["/edit/", "/new/", "/delete/"]
        .iter()
        .any(|kind| from.starts_with(&format!("{base}{kind}")))
    {
        return None;
    }
    // The browser resolves `%2e%2e` as dot segments, so it is decoded once. `%252e` is a file
    // named `%2e` and stays allowed.
    let decoded = crate::webui::decode_component(from);
    let unsafe_text = decoded.contains("//")
        || decoded.chars().any(|c| c == '\\' || c.is_control())
        || decoded
            .split('/')
            .any(|segment| matches!(segment, "." | ".."));
    (!unsafe_text).then_some(from)
}

/// A rejected submission is shown at the form's own URL, which accepts only POST. A reload
/// or a copied address sends a GET there instead, and lands on the page the form belongs to.
/// The editor and delete forms name their page in `from`.
pub(super) async fn get_form(
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let base = format!(
        "/{}/{}",
        crate::webui::encode_component(&params["owner"]),
        crate::webui::encode_component(&params["repo"])
    );
    let editor = query
        .get("from")
        .and_then(|from| editor_return(from, &base));
    let target = match (params["action"].as_str(), editor) {
        ("write-file" | "delete-file", Some(from)) => from.to_owned(),
        (action, _) => format!("{base}{}", form_page_suffix(action)),
    };
    crate::backend::api::http::render::redirect_response(&target)
}

pub(super) async fn post_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
    // The pairs keep the repeated checkboxes of a rule's force push list.
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let form: HashMap<String, String> = pairs.iter().cloned().collect();
    let owner = &params["owner"];
    let name = &params["repo"];
    let action = &params["action"];
    // This URL accepts only POST, so every failure page must link its appearance switch to
    // the repository page instead. Set it before the checks that can fail.
    ctx.return_to = Some(format!(
        "/{}/{}{}",
        crate::webui::encode_component(owner),
        crate::webui::encode_component(name),
        form_page_suffix(action)
    ));
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    if let Err(response) = ctx.require_user() {
        return response;
    }
    let Some(row) = FORM_ACTIONS.iter().find(|row| row.name == action.as_str()) else {
        return ctx.error(StatusCode::NOT_FOUND, "Repository action not found.");
    };
    let database_only = !row.has(GIT) && action != "write-file";
    let loaded = match if database_only {
        super::chrome::load_database(&ctx, owner, name).await
    } else {
        load_metadata(&ctx, owner, name).await
    } {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    // A repository form names the repository it was rendered for. After a rename, another
    // repository can take the old owner and name, and the form must not reach that one.
    let repository_id = loaded.repository.id.to_string();
    if row.has(BOUND) && field(&form, "repository_id") != repository_id {
        return ctx.error(
            StatusCode::CONFLICT,
            "The repository changed. Reload the page and try again.",
        );
    }
    // A refused save returns the editor with the submitted text, not an error page.
    if action == "write-file" && field(&form, "repository_id") != repository_id {
        return replaced_repository_editor(ctx, &loaded, &form).await;
    }
    // Only actions that consume Git metadata need readable storage.
    if loaded.git_unavailable && row.has(GIT) {
        return git_unavailable_response(&ctx);
    }
    if action == "write-file" {
        return write_form(ctx, &loaded, &form).await;
    }
    if action == "delete-file" {
        return delete_form(ctx, &loaded, &form).await;
    }
    // Only a visibility change needs the confirmation. A description-only save does not.
    let changes_visibility = action == "settings"
        && (field(&form, "visibility") == "private") != loaded.repository.is_private;
    if changes_visibility || row.has(CONFIRM) {
        if let Err(message) = confirmation(&form) {
            return reject(
                &ctx,
                &loaded,
                action,
                &form,
                StatusCode::BAD_REQUEST,
                message,
            );
        }
    }
    if action == "delete" && field(&form, "confirmation") != format!("{owner}/{name}") {
        return reject(
            &ctx,
            &loaded,
            action,
            &form,
            StatusCode::BAD_REQUEST,
            "Type the complete owner/repository name to confirm permanent deletion.",
        );
    }
    if action == "transfer" && field(&form, "confirmation") != name.as_str() {
        return reject(
            &ctx,
            &loaded,
            action,
            &form,
            StatusCode::BAD_REQUEST,
            "Type the repository name to confirm the transfer.",
        );
    }
    if row.has(ADMIN) && !loaded.chrome.can_admin {
        return ctx.repo_error(&loaded.chrome, StatusCode::FORBIDDEN, REPO_ADMIN_REQUIRED);
    }
    if action.starts_with("repository-property-") {
        return super::properties::form(&ctx, &loaded, action, &form, &pairs).await;
    }
    if action.starts_with("protection-") {
        return protection_form(&ctx, &loaded, action, &form, &pairs).await;
    }
    if action.starts_with("webhook-") {
        return webhook_form(&ctx, &loaded, action, &form, &pairs).await;
    }
    if action.starts_with("environment-") {
        return super::settings::environment_values::form(&ctx, &loaded, action, &form).await;
    }
    if action.starts_with("variable-") {
        return super::settings::variables::form(&ctx, &loaded, action, &form).await;
    }
    if matches!(action.as_str(), "secret-save" | "secret-delete") {
        return super::settings::secrets::form(&ctx, &loaded, action, &form).await;
    }
    if action.starts_with("deploy-key-") {
        return super::deploy_keys::form(&ctx, &loaded, action, &form).await;
    }
    if row.has(WRITE) && !loaded.chrome.can_write {
        return ctx.error(StatusCode::FORBIDDEN, "Write access is required.");
    }
    if action == "settings" && !matches!(field(&form, "visibility").as_str(), "public" | "private")
    {
        return reject(
            &ctx,
            &loaded,
            action,
            &form,
            StatusCode::BAD_REQUEST,
            "Choose public or private visibility.",
        );
    }
    let repository = &loaded.repository;
    let result = mutate(&ctx, repository, owner, name, action, &form).await;
    match result {
        Ok(path) => ctx.redirect(&path),
        Err((status, Json(error))) => reject(&ctx, &loaded, action, &form, status, &error.message),
    }
}

/// The name, `#rrggbb` color and description of a label form. Creating and editing a label check
/// them the same way.
fn label_fields(form: &HashMap<String, String>) -> ApiResult<(String, String, String)> {
    let name = field(form, "name");
    let description = field(form, "description");
    let color = parse_label_color(&field(form, "color"));
    match color {
        Some(color)
            if crate::backend::issues::validate_label(&name, Some(description.as_str()))
                .is_ok() =>
        {
            Ok((name, color, description))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "Invalid label name or color",
            )),
        )),
    }
}

/// The label that an edit or delete form names.
fn label_id(form: &HashMap<String, String>) -> ApiResult<i64> {
    field(form, "label_id").parse::<i64>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("validation_error", "Invalid label ID")),
        )
    })
}

async fn mutate(
    ctx: &PageContext,
    repository: &db::models::Repository,
    owner: &str,
    name: &str,
    action: &str,
    form: &HashMap<String, String>,
) -> ApiResult<String> {
    let pool = &ctx.state.pool;
    let actor = ctx.user.as_ref().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ApiError::new(
                "unauthorized",
                "Sign in to perform this action",
            )),
        )
    })?;
    let base = crate::webui::repository_path(owner, name);
    let cluster = ctx.state.cluster.as_ref();
    let ci = &ctx.state.ci_runs;
    match action {
        "star" | "unstar" | "watch" | "unwatch" => {
            use crate::backend::repositories::engagement::{change, Engagement};
            let kind = if matches!(action, "star" | "unstar") {
                Engagement::Star
            } else {
                Engagement::Watch
            };
            change(
                pool,
                &ctx.state.notification_broadcaster,
                actor.id,
                repository.id,
                kind,
                !action.starts_with("un"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::engagement_error)?;
            Ok(stars_return(form).unwrap_or(base))
        }
        "watch-level" => {
            let events: Vec<&str> = ["issues", "pulls", "releases"]
                .into_iter()
                .filter(|name| checkbox(form, &format!("events_{name}")))
                .collect();
            crate::backend::repositories::engagement::set_level(
                pool,
                actor.id,
                repository.id,
                &field(form, "level"),
                &events,
            )
            .await
            .map_err(crate::backend::api::repos::responses::engagement_error)?;
            Ok(base)
        }
        "fork" => {
            let fork = lifecycle::fork(
                pool,
                cluster,
                &ctx.state.repos_path,
                &ctx.state.notification_broadcaster,
                actor.id,
                repository.id,
                owner,
            )
            .await
            .map_err(lifecycle_error)?;
            Ok(format!(
                "/{}/{}",
                crate::webui::encode_component(&actor.username),
                crate::webui::encode_component(&fork.name)
            ))
        }
        "create-branch" => {
            crate::backend::repositories::refs::create_branch(
                pool,
                cluster,
                ci,
                actor.id,
                owner,
                repository.id,
                &field(form, "name"),
                &field(form, "sha"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(format!("{base}/branches"))
        }
        "default-branch" => {
            crate::backend::repositories::access::check_admin(pool, repository.id, actor.id)
                .await
                .map_err(access_error)?;
            crate::backend::repositories::refs::set_default_branch(
                pool,
                cluster,
                actor.id,
                repository,
                &field(form, "branch"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(settings_section_url(&base, action))
        }
        "delete-branch" => {
            crate::backend::repositories::refs::delete_branch(
                pool,
                cluster,
                ci,
                actor.id,
                repository.id,
                &field(form, "name"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(format!("{base}/branches"))
        }
        "create-tag" => {
            let message = field(form, "message");
            crate::backend::repositories::refs::create_tag(
                pool,
                cluster,
                ci,
                actor.id,
                owner,
                repository.id,
                &field(form, "name"),
                &field(form, "sha"),
                (!message.is_empty()).then_some(message.as_str()),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(format!("{base}/tags"))
        }
        "delete-tag" => {
            crate::backend::repositories::refs::delete_tag(
                pool,
                cluster,
                actor.id,
                repository.id,
                &field(form, "name"),
            )
            .await
            .map_err(crate::backend::api::repos::responses::ref_error)?;
            Ok(format!("{base}/tags"))
        }
        "settings" => {
            crate::backend::repositories::access::check_admin(pool, repository.id, actor.id)
                .await
                .map_err(access_error)?;
            lifecycle::validate_description(&field(form, "description"))
                .map_err(lifecycle_error)?;
            let private = field(form, "visibility") == "private";
            if !private
                && lifecycle::visibility_locked(pool, repository)
                    .await
                    .map_err(lifecycle_error)?
            {
                return Err((
                    StatusCode::FORBIDDEN,
                    Json(ApiError::new(
                        "visibility_locked",
                        "A fork of nonpublic history cannot be made public.",
                    )),
                ));
            }
            db::repositories::records::update_repository(
                pool,
                actor.id,
                repository.id,
                Some(&field(form, "description")),
                db::repositories::records::VisibilityUpdate {
                    is_private: private,
                    expected: if confirmation(form).is_ok() {
                        None
                    } else {
                        Some(repository.is_private)
                    },
                },
                db::repositories::records::MergeMethodUpdate {
                    merge: None,
                    squash: None,
                    rebase: None,
                },
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(format!("{base}/settings?tab=general&saved=general"))
        }
        "merge-methods" => {
            db::repositories::records::set_merge_methods(
                pool,
                actor.id,
                repository.id,
                db::repositories::records::MergeMethodUpdate {
                    merge: Some(checkbox(form, "allow_merge_commit")),
                    squash: Some(checkbox(form, "allow_squash_merge")),
                    rebase: Some(checkbox(form, "allow_rebase_merge")),
                },
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(settings_section_url(&base, action))
        }
        "secret-pattern-publish" => {
            use crate::backend::repositories::secret_scanning::patterns::Owner;
            let id = field(form, "pattern_id").parse().map_err(|_| {
                ApiError::failure(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "validation_error",
                    "Invalid custom pattern",
                )
            })?;
            let scope = if field(form, "organization") == "true" {
                Owner::Organization(repository.owner_org_id.ok_or_else(|| {
                    ApiError::failure(StatusCode::NOT_FOUND, "not_found", "Organization not found")
                })?)
            } else {
                Owner::Repository(repository.id)
            };
            db::secret_scanning::publish_pattern(pool, actor.id, &db::users::accounts::AccountCredential::Session(&crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default()), scope, id)
                .await
                .map_err(
                    |error| match error.downcast_ref::<db::secret_scanning::Refusal>() {
                        Some(db::secret_scanning::Refusal::Denied) => ApiError::failure(
                            StatusCode::FORBIDDEN,
                            "forbidden",
                            "Pattern administration required",
                        ),
                        Some(db::secret_scanning::Refusal::Missing) => ApiError::failure(
                            StatusCode::NOT_FOUND,
                            "not_found",
                            "Pattern not found",
                        ),
                        _ => ApiError::failure(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "unavailable",
                            "Custom patterns unavailable",
                        ),
                    },
                )?;
            Ok(settings_section_url(&base, action))
        }
        "code-security" => {
            let settings = db::secret_scanning::Settings {
                enabled: checkbox(form, "secret_scanning"),
                push_protection: checkbox(form, "push_protection"),
            };
            if settings.enabled && ctx.state.secret_encryptor.is_none() {
                return Err(ApiError::failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    "Secret scanning requires SECRETS_MASTER_KEY",
                ));
            }
            db::secret_scanning::save_settings(pool, actor.id, repository.id, &settings)
                .await
                .map_err(
                    |error| match error.downcast_ref::<db::secret_scanning::Refusal>() {
                        Some(db::secret_scanning::Refusal::Invalid) => ApiError::failure(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "validation_error",
                            "Push protection requires secret scanning",
                        ),
                        Some(db::secret_scanning::Refusal::Denied) => ApiError::failure(
                            StatusCode::FORBIDDEN,
                            "forbidden",
                            "Repository administration required",
                        ),
                        _ => ApiError::failure(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "unavailable",
                            "Secret scanning settings unavailable",
                        ),
                    },
                )?;
            Ok(settings_section_url(&base, action))
        }
        "oidc-settings" => {
            let settings = db::oidc::Settings::from_keys(
                checkbox(form, "use_default"),
                &field(form, "include_claim_keys"),
            );
            db::oidc::save_settings(
                pool,
                actor.id,
                db::oidc::Scope::Repository(repository.id),
                &settings,
            )
            .await
            .map_err(|error| {
                let status = match error.downcast_ref::<db::oidc::Refusal>() {
                    Some(db::oidc::Refusal::Denied) => StatusCode::FORBIDDEN,
                    Some(_) => StatusCode::UNPROCESSABLE_ENTITY,
                    None => StatusCode::SERVICE_UNAVAILABLE,
                };
                ApiError::failure(status, "oidc_settings", "Could not save OIDC settings")
            })?;
            Ok(format!("{base}/settings?tab=actions"))
        }
        "actions-settings" => {
            let invalid = |message: String| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ApiError::new("validation_error", &message)),
                )
            };
            let site = db::actions::get_actions_settings(pool).await.map_err(|_| {
                ApiError::failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    "The Actions settings are unavailable right now",
                )
            })?;
            // A limit may be lower than the site's, not higher.
            let limit = |name: &str, label: &str, max: i32, site: Option<i32>| {
                let value =
                    admin_actions::optional_number(form, name, label, max).map_err(invalid)?;
                match (value, site) {
                    (Some(value), Some(site)) if value > site => Err(invalid(format!(
                        "{label} can be at most {site}, the site's limit."
                    ))),
                    _ => Ok(value),
                }
            };
            let engine = field(form, "engine");
            if !engine.is_empty() && !site.engine_enabled(&engine) {
                return Err(invalid("Choose Site default or an enabled engine.".into()));
            }
            let settings = db::actions::RepoActionsSettings {
                org_cache_size_gib: None,
                org_cache_retention_days: None,
                engine: (!engine.is_empty()).then_some(engine),
                cache_size_gib: limit(
                    "cache_size_gib",
                    "Cache size in GiB",
                    10240,
                    site.cache_size_gib.or(Some(10)),
                )?,
                cache_retention_days: limit(
                    "cache_retention_days",
                    "Cache expiry in days",
                    365,
                    site.cache_retention_days.or(Some(7)),
                )?,
                enabled: checkbox(form, "enabled"),
                run_timeout_minutes: limit(
                    "run_timeout_minutes",
                    "The timeout",
                    admin_actions::MAX_TIMEOUT_MINUTES,
                    site.run_timeout_minutes,
                )?,
                job_cpus: limit(
                    "job_cpus",
                    "CPUs per job",
                    admin_actions::MAX_CPUS,
                    site.job_cpus,
                )?,
                job_memory_gib: limit(
                    "job_memory_gib",
                    "Memory per job",
                    admin_actions::MAX_MEMORY_GIB,
                    site.job_memory_gib,
                )?,
            };
            db::actions::save_repo_actions_settings(pool, actor.id, repository.id, &settings)
                .await
                .map_err(repository_settings_error)?;
            Ok(settings_section_url(&base, action))
        }
        "about" => {
            let description = field(form, "description");
            lifecycle::validate_description(&description).map_err(lifecycle_error)?;
            let website = db::repositories::topics::parse_website(&field(form, "website"))
                .map_err(repository_settings_error)?;
            // Topics are typed as a list, separated by commas or spaces.
            let topics = db::repositories::topics::parse_topics(
                field(form, "topics")
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .collect::<Vec<_>>(),
            )
            .map_err(repository_settings_error)?;
            db::repositories::topics::update_about(
                pool,
                actor.id,
                repository.id,
                &description,
                website.as_deref(),
                &topics,
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(base)
        }
        "rename" => {
            crate::backend::repositories::access::check_admin(pool, repository.id, actor.id)
                .await
                .map_err(access_error)?;
            let updated = db::repositories::records::rename_repository(
                pool,
                actor.id,
                repository.id,
                &field(form, "name"),
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(crate::webui::repository_path(owner, &updated.name))
        }
        "transfer" => {
            let updated = lifecycle::transfer(
                pool,
                ctx.state.cluster.as_ref(),
                actor.id,
                repository.id,
                &field(form, "new_owner"),
            )
            .await
            .map_err(lifecycle_error)?;
            let new_owner = repository_owner(pool, &updated).await?;
            Ok(crate::webui::repository_path(&new_owner, &updated.name))
        }
        "archive" | "unarchive" => {
            crate::backend::repositories::access::check_admin(pool, repository.id, actor.id)
                .await
                .map_err(access_error)?;
            // Archiving an archived repository, or the reverse, changes nothing and is not an error.
            db::repositories::records::set_repository_archived(
                pool,
                actor.id,
                repository.id,
                action == "archive",
            )
            .await
            .map_err(repository_settings_error)?;
            Ok(settings_section_url(&base, action))
        }
        "delete" => {
            lifecycle::delete(pool, cluster, actor.id, repository.id)
                .await
                .map_err(lifecycle_error)?;
            Ok(crate::webui::profile_path(owner))
        }
        "template" => {
            db::repositories::records::patch_repository(
                pool,
                actor.id,
                repository.id,
                &db::repositories::records::RepositoryChanges {
                    is_template: Some(checkbox(form, "is_template")),
                    ..Default::default()
                },
            )
            .await
            .map_err(lifecycle::storage_error)
            .map_err(lifecycle_error)?;
            Ok(settings_section_url(&base, action))
        }
        "remove-invited-collaborator" => {
            crate::backend::repositories::collaborators::remove(
                pool,
                actor.id,
                repository.id,
                &field(form, "username"),
            )
            .await
            .map_err(invitation_error)?;
            Ok(settings_section_url(&base, action))
        }
        "immutable-releases" => {
            confirmation(form).map_err(|message| {
                ApiError::failure(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "validation_error",
                    message,
                )
            })?;
            crate::backend::repositories::releases::immutable::set_enabled(
                pool,
                actor.id,
                repository.id,
                checkbox(form, "enabled"),
            )
            .await
            .map_err(immutable_releases_error)?;
            Ok(settings_section_url(&base, action))
        }
        "autolink-add" => {
            let input = db::repositories::autolinks::NewAutolink::new(
                &field(form, "key_prefix"),
                &field(form, "url_template"),
                checkbox(form, "is_alphanumeric"),
            )
            .map_err(|error| {
                ApiError::failure(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "validation_error",
                    &error.to_string(),
                )
            })?;
            db::repositories::autolinks::create(pool, actor.id, repository.id, &input)
                .await
                .map_err(autolink_error)?;
            Ok(settings_section_url(&base, action))
        }
        "autolink-delete" => {
            let id = field(form, "autolink_id").parse::<i64>().map_err(|_| {
                ApiError::failure(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "validation_error",
                    "Invalid autolink",
                )
            })?;
            db::repositories::autolinks::remove(pool, actor.id, repository.id, id)
                .await
                .map_err(autolink_error)?;
            Ok(settings_section_url(&base, action))
        }
        "invite-collaborator" => {
            let permission =
                crate::backend::repositories::collaborators::permission(&field(form, "access"))
                    .ok_or_else(|| {
                        ApiError::failure(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "validation_error",
                            "Invalid permission",
                        )
                    })?;
            crate::backend::repositories::collaborators::add(
                pool,
                actor.id,
                repository.id,
                &field(form, "username"),
                permission,
            )
            .await
            .map_err(invitation_error)?;
            Ok(settings_section_url(&base, action))
        }
        "cancel-invitation" => {
            let id = field(form, "invitation_id").parse::<i64>().map_err(|_| {
                ApiError::failure(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "validation_error",
                    "Invalid invitation",
                )
            })?;
            crate::backend::repositories::collaborators::update_invitation(
                pool,
                actor.id,
                repository.id,
                id,
                None,
            )
            .await
            .map_err(invitation_error)?;
            Ok(settings_section_url(&base, action))
        }
        "collaborator" | "remove-collaborator" => {
            let permission = if action == "remove-collaborator" {
                None
            } else {
                let level = db::models::AccessLevel::parse_collaborator(&field(form, "access"));
                Some(level.ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ApiError::new(
                            "validation_error",
                            "Choose a valid collaborator access level.",
                        )),
                    )
                })?)
            };
            crate::backend::repositories::access::change_collaborator(
                pool,
                actor.id,
                repository.id,
                &field(form, "username"),
                permission,
            )
            .await
            .map_err(access_error)?;
            Ok(settings_section_url(&base, action))
        }
        "label" => {
            let (name, color, description) = label_fields(form)?;
            db::labels::create_label(
                pool,
                actor.id,
                repository.id,
                &name,
                &color,
                Some(&description),
            )
            .await
            .map_err(label_write_error)?;
            Ok(format!("{base}/labels"))
        }
        "edit-label" => {
            let id = label_id(form)?;
            let (name, color, description) = label_fields(form)?;
            db::labels::update_label(
                pool,
                actor.id,
                repository.id,
                id,
                Some(&name),
                Some(&color),
                (serde_json::to_string(&description).unwrap_or_default()
                    != field(form, "original_description"))
                .then_some(description.as_str()),
            )
            .await
            .map_err(label_write_error)?;
            Ok(format!("{base}/labels"))
        }
        "delete-label" => {
            let id = label_id(form)?;
            db::labels::delete_label(pool, actor.id, repository.id, id)
                .await
                .map_err(label_write_error)?;
            Ok(format!("{base}/labels"))
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", "Unknown repository action")),
        )),
    }
}

pub(super) async fn page(
    axum::extract::State(state): axum::extract::State<
        std::sync::Arc<crate::backend::app::AppState>,
    >,
    headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::extract::Path(params): axum::extract::Path<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let ctx =
        match crate::backend::api::http::context::PageContext::load(state, headers, &uri).await {
            Ok(ctx) => ctx,
            Err(response) => return response,
        };
    let tab = view::settings::settings_tab(ctx.query.get("tab").map_or("", String::as_str), true);
    let loaded = match if tab == "general" || tab.is_empty() {
        load_metadata(&ctx, &params["owner"], &params["repo"]).await
    } else {
        super::chrome::load_database(&ctx, &params["owner"], &params["repo"]).await
    } {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    match settings_page(&ctx, &loaded).await {
        Ok(response) | Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::repos::chrome::hex_revision;
    use crate::backend::api::repos::code::contents::editor_from;
    #[test]
    fn force_push_names_preserve_case_distinct_accounts() {
        let form = HashMap::from([("force_push_extra".into(), "Alice alice Alice".into())]);
        assert_eq!(force_push_names(&form, &[]), vec!["Alice", "alice"]);
    }

    #[test]
    fn confirmations_are_explicit_not_truthy() {
        let mut form = HashMap::new();
        assert!(confirmation(&form).is_err());
        form.insert("confirm".into(), "false".into());
        assert!(confirmation(&form).is_err());
        form.insert("confirm".into(), "yes".into());
        assert!(confirmation(&form).is_ok());
    }
    #[test]
    fn editor_redisplay_retains_text_and_expected_revision() {
        let form = HashMap::from([
            ("content".into(), "</textarea><script>x</script>".into()),
            ("expected_revision".into(), "old".into()),
            ("creating".into(), "yes".into()),
            ("intent".into(), "preview".into()),
            ("commit_target".into(), "new_branch".into()),
        ]);
        let editor = editor_from(&form);
        assert_eq!(editor.content, "</textarea><script>x</script>");
        assert_eq!(editor.expected_revision, "old");
        assert!(editor.creating && editor.preview);
        assert!(editor.commit_to_new_branch);
    }
    #[test]
    fn validation_models_never_reflect_undeclared_secrets_or_authority() {
        let values = HashMap::from([
            ("name".into(), "feature/x".into()),
            ("sha".into(), "abcdef1234".into()),
            ("password".into(), "must-not-appear".into()),
            ("disk_path".into(), "must-not-appear".into()),
            ("csrf_token".into(), "must-not-appear".into()),
            ("confirm".into(), "yes".into()),
        ]);
        let safe = safe_submission("create-branch", &values);
        assert_eq!(safe.len(), 2);
        assert_eq!(safe["name"], "feature/x");
        assert_eq!(safe["sha"], "abcdef1234");
        assert!(!serde_json::to_string(&safe)
            .unwrap()
            .contains("must-not-appear"));
        assert!(safe_submission("star", &values).is_empty());
    }
    #[test]
    fn retries_keep_transfer_confirmation_and_watch_choices() {
        let form = HashMap::from([
            ("new_owner".into(), "unknown".into()),
            ("confirmation".into(), "project".into()),
            ("level".into(), "custom".into()),
            ("events_issues".into(), "yes".into()),
        ]);
        assert_eq!(
            safe_submission("transfer", &form)["confirmation"],
            "project"
        );
        assert_eq!(safe_submission("watch-level", &form)["level"], "custom");
        assert_eq!(
            safe_submission("watch-level", &form)["events_issues"],
            "yes"
        );
    }

    #[test]
    fn retries_keep_the_ci_engine_and_oidc_choices() {
        let form = HashMap::from([
            ("engine".into(), "native".into()),
            ("job_memory_gib".into(), "999".into()),
            ("use_default".into(), "on".into()),
            ("include_claim_keys".into(), "repo, bad key".into()),
        ]);
        assert_eq!(
            safe_submission("actions-settings", &form)["engine"],
            "native"
        );
        let oidc = safe_submission("oidc-settings", &form);
        assert_eq!(oidc["use_default"], "true");
        assert_eq!(oidc["include_claim_keys"], "repo, bad key");
    }

    #[test]
    fn force_push_accounts_are_case_sensitive() {
        let form = HashMap::from([("force_push_extra".into(), "Alice alice".into())]);
        assert_eq!(force_push_names(&form, &[]), ["Alice", "alice"]);
    }

    #[test]
    fn command_option_like_revisions_are_not_accepted() {
        assert!(!hex_revision("--all"));
        assert!(!hex_revision("main~1"));
        assert!(hex_revision("abcd1234"));
    }
}
