use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::get_current_user;
use crate::backend::api::repos::access::access_error;
use crate::backend::api::repos::paging::{json_page, PageQuery};
use crate::backend::api::repos::responses::{repository_owners, repository_topics, RepoResponse};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::db::organizations::{self as orgs, OrganizationOutcome, OrganizationPage};
use crate::backend::db::repositories::records::{RepositoryFilter, RepositorySort};
use crate::backend::repositories::{
    access::AccessError,
    lifecycle::{self, InitialFiles},
    templates::{self, Template},
};
use crate::webui::app::Page as AppPage;
use crate::webui::pages::discovery::{
    self as view, NewRepositoryForm, RepositoryCreationChoices, RepositoryCreationOrganization,
};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::{Form, Json};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
pub async fn list_repos(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Vec<RepoResponse>>, (StatusCode, Json<ApiError>)> {
    let (offset, limit) = json_page(query.page, Some(query.per_page.unwrap_or(100)))?;
    let repos = db::repositories::records::search_public_repositories(
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
    let mut responses = Vec::with_capacity(repos.len());
    for repository in repos {
        let owner = owners.remove(&repository.id).expect("validated owner");
        let topics = topics.remove(&repository.id).unwrap_or_default();
        responses.push(RepoResponse::new(&repository, &owner, topics, None));
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
}

pub async fn create_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateRepoRequest>,
) -> Result<(StatusCode, Json<RepoResponse>), (StatusCode, Json<ApiError>)> {
    let pool = state.pool.clone();
    let user = get_current_user(&pool, &headers).await?;

    let creation_owner = db::repositories::creation::RepositoryCreationOwner {
        actor_id: user.id,
        target: match req.owner_org_id {
            Some(id) if id > 0 => db::repositories::creation::RepositoryTarget::Organization(id),
            None => db::repositories::creation::RepositoryTarget::Personal(user.id),
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
    lifecycle::validate_description(req.description.as_deref().unwrap_or(""))
        .map_err(crate::backend::api::repos::responses::lifecycle_error)?;
    let repo = lifecycle::provision(
        (&pool, state.cluster.as_ref()),
        &state.repos_path,
        creation_owner,
        &req.name,
        req.description.as_deref(),
        req.is_private,
        None,
    )
    .await
    .map_err(crate::backend::api::repos::responses::lifecycle_error)?;

    let owner = db::repositories::records::repository_owner_name(&pool, repo.id)
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
    Ok((
        StatusCode::CREATED,
        Json(RepoResponse::new(&repo, &owner, Vec::new(), None)),
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

fn decode_creation(
    form: &HashMap<String, String>,
) -> (
    NewRepositoryForm,
    Result<(Option<i64>, InitialFiles), CreationRefusal>,
) {
    let values = NewRepositoryForm {
        name: draft_value(form, "name", 256),
        description: draft_value(form, "description", 4096),
        is_private: form
            .get("visibility")
            .is_some_and(|value| value == "private"),
        owner_org_id: draft_value(form, "owner_org_id", 32),
        readme: form.contains_key("readme"),
        gitignore: draft_value(form, "gitignore", 64),
        license: draft_value(form, "license", 64),
    };
    let validity = (|| {
        let target =
            selected_organization(form.get("owner_org_id").map(String::as_str).unwrap_or(""))
                .map_err(|message| (None, message))?;
        let name = form.get("name").map(String::as_str).unwrap_or("");
        if name.is_empty() {
            return Err((Some("name"), "Repository name is required"));
        }
        lifecycle::validate_name(name).map_err(|error| (Some("name"), error.message))?;
        lifecycle::validate_description(form.get("description").map(String::as_str).unwrap_or(""))
            .map_err(|error| (None, error.message))?;
        if !matches!(
            form.get("visibility").map(String::as_str),
            Some("public" | "private")
        ) {
            return Err((None, "Choose public or private visibility."));
        }
        let files = InitialFiles {
            readme: values.readme,
            gitignore: template_choice(
                form.get("gitignore").map(String::as_str).unwrap_or(""),
                templates::gitignore,
                "Choose a .gitignore template from the list.",
            )?,
            license: template_choice(
                form.get("license").map(String::as_str).unwrap_or(""),
                templates::license,
                "Choose a license from the list.",
            )?,
        };
        Ok((target, files))
    })();
    (values, validity)
}

async fn load_choices(
    ctx: &PageContext,
    actor: i64,
    after: i64,
    limit: i64,
) -> Result<RepositoryCreationChoices, StatusCode> {
    let page = OrganizationPage::new(after, limit).map_err(|_| StatusCode::BAD_REQUEST)?;
    match orgs::list_repository_creation_organizations(&ctx.state.pool, actor, page).await {
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
    ctx.page_model(
        "Create a new repository",
        AppPage::Discovery(view::Page::NewRepository {
            owner: user.username.clone(),
            csrf_token: ctx.csrf_token.clone(),
            choices,
            form: NewRepositoryForm {
                owner_org_id: target.map(|id| id.to_string()).unwrap_or_default(),
                ..Default::default()
            },
            error: None,
        }),
    )
}

async fn creation_error(
    ctx: &PageContext,
    form: NewRepositoryForm,
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
    let (values, validity) = decode_creation(&form);
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
        .is_some_and(|intent| intent == "page-owners")
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
    if target.is_none() && lifecycle::validate_name(&user.username).is_err() {
        return creation_error(
            &ctx,
            values,
            page,
            StatusCode::BAD_REQUEST,
            None,
            "This account's repository path is invalid. Contact an administrator.",
        )
        .await;
    }
    let creation_owner = db::repositories::creation::RepositoryCreationOwner {
        actor_id: user.id,
        target: match target {
            Some(id) => db::repositories::creation::RepositoryTarget::Organization(id),
            None => db::repositories::creation::RepositoryTarget::Personal(user.id),
        },
    };
    let result = lifecycle::provision_with_initial_files(
        (&ctx.state.pool, ctx.state.cluster.as_ref()),
        &ctx.state.repos_path,
        creation_owner,
        &values.name,
        Some(&values.description)
            .filter(|value| !value.is_empty())
            .map(String::as_str),
        values.is_private,
        files,
    )
    .await;
    match result {
        Ok(repo) => {
            match db::repositories::records::repository_owner_name(&ctx.state.pool, repo.id).await {
                Ok(Some(owner)) => ctx.redirect(&view::repository_url(&owner, &repo.name)),
                _ => creation_error(&ctx, values, page, StatusCode::SERVICE_UNAVAILABLE, None, "The repository was created, but its owner could not be loaded. Check your repositories before retrying.").await,
            }
        }
        Err(error) => {
            let status = crate::backend::api::repos::responses::lifecycle_status(&error);

            // No missing-namespace oracle and never display raw API/driver errors.
            let status = if status == StatusCode::NOT_FOUND {
                StatusCode::FORBIDDEN
            } else {
                status
            };
            let message = match status {
                StatusCode::CONFLICT => "That owner already has a repository with this name, or the name is reserved. Choose another name.",
                StatusCode::BAD_REQUEST => "The repository details are invalid. Please review them.",
                StatusCode::UNAUTHORIZED => "Your session expired. Log in before creating a repository.",
                StatusCode::FORBIDDEN => "You do not have permission to create this repository for the selected owner.",
                _ => "Repository creation could not be completed. Check your repositories before retrying; contact an administrator if the problem persists.",
            };
            creation_error(&ctx, values, page, status, None, message).await
        }
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
            assert!(lifecycle::validate_name(invalid).is_err(), "{invalid:?}");
        }
        for valid in ["alice", "my-repo", "a.b_c-1", "répo"] {
            assert!(lifecycle::validate_name(valid).is_ok());
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
