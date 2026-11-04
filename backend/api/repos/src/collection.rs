use crate::paging::json_page;
use crate::paging::PageQuery;
use crate::responses::repository_owners;
use crate::responses::repository_topics;
use crate::responses::RepoResponse;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::Form;
use axum::Json;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::error::ApiError;
use codeyard_api_http::session::get_current_user;
use codeyard_api_http::shared::repos::access::access_error;
use codeyard_api_http::shared::repos::responses::repository_fork_counts;
use codeyard_app::app::AppState;
use codeyard_app::repositories::lifecycle::InitialFiles;
use codeyard_app::repositories::templates::Template;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_orgs::organizations::OrganizationPage;
use codeyard_db_repos::repositories::records::RepositoryFilter;
use codeyard_db_repos::repositories::records::RepositorySort;
use codeyard_identity::repository_access::AccessError;
use codeyard_webui::app::Page as AppPage;
use codeyard_webui::pages::discovery as view;
use codeyard_webui::pages::discovery::NewRepositoryForm;
use codeyard_webui::pages::discovery::RepositoryCreationChoices;
use codeyard_webui::pages::discovery::RepositoryCreationOrganization;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

mod properties;
pub(crate) async fn list_repos(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Vec<RepoResponse>>, (StatusCode, Json<ApiError>)> {
    let (offset, limit) = json_page(query.page, Some(query.per_page.unwrap_or(100)))?;
    let repos = codeyard_db_repos::repositories::records::search_public_repositories(
        &state.pool,
        "",
        RepositoryFilter::default(),
        RepositorySort::Recent,
        limit as i64,
        offset as i64,
    )
    .await
    .map_err(|_| access_error(AccessError::Unavailable))?;

    let ids = repos.iter().map(|repo| repo.id).collect::<Vec<_>>();
    let mut owners = repository_owners(&state.pool, &ids).await?;
    let mut topics = repository_topics(&state.pool, &ids).await?;
    let mut fork_counts = repository_fork_counts(&state.pool, None, &ids).await?;
    let readable =
        codeyard_db_repos::repositories::access::readable_repository_ids(&state.pool, None, &ids)
            .await
            .map_err(|_| access_error(AccessError::Unavailable))?;
    let mut responses = Vec::with_capacity(repos.len());
    for repository in repos {
        if !readable.contains(&repository.id) {
            continue;
        }
        let owner = owners.remove(&repository.id).expect("validated owner");
        let topics = topics.remove(&repository.id).unwrap_or_default();
        responses.push(RepoResponse::new(
            &repository,
            &owner,
            topics,
            None,
            fork_counts.remove(&repository.id).unwrap_or_default(),
        ));
    }

    Ok(Json(responses))
}

#[derive(Deserialize)]
pub struct CreateRepoRequest {
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub is_private: bool,
    /// Select an organization target, never an arbitrary personal owner.
    pub owner_org_id: Option<i64>,
    pub custom_properties: Option<
        HashMap<
            String,
            Option<codeyard_db_core::queries::organizations::properties::PropertyValue>,
        >,
    >,
}

pub async fn create_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateRepoRequest>,
) -> Result<(StatusCode, Json<RepoResponse>), (StatusCode, Json<ApiError>)> {
    let pool = state.pool.clone();
    let user = get_current_user(&pool, &headers).await?;
    let authority = codeyard_db_core::queries::credentials::Actor::from((
        user.id,
        codeyard_api_http::session::extract_session_id(&headers).unwrap_or_default(),
    ));

    let creation_owner = codeyard_db_repos::repositories::creation::RepositoryCreationOwner {
        actor_id: user.id,
        target: match req.owner_org_id {
            Some(id) if id > 0 => {
                codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(id)
            }
            None => codeyard_db_repos::repositories::creation::RepositoryTarget::Personal(user.id),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ApiError::new(
                        "validation_error",
                        "Invalid organization target",
                    )),
                ))
            }
        },
    };
    codeyard_app::repositories::lifecycle::validate_description(
        req.description.as_deref().unwrap_or(""),
    )
    .map_err(crate::responses::lifecycle_error)?;
    let properties = req
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
    let repo = codeyard_app::repositories::lifecycle::provision_with_properties(
        (&pool, state.cluster.as_ref()),
        &state.repos_path,
        codeyard_app::repositories::lifecycle::CreationInput {
            owner: creation_owner,
            properties: &properties,
            admission: codeyard_db_repos::repositories::creation::CreationAdmission {
                authority: Some(&authority),
                advisory: None,
            },
        },
        &req.name,
        req.description.as_deref(),
        req.is_private,
        None,
    )
    .await
    .map_err(crate::responses::lifecycle_error)?;

    let owner = codeyard_db_repos::repositories::records::repository_owner_name(&pool, repo.id)
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "unavailable",
                    "Repository owner is temporarily unavailable",
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError::new(
                    "unavailable",
                    "Repository owner is temporarily unavailable",
                )),
            )
        })?;
    let forks = repository_fork_counts(&pool, Some(&headers), &[repo.id])
        .await?
        .remove(&repo.id)
        .unwrap_or_default();
    Ok((
        StatusCode::CREATED,
        Json(RepoResponse::new(&repo, &owner, Vec::new(), None, forks)),
    ))
}

fn selected_organization(raw: &str) -> Result<Option<i64>, &'static str> {
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > 19 || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Choose a valid repository owner.");
    }
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .map(Some)
        .ok_or("Choose a valid repository owner.")
}

fn choice_page(query: &HashMap<String, String>) -> Result<(i64, i64), &'static str> {
    let parse = |key: &str, default: i64| match query.get(key) {
        None => Ok(default),
        Some(raw)
            if !raw.is_empty()
                && raw.len() <= 19
                && raw.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            raw.parse::<i64>()
                .map_err(|_| "Invalid organization choices page.")
        }
        _ => Err("Invalid organization choices page."),
    };
    let after = parse("org_after", 0)?;
    let limit = parse("org_limit", 50)?;
    OrganizationPage::new(after, limit).map_err(|_| "Invalid organization choices page.")?;
    Ok((after, limit))
}

// Retain only nonsecret bounded drafts, including malformed selectors. Validate the
// ORIGINAL input, not a truncated value that might accidentally become acceptable.
fn draft_value(form: &HashMap<String, String>, key: &str, max: usize) -> String {
    let raw = form.get(key).map(String::as_str).unwrap_or_default();
    let mut end = raw.len().min(max);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_owned()
}

/// A refused submission: the field to mark, if one field is at fault, and the message.
type CreationRefusal = (Option<&'static str>, &'static str);

/// The template a select submitted: none for an empty value, otherwise a listed template.
fn template_choice(
    raw: &str,
    find: fn(&str) -> Option<&'static Template>,
    message: &'static str,
) -> Result<Option<&'static Template>, CreationRefusal> {
    if raw.is_empty() {
        return Ok(None);
    }
    find(raw).map(Some).ok_or((None, message))
}

fn source_name(raw: &str) -> Result<Option<(&str, &str)>, CreationRefusal> {
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.split_once('/') {
        Some((owner, name))
            if raw.len() <= 2048
                && !raw.chars().any(|ch| ch.is_control() || ch.is_whitespace())
                && !owner.is_empty()
                && !name.is_empty()
                && !name.contains('/') =>
        {
            Ok(Some((owner, name)))
        }
        _ => Err((Some("source"), "Use owner/repository for the source.")),
    }
}

fn decode_creation(
    form: &HashMap<String, String>,
) -> (
    NewRepositoryForm,
    Result<(Option<i64>, InitialFiles), CreationRefusal>,
) {
    let mut values = NewRepositoryForm {
        name: draft_value(form, "name", 256),
        description: draft_value(form, "description", 4096),
        is_private: form
            .get("visibility")
            .is_some_and(|value| value == "private"),
        owner_org_id: draft_value(form, "owner_org_id", 32),
        readme: form.contains_key("readme"),
        gitignore: draft_value(form, "gitignore", 64),
        license: draft_value(form, "license", 64),
        source: draft_value(form, "source", 2048),
        fork_source: form.contains_key("fork_source"),
        all_branches: form.contains_key("all_branches"),
        ..Default::default()
    };
    let metadata_validity = crate::collection::properties::drafts(form, &mut values);
    let validity = (|| {
        metadata_validity?;
        let target =
            selected_organization(form.get("owner_org_id").map(String::as_str).unwrap_or(""))
                .map_err(|message| (None, message))?;
        let name = form.get("name").map(String::as_str).unwrap_or("");
        if name.is_empty() {
            return Err((Some("name"), "Repository name is required"));
        }
        codeyard_app::repositories::lifecycle::validate_name(name)
            .map_err(|error| (Some("name"), error.message))?;
        codeyard_app::repositories::lifecycle::validate_description(
            form.get("description").map(String::as_str).unwrap_or(""),
        )
        .map_err(|error| (None, error.message))?;
        if !matches!(
            form.get("visibility").map(String::as_str),
            Some("public" | "private")
        ) {
            return Err((None, "Choose public or private visibility."));
        }
        if source_name(form.get("source").map(String::as_str).unwrap_or(""))?.is_some() {
            return Ok((target, InitialFiles::default()));
        }
        let files = InitialFiles {
            readme: values.readme,
            gitignore: template_choice(
                form.get("gitignore").map(String::as_str).unwrap_or(""),
                codeyard_app::repositories::templates::gitignore,
                "Choose a .gitignore template from the list.",
            )?,
            license: template_choice(
                form.get("license").map(String::as_str).unwrap_or(""),
                codeyard_app::repositories::templates::license,
                "Choose a license from the list.",
            )?,
        };
        Ok((target, files))
    })();
    (values, validity)
}

pub(super) async fn load_choices(
    ctx: &PageContext,
    actor: i64,
    after: i64,
    limit: i64,
) -> Result<RepositoryCreationChoices, StatusCode> {
    let page = OrganizationPage::new(after, limit).map_err(|_| StatusCode::BAD_REQUEST)?;
    match codeyard_db_orgs::organizations::profiles::list_repository_creation_organizations(
        &ctx.state.pool,
        actor,
        page,
    )
    .await
    {
        Ok(OrganizationOutcome::Applied(rows)) => {
            let next_after = if rows.len() == limit as usize {
                rows.last().map(|org| org.id)
            } else {
                None
            };
            Ok(RepositoryCreationChoices {
                organizations: rows
                    .into_iter()
                    .map(|org| RepositoryCreationOrganization {
                        id: org.id,
                        slug: org.name,
                    })
                    .collect(),
                after,
                limit,
                next_after,
            })
        }
        Ok(OrganizationOutcome::Denied) => Err(StatusCode::FORBIDDEN),
        _ => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

pub(crate) async fn new_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let (after, limit) = match choice_page(&ctx.query) {
        Ok(page) => page,
        Err(message) => return ctx.error(StatusCode::BAD_REQUEST, message),
    };
    let raw = ctx
        .query
        .get("owner_org_id")
        .map(String::as_str)
        .unwrap_or("");
    let target = match selected_organization(raw) {
        Ok(target) => target,
        Err(message) => return ctx.error(StatusCode::BAD_REQUEST, message),
    };
    let choices = match load_choices(&ctx, user.id, after, limit).await {
        Ok(choices) => choices,
        Err(StatusCode::FORBIDDEN) => {
            return ctx.error(
                StatusCode::FORBIDDEN,
                "This account cannot create repositories.",
            )
        }
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Repository owner choices are temporarily unavailable.",
            )
        }
    };
    // No unscoped selected-target read. Paged-away/forbidden/missing targets all
    // receive the same honest unavailable option, without a name or permission claim.
    let mut form = NewRepositoryForm {
        owner_org_id: target.map(|id| id.to_string()).unwrap_or_default(),
        source: draft_value(&ctx.query, "source", 2048),
        fork_source: ctx
            .query
            .get("fork_source")
            .is_some_and(|value| value == "true"),
        ..Default::default()
    };
    if let Some(org) =
        target.filter(|id| choices.organizations.iter().any(|choice| choice.id == *id))
    {
        match codeyard_db_orgs::organizations::settings::get(&ctx.state.pool, org).await {
            Ok(Some(settings)) => {
                form.is_private = settings.members_can_create_private_repositories
                    && !settings.members_can_create_public_repositories
            }
            _ => {
                return ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Repository owner policies are temporarily unavailable.",
                )
            }
        }
    }
    if let Err(status) = crate::collection::properties::populate(&ctx, &choices, &mut form).await {
        return ctx.error(
            status,
            crate::collection::properties::failure_message(status),
        );
    }
    ctx.page_model(
        "Create a new repository",
        AppPage::Discovery(view::Page::NewRepository {
            owner: user.username.clone(),
            csrf_token: ctx.csrf_token.clone(),
            choices,
            form,
            error: None,
        }),
    )
}

async fn creation_error(
    ctx: &PageContext,
    mut form: NewRepositoryForm,
    page: (i64, i64),
    status: StatusCode,
    field: Option<&'static str>,
    message: &str,
) -> Response {
    let Some(user) = &ctx.user else {
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "Log in before creating a repository.",
        );
    };
    let (status, message, choices) = match load_choices(ctx, user.id, page.0, page.1).await {
        Ok(choices) => (status, message, choices),
        Err(StatusCode::FORBIDDEN) => (StatusCode::FORBIDDEN,
            "This account cannot create repositories.", RepositoryCreationChoices::default()),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE,
            "Repository owner choices are temporarily unavailable. Your bounded draft is retained; reload before retrying.",
            RepositoryCreationChoices::default()),
    };
    if let Err(status) = crate::collection::properties::populate(ctx, &choices, &mut form).await {
        return ctx.error(
            status,
            crate::collection::properties::failure_message(status),
        );
    }
    ctx.page_model_status(
        status,
        "Create a new repository",
        AppPage::Discovery(view::Page::NewRepository {
            owner: user.username.clone(),
            csrf_token: ctx.csrf_token.clone(),
            choices,
            form,
            error: Some(view::FormError {
                field: field.map(str::to_owned),
                message: message.into(),
            }),
        }),
    )
}

pub(crate) async fn create_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let (mut values, validity) = decode_creation(&form);
    let page = match choice_page(&ctx.query) {
        Ok(page) => page,
        Err(message) => {
            return creation_error(
                &ctx,
                values,
                (0, 50),
                StatusCode::BAD_REQUEST,
                None,
                message,
            )
            .await
        }
    };
    if form
        .get("intent")
        .is_some_and(|intent| matches!(intent.as_str(), "page-owners" | "load-properties"))
    {
        let choices = match load_choices(&ctx, user.id, page.0, page.1).await {
            Ok(choices) => choices,
            Err(_) => {
                return creation_error(
                    &ctx,
                    values,
                    page,
                    StatusCode::SERVICE_UNAVAILABLE,
                    None,
                    "Repository owner choices are temporarily unavailable.",
                )
                .await
            }
        };
        if form.get("intent").map(String::as_str) == Some("load-properties")
            && selected_organization(form.get("owner_org_id").map(String::as_str).unwrap_or(""))
                .is_err()
        {
            return creation_error(
                &ctx,
                values,
                page,
                StatusCode::BAD_REQUEST,
                None,
                "Choose a valid repository owner.",
            )
            .await;
        }
        if let Err(status) =
            crate::collection::properties::populate(&ctx, &choices, &mut values).await
        {
            return ctx.error(
                status,
                crate::collection::properties::failure_message(status),
            );
        }
        return ctx.page_model(
            "Create a new repository",
            AppPage::Discovery(view::Page::NewRepository {
                owner: user.username.clone(),
                csrf_token: ctx.csrf_token.clone(),
                choices,
                form: values,
                error: None,
            }),
        );
    }
    let (target, files) = match validity {
        Ok(choice) => choice,
        Err((field, message)) => {
            return creation_error(&ctx, values, page, StatusCode::BAD_REQUEST, field, message)
                .await
        }
    };
    let property_values =
        match crate::collection::properties::values(&ctx, user.id, target, &values).await {
            Ok(values) => values,
            Err((status, message)) => {
                return creation_error(&ctx, values, page, status, None, message).await
            }
        };
    let authority = ctx.database_actor(user.id);
    let creation_owner = codeyard_db_repos::repositories::creation::RepositoryCreationOwner {
        actor_id: user.id,
        target: match target {
            Some(id) => {
                codeyard_db_repos::repositories::creation::RepositoryTarget::Organization(id)
            }
            None => codeyard_db_repos::repositories::creation::RepositoryTarget::Personal(user.id),
        },
    };
    let result = if !values.source.is_empty() {
        create_selection(
            &ctx,
            creation_owner,
            &values,
            files,
            &property_values,
            &authority,
        )
        .await
    } else {
        codeyard_app::repositories::lifecycle::provision_with_initial_properties(
            (&ctx.state.pool, ctx.state.cluster.as_ref()),
            &ctx.state.repos_path,
            codeyard_app::repositories::lifecycle::CreationInput {
                owner: creation_owner,
                properties: &property_values,
                admission: codeyard_db_repos::repositories::creation::CreationAdmission {
                    authority: Some(&authority),
                    advisory: None,
                },
            },
            &values.name,
            Some(&values.description)
                .filter(|value| !value.is_empty())
                .map(String::as_str),
            values.is_private,
            files,
        )
        .await
    };
    match result {
        Ok(repo) => {
            match codeyard_db_repos::repositories::records::repository_owner_name(&ctx.state.pool, repo.id).await {
                Ok(Some(owner)) => ctx.redirect(&view::repository_url(&owner, &repo.name)),
                _ => creation_error(&ctx, values, page, StatusCode::SERVICE_UNAVAILABLE, None, "The repository was created, but its owner could not be loaded. Check your repositories before retrying.").await,
            }
        }
        Err(error) => {
            let status = crate::responses::lifecycle_status(&error);

            // No missing-namespace oracle and never display raw API/driver errors.
            let status = if status == StatusCode::NOT_FOUND {
                StatusCode::FORBIDDEN
            } else {
                status
            };
            let message = match status {
                StatusCode::CONFLICT => "That owner already has a repository with this name, or the name is reserved. Choose another name.",
                StatusCode::BAD_REQUEST => error.message,
                StatusCode::UNAUTHORIZED => "Your session expired. Log in before creating a repository.",
                StatusCode::FORBIDDEN => "You do not have permission to create this repository for the selected owner.",
                _ => "Repository creation could not be completed. Check your repositories before retrying; contact an administrator if the problem persists.",
            };
            creation_error(&ctx, values, page, status, None, message).await
        }
    }
}

async fn create_selection(
    ctx: &PageContext,
    owner: codeyard_db_repos::repositories::creation::RepositoryCreationOwner,
    values: &NewRepositoryForm,
    files: InitialFiles,
    properties: &[codeyard_db_core::queries::organizations::properties::ValueInput],
    authority: &codeyard_db_core::queries::credentials::Actor,
) -> Result<codeyard_db_core::models::Repository, codeyard_app::repositories::lifecycle::StorageError>
{
    let description = Some(values.description.as_str()).filter(|value| !value.is_empty());
    let source = source_name(&values.source).map_err(|(_, message)| {
        codeyard_app::repositories::lifecycle::StorageError {
            kind: codeyard_app::repositories::lifecycle::LifecycleFailure::Invalid,
            message,
        }
    })?;
    if let Some((source_owner, source_name)) = source {
        let source = codeyard_db_repos::repositories::records::get_repository(
            &ctx.state.pool,
            source_owner,
            source_name,
        )
        .await
        .map_err(|_| codeyard_app::repositories::lifecycle::StorageError {
            kind: codeyard_app::repositories::lifecycle::LifecycleFailure::Unavailable,
            message: "Source repository is unavailable.",
        })?
        .ok_or(codeyard_app::repositories::lifecycle::StorageError {
            kind: codeyard_app::repositories::lifecycle::LifecycleFailure::Denied,
            message: "Source repository is unavailable.",
        })?;
        if values.fork_source {
            codeyard_app::repositories::lifecycle::fork_into(
                &ctx.state.pool,
                ctx.state.cluster.as_ref(),
                &ctx.state.repos_path,
                &ctx.state.notification_broadcaster,
                codeyard_app::repositories::lifecycle::ForkOptions {
                    owner,
                    name: Some(&values.name),
                    default_branch_only: !values.all_branches,
                    properties,
                    authority: Some(authority),
                },
                source.id,
                source_owner,
            )
            .await
        } else {
            codeyard_app::repositories::lifecycle::generate(
                (&ctx.state.pool, ctx.state.cluster.as_ref()),
                &ctx.state.repos_path,
                owner,
                source.id,
                codeyard_app::repositories::lifecycle::TemplateOptions {
                    name: &values.name,
                    description,
                    private: values.is_private,
                    all_branches: values.all_branches,
                    properties,
                    authority: Some(authority),
                },
            )
            .await
        }
    } else {
        codeyard_app::repositories::lifecycle::provision_with_initial_properties(
            (&ctx.state.pool, ctx.state.cluster.as_ref()),
            &ctx.state.repos_path,
            codeyard_app::repositories::lifecycle::CreationInput {
                owner,
                properties,
                admission: codeyard_db_repos::repositories::creation::CreationAdmission {
                    authority: Some(authority),
                    advisory: None,
                },
            },
            &values.name,
            description,
            values.is_private,
            files,
        )
        .await
    }
}

#[cfg(test)]
mod creation_tests {
    use super::*;
    fn input(values: &[(&str, &str)]) -> HashMap<String, String> {
        values
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }
    #[test]
    fn names_and_account_paths_cannot_traverse_directories() {
        for invalid in [
            "",
            ".",
            "..",
            "../repo",
            "/absolute",
            "a/b",
            "a\\b",
            "a%2Fb",
            "a\0b",
            "white space",
            "x?y",
            "x#y",
        ] {
            assert!(
                codeyard_app::repositories::lifecycle::validate_name(invalid).is_err(),
                "{invalid:?}"
            );
        }
        for valid in ["alice", "my-repo", "a.b_c-1", "répo"] {
            assert!(codeyard_app::repositories::lifecycle::validate_name(valid).is_ok());
        }
    }
    #[test]
    fn selectors_never_accept_arbitrary_personal_owners_or_malformed_ids() {
        assert_eq!(selected_organization(""), Ok(None));
        assert_eq!(selected_organization("12"), Ok(Some(12)));
        for raw in [
            "-1",
            "0",
            "+1",
            " 1",
            "1 ",
            "1.0",
            "user:1",
            "org:1",
            "9223372036854775808",
        ] {
            assert!(selected_organization(raw).is_err(), "{raw}");
        }
        let (_, result) = decode_creation(&input(&[
            ("name", "demo"),
            ("visibility", "public"),
            ("owner", "other"),
            ("owner_user_id", "999"),
        ]));
        assert_eq!(result.map(|(target, _)| target), Ok(None));
    }
    #[test]
    fn an_empty_name_is_refused_as_a_name_field_error() {
        let (_, validity) = decode_creation(&input(&[("name", ""), ("visibility", "public")]));
        assert_eq!(validity, Err((Some("name"), "Repository name is required")));
        let (_, validity) =
            decode_creation(&input(&[("name", "bad name"), ("visibility", "public")]));
        let Err((field, message)) = validity else {
            panic!("a name with a space must be refused");
        };
        assert_eq!(field, Some("name"));
        assert!(message.starts_with("Repository name must be"));
        let (_, validity) = decode_creation(&input(&[("name", "demo"), ("visibility", "maybe")]));
        assert_eq!(
            validity,
            Err((None, "Choose public or private visibility."))
        );
    }
    #[test]
    fn initial_files_must_be_listed_templates_or_none() {
        let (_, validity) = decode_creation(&input(&[
            ("name", "demo"),
            ("visibility", "public"),
            ("readme", "on"),
            ("gitignore", "Rust"),
            ("license", "MIT"),
        ]));
        let Ok((None, files)) = validity else {
            panic!("listed templates must be accepted");
        };
        assert!(files.readme);
        assert_eq!(files.gitignore.map(|template| template.name), Some("Rust"));
        assert_eq!(files.license.map(|template| template.name), Some("MIT"));
        let (_, validity) = decode_creation(&input(&[
            ("name", "demo"),
            ("visibility", "public"),
            ("gitignore", "Haskell"),
        ]));
        assert_eq!(
            validity,
            Err((None, "Choose a .gitignore template from the list."))
        );
        let (_, validity) = decode_creation(&input(&[
            ("name", "demo"),
            ("visibility", "public"),
            ("license", "WTFPL"),
        ]));
        assert_eq!(validity, Err((None, "Choose a license from the list.")));
    }

    #[test]
    fn validation_preserves_nonsecret_bounded_drafts_without_accepting_truncation() {
        let (values, validity) = decode_creation(&input(&[
            ("name", "../bad"),
            ("description", "<script>draft</script>"),
            ("visibility", "private"),
            ("owner_org_id", "-1"),
        ]));
        assert!(validity.is_err());
        assert_eq!(values.name, "../bad");
        assert_eq!(values.description, "<script>draft</script>");
        assert!(values.is_private);
        assert_eq!(values.owner_org_id, "-1");
        for (key, value) in [
            ("name", "a".repeat(101)),
            ("description", "a".repeat(2001)),
            ("visibility", "internal".into()),
        ] {
            let mut form = input(&[("name", "demo"), ("visibility", "public")]);
            form.insert(key.into(), value);
            assert!(decode_creation(&form).1.is_err());
        }
        let (draft, validity) = decode_creation(&input(&[
            ("name", &"é".repeat(1000)),
            ("description", &"é".repeat(4000)),
            ("visibility", "private"),
        ]));
        assert!(validity.is_err());
        assert_eq!(draft.name.len(), 256);
        assert_eq!(draft.description.len(), 4096);
    }
    #[test]
    fn cursor_only_paginates_bounded_organization_choices() {
        assert_eq!(choice_page(&HashMap::new()), Ok((0, 50)));
        assert_eq!(
            choice_page(&input(&[("org_after", "12"), ("org_limit", "1")])),
            Ok((12, 1))
        );
        for values in [
            [("org_after", "-1")],
            [("org_limit", "0")],
            [("org_limit", "101")],
            [("org_after", "abc")],
        ] {
            assert!(choice_page(&input(&values)).is_err());
        }
    }
}
