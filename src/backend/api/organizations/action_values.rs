use super::organization::{self, field};
use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    db::{
        self,
        organization_values::{self as values, Kind, Save, ValueError},
    },
};
use crate::webui::pages::orgs::actions as view;
use axum::{
    extract::{DefaultBodyLimit, Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::post,
    Router,
};
use std::{collections::HashMap, sync::Arc};

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
        &ctx.state.pool, actor, ctx.session_id.as_deref().ok_or(ValueError::Denied)?, org, page as i64,
    ).await?;
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
    let oidc = db::oidc::settings(&ctx.state.pool, actor, db::oidc::Scope::Organization(org))
        .await
        .map_err(ValueError::Authority)?;
    let inclusions = db::oidc::inclusions(&ctx.state.pool, actor, org)
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
        return if ids.trim().is_empty() {
            Ok(None)
        } else {
            Err(ValueError::Visibility)
        };
    }
    ids.split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .map(|id| id.parse().map_err(|_| ValueError::Repository))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
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
    if action.starts_with("oidc-") {
        let result = match action.as_str() {
            "oidc-save" => {
                let mut settings =
                    db::oidc::Settings::from_keys(false, field(&form, "include_claim_keys"));
                if settings.include_claim_keys.is_none() {
                    settings.include_claim_keys = Some(vec!["repo".into(), "context".into()]);
                }
                db::oidc::save_settings(
                    &ctx.state.pool,
                    actor,
                    db::oidc::Scope::Organization(org),
                    &settings,
                )
                .await
            }
            "oidc-property-add" | "oidc-property-delete" => {
                db::oidc::change_inclusion(
                    &ctx.state.pool,
                    actor,
                    org,
                    field(&form, "custom_property_name"),
                    action == "oidc-property-delete",
                )
                .await
            }
            _ => return organization::not_found(&ctx),
        };
        return match result {
            Ok(()) => ctx.redirect(&format!("/settings/organizations/{org}?tab=actions")),
            Err(error) => {
                let status = match error.downcast_ref::<db::oidc::Refusal>() {
                    Some(db::oidc::Refusal::Denied) => StatusCode::FORBIDDEN,
                    Some(_) => StatusCode::UNPROCESSABLE_ENTITY,
                    None => StatusCode::SERVICE_UNAVAILABLE,
                };
                organization::respond(
                    &ctx,
                    Some(org),
                    status,
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
        _ => return organization::not_found(&ctx),
    };
    let name = field(&form, "name").to_ascii_uppercase();
    let previous = field(&form, "previous").to_ascii_uppercase();
    if action.ends_with("-delete") && field(&form, "confirm") != "yes" {
        return organization::respond(
            &ctx,
            Some(org),
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("Confirm deletion before submitting.".to_owned()),
            Default::default(),
        )
        .await;
    }
    let result = if action.ends_with("-delete") {
        values::delete(&ctx.state.pool, actor, org, kind, &name).await
    } else {
        match repositories(&form) {
            Err(error) => Err(error),
            Ok(repositories) => values::save(
                &ctx.state.pool,
                ctx.state.secret_encryptor.as_deref(),
                actor,
                org,
                &Save {
                    kind,
                    previous: (action == "variable-save").then_some(previous.as_str()),
                    name: &name,
                    value: (action != "secret-access").then(|| field(&form, "value")),
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
        Err(ValueError::Denied | ValueError::NotFound) => organization::not_found(&ctx),
        Err(error) => {
            let (status, message) = match error {
                ValueError::Database(_) | ValueError::Authority(_) | ValueError::Encryption(_) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Organization Actions settings are unavailable.".to_owned(),
                ),
                _ => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
            };
            // Never echo a secret into a refusal page or hydration payload.
            organization::respond(&ctx, Some(org), status, Some(message), Default::default()).await
        }
    }
}
