use axum::extract::DefaultBodyLimit;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::user::responses::field;
use codeyard_app::app::AppState;
use codeyard_db_orgs::organization_values as values;
use codeyard_db_orgs::organization_values::Kind;
use codeyard_db_orgs::organization_values::Save;
use codeyard_db_orgs::organization_values::ValueError;
use codeyard_webui::pages::orgs::actions as view;
use std::collections::HashMap;
use std::sync::Arc;

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/organizations/:id/actions/:action", post(change))
        .layer(DefaultBodyLimit::max(256 * 1024))
}

pub(super) async fn build(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> Result<view::Panel, ValueError> {
    let page = ctx
        .query
        .get("values_page")
        .and_then(|page| page.parse::<usize>().ok())
        .filter(|page| (1..=1000).contains(page))
        .unwrap_or(1);
    let listed = values::settings_page(
        &ctx.state.pool,
        actor,
        ctx.session_id.as_deref().ok_or(ValueError::Denied)?,
        org,
        page as i64,
    )
    .await?;
    let more = listed.more;
    let link = |page: usize| {
        let mut query: Vec<(String, String)> = ctx
            .query
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "values_page" | "saved"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        query.push(("values_page".into(), page.to_string()));
        query.sort();
        format!(
            "/settings/organizations/{org}?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query)
                .finish()
        )
    };
    let mut items = Vec::new();
    for entry in listed.values {
        let row = entry.value;
        items.push(view::Value {
            name: row.name,
            value: row.value,
            secret: row.kind == Kind::Secret.as_str(),
            visibility: row.visibility,
            repositories: entry.repository_ids,
        });
    }
    let oidc = codeyard_db_actions::oidc::settings(
        &ctx.state.pool,
        actor,
        codeyard_db_core::queries::oidc::Scope::Organization(org),
    )
    .await
    .map_err(ValueError::Authority)?;
    let inclusions = codeyard_db_actions::oidc::inclusions(&ctx.state.pool, actor, org)
        .await
        .map_err(ValueError::Authority)?;
    Ok(view::Panel {
        oidc_claim_keys: oidc
            .include_claim_keys
            .unwrap_or_else(|| vec!["repo".into(), "context".into()])
            .join(", "),
        oidc_properties: inclusions
            .into_iter()
            .map(|p| p.custom_property_name)
            .collect(),
        csrf_token: ctx.csrf_token.clone(),
        org_id: org,
        encryption_enabled: ctx.state.secret_encryptor.is_some(),
        values: items,
        previous: (page > 1).then(|| link(page - 1)),
        next: more.then(|| link(page + 1)),
    })
}
fn repositories(form: &HashMap<String, String>) -> Result<Option<Vec<i64>>, ValueError> {
    let ids = field(form, "repositories");
    if field(form, "visibility") != "selected" {
        return Ok(None);
    }
    ids.split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .map(|id| id.parse().map_err(|_| ValueError::Repository))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}
/// The rejected variable form, so its input comes back. Secret forms are never drafted.
fn variable_draft(action: &str, form: &HashMap<String, String>) -> HashMap<String, String> {
    let mut draft: HashMap<String, String> = [
        ("name", 1024),
        ("previous", 1024),
        ("value", 49_152),
        ("visibility", 32),
        ("repositories", 8192),
    ]
    .into_iter()
    .filter_map(|(key, limit)| {
        form.get(key)
            .filter(|value| value.len() <= limit)
            .map(|value| (key.to_owned(), value.clone()))
    })
    .collect();
    draft.insert("action".to_owned(), action.to_owned());
    draft
}
async fn change(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((org, action)): Path<(i64, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    ctx.query.insert("tab".to_owned(), "actions".to_owned());
    let actor = match ctx.require_user() {
        Ok(actor) => actor.id,
        Err(response) => return response,
    };
    if field(&form, "org_id").parse::<i64>().ok() != Some(org) {
        return ctx.error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Organization does not match this form.",
        );
    }
    let credential = codeyard_db_core::queries::users::accounts::AccountCredential::Session(
        ctx.session_id.as_deref().unwrap_or(""),
    );
    if action.starts_with("oidc-") {
        let result = match action.as_str() {
            "oidc-save" => {
                let mut settings = codeyard_db_core::queries::oidc::Settings::from_keys(
                    false,
                    field(&form, "include_claim_keys"),
                );
                if settings.include_claim_keys.is_none() {
                    settings.include_claim_keys = Some(vec!["repo".into(), "context".into()]);
                }
                codeyard_db_actions::oidc::save_settings(
                    &ctx.state.pool,
                    (actor, &credential),
                    codeyard_db_core::queries::oidc::Scope::Organization(org),
                    &settings,
                )
                .await
            }
            "oidc-property-add" | "oidc-property-delete" => {
                codeyard_db_actions::oidc::change_inclusion(
                    &ctx.state.pool,
                    (actor, &credential),
                    org,
                    field(&form, "custom_property_name"),
                    action == "oidc-property-delete",
                )
                .await
            }
            _ => return crate::organizations::organization::not_found(&ctx),
        };
        return match result {
            Ok(()) => ctx.redirect(&format!("/settings/organizations/{org}?tab=actions")),
            Err(error) => {
                crate::organizations::organization::respond(
                    &ctx,
                    Some(org),
                    codeyard_api_http::error::oidc_settings_status(&error),
                    Some("Could not save OIDC settings.".into()),
                    Default::default(),
                )
                .await
            }
        };
    }
    let kind = match action.as_str() {
        "secret-save" | "secret-access" | "secret-delete" => Kind::Secret,
        "variable-create" | "variable-save" | "variable-delete" => Kind::Variable,
        _ => return crate::organizations::organization::not_found(&ctx),
    };
    let name = field(&form, "name").to_ascii_uppercase();
    let previous = field(&form, "previous").to_ascii_uppercase();
    if action.ends_with("-delete") && field(&form, "confirm") != "yes" {
        return crate::organizations::organization::respond(
            &ctx,
            Some(org),
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("Confirm deletion before submitting.".to_owned()),
            Default::default(),
        )
        .await;
    }
    let submitted_value = if action == "secret-access" {
        None
    } else {
        let original = if action == "variable-save" {
            match values::list(&ctx.state.pool, actor, org, kind).await {
                Ok(rows) => match rows.into_iter().find(|row| row.name == previous) {
                    Some(row) => row.value,
                    None => return crate::organizations::organization::not_found(&ctx),
                },
                Err(_) => return crate::organizations::organization::unavailable(&ctx),
            }
        } else {
            String::new()
        };
        Some(codeyard_app::repositories::files::native_text(
            field(&form, "value").to_owned(),
            &original,
        ))
    };
    let result = if action.ends_with("-delete") {
        values::delete(&ctx.state.pool, actor, &credential, org, kind, &name).await
    } else {
        match repositories(&form) {
            Err(error) => Err(error),
            Ok(repositories) => values::save(
                &ctx.state.pool,
                ctx.state.secret_encryptor.as_deref(),
                actor,
                &credential,
                org,
                &Save {
                    kind,
                    previous: (action == "variable-save").then_some(previous.as_str()),
                    name: &name,
                    value: submitted_value.as_deref(),
                    visibility: Some(field(&form, "visibility")),
                    repositories: repositories.as_deref(),
                },
            )
            .await
            .map(|_| ()),
        }
    };
    match result {
        Ok(()) => ctx.redirect(&format!("/settings/organizations/{org}?tab=actions")),
        Err(ValueError::Denied | ValueError::NotFound) => {
            crate::organizations::organization::not_found(&ctx)
        }
        Err(error) => {
            let (status, message) = match error {
                ValueError::Database(_) | ValueError::Authority(_) | ValueError::Encryption(_) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Organization Actions settings are unavailable.".to_owned(),
                ),
                _ => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
            };
            // Never echo a secret into a refusal page or hydration payload.
            let draft = if matches!(action.as_str(), "variable-create" | "variable-save") {
                variable_draft(&action, &form)
            } else {
                HashMap::new()
            };
            crate::organizations::organization::respond(
                &ctx,
                Some(org),
                status,
                Some(message),
                draft,
            )
            .await
        }
    }
}
