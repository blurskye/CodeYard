//! Native migration forms share import admission and storage publication with REST.
use super::{chrome::load_database, collection::load_choices};
use crate::backend::{
    api::{
        http::{context::PageContext, session},
        user::responses::field,
    },
    app::AppState,
    db,
    repositories::{
        imports::{self, ImportCredential},
        lifecycle,
        store::repo_store,
    },
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    Form,
};
use codeyard_webui::{
    app::Page,
    pages::discovery::{self as view, FormError, MigrationForm, RepositoryCreationChoices},
};
use std::{collections::HashMap, sync::Arc};

fn source_credentials(form: &HashMap<String, String>) -> Option<imports::Credentials> {
    let username = field(form, "source_username");
    let password = field(form, "source_token");
    if username.is_empty() && password.is_empty() {
        return None;
    }
    Some(imports::Credentials {
        username: if username.is_empty() { "git" } else { username }.to_owned(),
        password: password.to_owned(),
    })
}

async fn render(
    ctx: &PageContext,
    form: MigrationForm,
    error: Option<(StatusCode, &str)>,
) -> Response {
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let choices = if form.repository_id.is_some() {
        RepositoryCreationChoices::default()
    } else {
        match load_choices(ctx, user.id, 0, 100).await {
            Ok(choices) => choices,
            Err(_) => {
                return ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Repository owner choices unavailable.",
                )
            }
        }
    };
    ctx.page_model_status(
        error.map_or(StatusCode::OK, |(status, _)| status),
        "New migration",
        Page::Discovery(view::Page::Migration {
            owner: user.username.clone(),
            csrf_token: ctx.csrf_token.clone(),
            choices,
            form,
            error: error.map(|(_, message)| FormError {
                field: None,
                message: message.to_owned(),
            }),
        }),
    )
}

pub(super) async fn new(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    render(
        &ctx,
        MigrationForm {
            is_private: true,
            action: "/new/migration".to_owned(),
            ..Default::default()
        },
        None,
    )
    .await
}

pub(super) async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(input): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&input) {
        return response;
    }
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let source = imports::source_url(field(&input, "source_url"));
    let form = MigrationForm {
        name: field(&input, "name").chars().take(100).collect(),
        source_url: source
            .as_ref()
            .map(|url| url.to_string())
            .unwrap_or_default(),
        is_private: input.contains_key("private"),
        owner_org_id: field(&input, "owner_org_id").chars().take(20).collect(),
        action: "/new/migration".to_owned(),
        ..Default::default()
    };
    if let Err(error) = lifecycle::validate_name(field(&input, "name")) {
        return render(
            &ctx,
            form,
            Some((StatusCode::UNPROCESSABLE_ENTITY, error.message)),
        )
        .await;
    }
    let source = match source {
        Ok(url) => url,
        Err(_) => {
            return render(
                &ctx,
                form,
                Some((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Enter an HTTPS clone URL without credentials, a query or a fragment.",
                )),
            )
            .await
        }
    };
    let credentials = source_credentials(&input);
    if credentials
        .as_ref()
        .is_some_and(|credentials| !credentials.valid())
    {
        return render(
            &ctx,
            form,
            Some((
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid source credentials.",
            )),
        )
        .await;
    }
    let target = match field(&input, "owner_org_id") {
        "" => db::repositories::creation::RepositoryTarget::Personal(user.id),
        value => match value.parse::<i64>().ok().filter(|id| *id > 0) {
            Some(id) => db::repositories::creation::RepositoryTarget::Organization(id),
            None => {
                return render(
                    &ctx,
                    form,
                    Some((
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Choose a valid repository owner.",
                    )),
                )
                .await
            }
        },
    };
    let repo = match lifecycle::provision(
        (&ctx.state.pool, ctx.state.cluster.as_ref()),
        &ctx.state.repos_path,
        db::repositories::creation::RepositoryCreationOwner {
            actor_id: user.id,
            target,
        },
        field(&input, "name"),
        None,
        form.is_private,
        None,
    )
    .await
    {
        Ok(repo) => repo,
        Err(error) => {
            return render(
                &ctx,
                form,
                Some((super::responses::lifecycle_status(&error), error.message)),
            )
            .await
        }
    };
    let owner =
        match db::repositories::records::repository_owner_name(&ctx.state.pool, repo.id).await {
            Ok(Some(owner)) => owner,
            _ => {
                return ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Repository created, but its owner could not be loaded.",
                )
            }
        };
    let base = view::repository_url(&owner, &repo.name);
    let credential = match session::extract_session_id(&ctx.headers) {
        Some(id) => ImportCredential::Session(id),
        None => {
            return ctx.error(
                StatusCode::UNAUTHORIZED,
                "Repository created; sign in again to start its migration.",
            )
        }
    };
    match imports::start(
        ctx.state.clone(),
        repo,
        credential,
        source.as_str(),
        credentials,
    )
    .await
    {
        Ok(_) => ctx.redirect(&base),
        Err(_) => ctx.redirect(&format!("{base}/migration?error=start")),
    }
}

pub(super) async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.require_user() {
        return response;
    }
    let loaded = match load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if !loaded.chrome.can_admin {
        return ctx.error(StatusCode::FORBIDDEN, "Repository administration required.");
    }
    let import = match db::source_imports::get(&ctx.state.pool, loaded.repository.id).await {
        Ok(import) => import,
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Import status unavailable.",
            )
        }
    };
    let form = MigrationForm {
        repository_id: Some(loaded.repository.id),
        name: name.clone(),
        source_url: import
            .as_ref()
            .map(|import| import.vcs_url.clone())
            .unwrap_or_default(),
        status: import.as_ref().map(|import| import.state().to_owned()),
        action: format!("{}/migration", view::repository_url(&owner, &name)),
        ..Default::default()
    };
    let error = ctx.query.contains_key("error").then_some((
        StatusCode::SERVICE_UNAVAILABLE,
        "Repository created, but the import could not start. Retry here.",
    ));
    render(&ctx, form, error).await
}

pub(super) async fn submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
    Form(input): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&input) {
        return response;
    }
    let user = match ctx.require_user() {
        Ok(user) => user,
        Err(response) => return response,
    };
    let loaded = match load_database(&ctx, &owner, &name).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    if !loaded.chrome.can_admin {
        return ctx.error(StatusCode::FORBIDDEN, "Repository administration required.");
    }
    if field(&input, "repository_id").parse::<i64>().ok() != Some(loaded.repository.id) {
        return ctx.error(
            StatusCode::CONFLICT,
            "Repository changed; reload the migration page.",
        );
    }
    let base = view::repository_url(&owner, &name);
    if field(&input, "intent") == "cancel" {
        return match repo_store(&loaded.repository, ctx.state.cluster.as_ref())
            .cancel_import_as(&ctx.state.pool, loaded.repository.id, user.id)
            .await
        {
            Ok(()) => ctx.redirect(&base),
            Err(_) => ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Import cancellation unavailable.",
            ),
        };
    }
    let credential = match session::extract_session_id(&ctx.headers) {
        Some(id) => ImportCredential::Session(id),
        None => return ctx.error(StatusCode::UNAUTHORIZED, "Sign in again."),
    };
    let credentials = source_credentials(&input);
    let result = imports::start(
        ctx.state.clone(),
        loaded.repository.clone(),
        credential,
        field(&input, "source_url"),
        credentials,
    )
    .await;
    match result {
        Ok(_) => ctx.redirect(&base),
        Err(error) => {
            let status = match error {
                imports::ImportError::Source | imports::ImportError::Credentials => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                imports::ImportError::Running | imports::ImportError::NotEmpty => {
                    StatusCode::CONFLICT
                }
                imports::ImportError::Denied => StatusCode::FORBIDDEN,
                imports::ImportError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            };
            let form = MigrationForm {
                repository_id: Some(loaded.repository.id),
                name,
                source_url: imports::source_url(field(&input, "source_url"))
                    .map(|url| url.to_string())
                    .unwrap_or_default(),
                action: format!("{base}/migration"),
                ..Default::default()
            };
            render(&ctx, form, Some((status, &error.to_string()))).await
        }
    }
}
