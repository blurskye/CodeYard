//! Native attestation browsing, bundle downloads and CSRF-protected deletion.
use axum::extract::DefaultBodyLimit;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::Form;
use axum::Json;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_db_security::attestations::Filter;
use codeyard_db_security::attestations::Owner;
use codeyard_db_security::attestations::Refusal;
use codeyard_webui::app::Page as AppPage;
use codeyard_webui::encode_component;
use codeyard_webui::pages::attestations as view;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;

pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/orgs/:org/attestations", get(show).post(remove))
        .cell_route("/users/:username/attestations", get(show).post(remove))
        .cell_route("/:owner/:repo/attestations", get(show).post(remove))
        .cell_route(
            "/:owner/:repo/attestations/:attestation_id/bundle",
            get(bundle),
        )
        .layer(DefaultBodyLimit::max(16 * 1024))
}

fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    let (status, message) = match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => (
            StatusCode::FORBIDDEN,
            "Attestation deletion is not permitted.",
        ),
        Some(Refusal::Missing) => (StatusCode::NOT_FOUND, "Attestation not found."),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Attestations are temporarily unavailable.",
        ),
    };
    ctx.error(status, message)
}

async fn model(ctx: &PageContext, params: &Params) -> Result<view::Page, Response> {
    let (org, repo, repo_chrome, title, base) = if let Some(name) = params.get("org") {
        let org = codeyard_db_orgs::organizations::profiles::get_public_organization(
            &ctx.state.pool,
            name,
        )
        .await
        .map_err(|error| failure(ctx, error))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Organization not found."))?;
        (
            Some(Owner::Organization(org.id)),
            None,
            None,
            org.name.clone(),
            format!("/orgs/{}/attestations", encode_component(&org.name)),
        )
    } else if let Some(username) = params.get("username") {
        let user =
            codeyard_db_identity::users::accounts::get_user_by_username(&ctx.state.pool, username)
                .await
                .map_err(|error| failure(ctx, error))?
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "User not found."))?;
        (
            Some(Owner::User(user.id)),
            None,
            None,
            user.username.clone(),
            format!("/users/{}/attestations", encode_component(&user.username)),
        )
    } else {
        let loaded = codeyard_api_http::shared::repos::chrome::load_database(
            ctx,
            &params["owner"],
            &params["repo"],
        )
        .await?;
        let title = format!("{}/{}", params["owner"], loaded.repository.name);
        let base = format!(
            "/{}/{}/attestations",
            encode_component(&params["owner"]),
            encode_component(&loaded.repository.name)
        );
        (
            None,
            Some(loaded.repository.id),
            Some(Box::new(loaded.chrome)),
            title,
            base,
        )
    };
    let digest = ctx.query.get("digest").cloned().unwrap_or_default();
    let predicate = ctx.query.get("predicate").cloned().unwrap_or_default();
    if !digest.is_empty() && !codeyard_db_core::queries::attestations::valid_digest(&digest) {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Enter a complete lowercase SHA-256 or SHA-512 subject digest.",
        ));
    }
    if !predicate.is_empty() && !codeyard_db_security::attestations::valid_predicate(&predicate) {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            "Predicate type is invalid or too long.",
        ));
    }
    let after = match ctx.query.get("after") {
        Some(value) => value
            .parse::<i64>()
            .ok()
            .filter(|id| *id >= 0)
            .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid attestation page."))?,
        None => 0,
    };
    let digests = vec![digest.clone()];
    let browser_session = codeyard_api_http::session::extract_session_id(&ctx.headers);
    let filter = Filter {
        actor: ctx.user.as_ref().map(|user| user.id),
        session: browser_session.as_deref(),
        owner: org,
        repo,
        digests: (!digest.is_empty()).then_some(digests.as_slice()),
        predicate: (!predicate.is_empty()).then_some(predicate.as_str()),
        after,
        before: i64::MAX,
        limit: 31,
    };
    let mut rows = codeyard_db_security::attestations::summaries(&ctx.state.pool, &filter)
        .await
        .map_err(|error| failure(ctx, error))?;
    let more = rows.len() > 30;
    rows.truncate(30);
    let mut entries = Vec::new();
    for row in &rows {
        let owner = &row.owner_name;
        let repository_href = format!(
            "/{}/{}",
            encode_component(owner),
            encode_component(&row.repository)
        );
        let mut subjects: Vec<String> =
            serde_json::from_str(&row.subjects).map_err(|error| failure(ctx, error.into()))?;
        let omitted_subjects = subjects.len().saturating_sub(5);
        subjects.truncate(5);
        entries.push(view::Entry {
            id: row.id,
            repository: format!("{owner}/{}", row.repository),
            repository_href: repository_href.clone(),
            bundle_href: format!("{repository_href}/attestations/{}/bundle", row.id),
            predicate: row.predicate_type.clone(),
            subjects,
            omitted_subjects,
            delete_action: row.can_delete.then(|| ctx.location.clone()),
            inventory_base: row
                .owner_org_id
                .map(|_| format!("/orgs/{}/artifacts", encode_component(owner))),
        });
    }
    let next = if more {
        let after = rows.last().map(|row| row.id).unwrap_or_default();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("digest", &digest)
            .append_pair("predicate", &predicate)
            .append_pair("after", &after.to_string())
            .finish();
        Some(format!("{base}?{query}"))
    } else {
        None
    };
    Ok(view::Page {
        title,
        base,
        repo: repo_chrome,
        digest,
        predicate,
        entries,
        next,
        csrf_token: ctx.csrf_token.clone(),
        message: None,
    })
}

async fn show(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<Params>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    match model(&ctx, &params).await {
        Ok(page) => ctx.page_model("Attestations", AppPage::Attestations(page)),
        Err(response) => response,
    }
}

async fn bundle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<Params>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match codeyard_api_http::shared::repos::chrome::load_database(
        &ctx,
        &params["owner"],
        &params["repo"],
    )
    .await
    {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let Some(id) = params["attestation_id"]
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0 && *id < i64::MAX)
    else {
        return ctx.error(StatusCode::NOT_FOUND, "Attestation not found.");
    };
    let browser_session = codeyard_api_http::session::extract_session_id(&ctx.headers);
    let filter = Filter {
        actor: ctx.user.as_ref().map(|user| user.id),
        session: browser_session.as_deref(),
        owner: None,
        repo: Some(loaded.repository.id),
        digests: None,
        predicate: None,
        after: id - 1,
        before: id + 1,
        limit: 1,
    };
    match codeyard_db_security::attestations::list(&ctx.state.pool, &filter).await {
        Ok(rows) => match rows
            .first()
            .and_then(|row| serde_json::from_str::<serde_json::Value>(&row.bundle).ok())
        {
            Some(value) => (
                [
                    (
                        "content-disposition",
                        format!("attachment; filename=\"attestation-{id}.json\""),
                    ),
                    ("cache-control", "private, no-store".to_owned()),
                ],
                Json(value),
            )
                .into_response(),
            None => ctx.error(StatusCode::NOT_FOUND, "Attestation not found."),
        },
        Err(error) => failure(&ctx, error),
    }
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(params): Path<Params>,
    Form(form): Form<Params>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let Some(id) = form
        .get("attestation_id")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|id| *id > 0)
    else {
        return ctx.error(StatusCode::BAD_REQUEST, "Choose an attestation.");
    };
    // Recheck the exact route scope, not just whether the actor can write some other repository.
    let org = if let Some(org) = params.get("org") {
        match codeyard_db_orgs::organizations::profiles::get_public_organization(
            &ctx.state.pool,
            org,
        )
        .await
        {
            Ok(Some(org)) => Owner::Organization(org.id),
            Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "Organization not found."),
            Err(error) => return failure(&ctx, error),
        }
    } else if let Some(username) = params.get("username") {
        match codeyard_db_identity::users::accounts::get_user_by_username(&ctx.state.pool, username)
            .await
        {
            Ok(Some(user)) => Owner::User(user.id),
            Ok(None) => return ctx.error(StatusCode::NOT_FOUND, "User not found."),
            Err(error) => return failure(&ctx, error),
        }
    } else {
        let loaded = match codeyard_api_http::shared::repos::chrome::load_database(
            &ctx,
            &params["owner"],
            &params["repo"],
        )
        .await
        {
            Ok(loaded) => loaded,
            Err(response) => return response,
        };
        let browser_session = codeyard_api_http::session::extract_session_id(&ctx.headers);
        let filter = Filter {
            actor: Some(actor),
            session: browser_session.as_deref(),
            owner: None,
            repo: Some(loaded.repository.id),
            digests: None,
            predicate: None,
            after: id.saturating_sub(1),
            before: id.saturating_add(1),
            limit: 1,
        };
        match codeyard_db_security::attestations::list(&ctx.state.pool, &filter).await {
            Ok(rows) if !rows.is_empty() => {}
            Ok(_) => return ctx.error(StatusCode::NOT_FOUND, "Attestation not found."),
            Err(error) => return failure(&ctx, error),
        }
        if let Some(org) = loaded.repository.owner_org_id {
            Owner::Organization(org)
        } else if let Some(user) = loaded.repository.owner_user_id {
            Owner::User(user)
        } else {
            return ctx.error(StatusCode::NOT_FOUND, "Repository owner not found.");
        }
    };
    match codeyard_db_security::attestations::delete(
        &ctx.state.pool,
        ctx.database_actor(actor),
        org,
        &[id],
        &[],
    )
    .await
    {
        Ok(0) => ctx.error(StatusCode::NOT_FOUND, "Attestation not found."),
        Ok(_) => ctx.redirect(&ctx.location),
        Err(error) => failure(&ctx, error),
    }
}
