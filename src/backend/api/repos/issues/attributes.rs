use crate::backend::{
    api::{
        http::context::PageContext,
        repos::{
            discussion::{repository, unavailable, ResultPage},
            forms::{authorize_post, field},
        },
    },
    app::AppState,
    db::{
        self,
        issue_attributes::{Attribute, Definition, OptionInput, Refusal, ValueInput},
    },
};
use crate::webui::pages::repos::issues::{IssueAttribute, IssueAttributes, IssueSelectOption};
use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
fn view(a: Attribute, value: Option<Value>) -> IssueAttribute {
    IssueAttribute {
        revision: db::issue_attributes::revision(&a).unwrap_or_default(),
        id: a.id,
        kind: a.kind,
        name: a.name,
        description: a.description,
        data_type: a.data_type.unwrap_or_default(),
        visibility: a.visibility,
        is_enabled: a.is_enabled,
        color: a.color,
        options: a
            .options
            .into_iter()
            .map(|o| IssueSelectOption {
                id: o.id,
                name: o.name,
                color: o.color,
                description: o.description,
                priority: o.priority,
            })
            .collect(),
        value,
    }
}
pub(crate) async fn load(
    ctx: &PageContext,
    repo: i64,
    issue: i64,
    organization: bool,
) -> ResultPage<Box<IssueAttributes>> {
    let actor = ctx.user.as_ref().map(|u| u.id);
    let types = db::issue_attributes::catalog(&ctx.state.pool, repo, actor, "type")
        .await
        .map_err(|_| unavailable(ctx))?;
    let fields = db::issue_attributes::catalog(&ctx.state.pool, repo, actor, "field")
        .await
        .map_err(|_| unavailable(ctx))?;
    let values = db::issue_attributes::values(&ctx.state.pool, repo, issue, actor)
        .await
        .map_err(|_| unavailable(ctx))?;
    let selected_type = db::issue_attributes::assigned_type(&ctx.state.pool, repo, issue)
        .await
        .map_err(|_| unavailable(ctx))?
        .map(|a| a.id);
    let can_configure = if let Some(actor) = actor {
        db::issue_attributes::owner_can_configure(&ctx.state.pool, repo, actor)
            .await
            .map_err(|_| unavailable(ctx))?
    } else {
        false
    };
    let owner = db::repositories::records::get_repository_by_id(&ctx.state.pool, repo).await.map_err(|_| unavailable(ctx))?.ok_or_else(|| unavailable(ctx))?;
    Ok(Box::new(IssueAttributes {
        owner_key: db::issue_attributes::owner_key(owner.owner_user_id, owner.owner_org_id),
        types: types.into_iter().map(|a| view(a, None)).collect(),
        fields: fields
            .into_iter()
            .map(|a| {
                let value = values
                    .iter()
                    .find(|v| v.attribute.id == a.id)
                    .map(|v| v.value.clone());
                view(a, value)
            })
            .collect(),
        selected_type,
        can_configure,
        organization,
    }))
}
fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Missing) => ctx.error(StatusCode::NOT_FOUND, "Issue attribute not found."),
        Some(Refusal::Denied) => {
            ctx.error(StatusCode::FORBIDDEN, "Issue attribute mutation denied.")
        }
        Some(reason) => ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &reason.to_string()),
        None if error.downcast_ref::<sqlx::Error>().is_some_and(|e| {
            e.as_database_error()
                .is_some_and(|d| d.is_unique_violation())
        }) =>
        {
            ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Duplicate issue attribute.",
            )
        }
        None => unavailable(ctx),
    }
}
pub(crate) async fn native_change(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<HashMap<String, String>>,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    authorize_post(&ctx, &form)?;
    let repo = repository(&ctx, &owner, &name).await?;
    let actor = ctx.require_user()?.id;
    let issue = db::issues::get_issue(&ctx.state.pool, repo.record.id, number)
        .await
        .map_err(|_| unavailable(&ctx))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue not found."))?;
    crate::backend::api::repos::discussion::require_item_binding(&ctx, &form, repo.record.id, issue.id)?;
    let id = || {
        field(&form, "attribute_id")
            .parse::<i64>()
            .map_err(|_| ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid issue attribute."))
    };
    match field(&form, "action").as_str() {
        "configure" => {
            let kind = field(&form, "kind");
            let attribute_id = form.get("attribute_id").map(|_| id()).transpose()?;
            let catalog =
                db::issue_attributes::catalog(&ctx.state.pool, repo.record.id, Some(actor), &kind)
                    .await
                    .map_err(|_| unavailable(&ctx))?;
            let parse_priority = |name: &str| {
                let text = field(&form, name);
                if text.trim().is_empty() {
                    Ok(None)
                } else {
                    text.parse::<i64>().map(Some).map_err(|_| {
                        ctx.error(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "Invalid select option priority.",
                        )
                    })
                }
            };
            let mut options = Vec::new();
            if let Some(old) = attribute_id.and_then(|id| catalog.iter().find(|a| a.id == id)) {
                for option in &old.options {
                    if !form.contains_key(&format!("option_{}_remove", option.id)) {
                        options.push(OptionInput {
                            id: Some(option.id),
                            name: field(&form, &format!("option_{}_name", option.id)),
                            color: field(&form, &format!("option_{}_color", option.id)),
                            description: match form
                                .get(&format!("option_{}_description", option.id))
                            {
                                Some(text) => Some(text.clone()).filter(|text| !text.is_empty()),
                                None => option.description.clone(),
                            },
                            priority: if form
                                .contains_key(&format!("option_{}_priority", option.id))
                            {
                                parse_priority(&format!("option_{}_priority", option.id))?
                            } else {
                                Some(option.priority)
                            },
                        });
                    }
                }
            }
            let new_name = field(&form, "new_option_name");
            if !new_name.trim().is_empty() {
                options.push(OptionInput {
                    id: None,
                    name: new_name,
                    color: field(&form, "new_option_color"),
                    description: Some(field(&form, "new_option_description"))
                        .filter(|text| !text.is_empty()),
                    priority: parse_priority("new_option_priority")?,
                });
            }
            let definition = Definition {
                id: attribute_id,
                kind: kind.clone(),
                name: field(&form, "name"),
                description: Some(field(&form, "description")).filter(|s| !s.is_empty()),
                data_type: if kind == "field" {
                    Some(field(&form, "data_type"))
                } else {
                    None
                },
                visibility: if kind == "field" {
                    field(&form, "visibility")
                } else {
                    "all".into()
                },
                is_enabled: kind == "field" || form.get("is_enabled").is_some_and(|v| v == "true"),
                color: Some(field(&form, "color")).filter(|s| !s.is_empty()),
                options,
            };
            db::issue_attributes::configure_checked(&ctx.state.pool, repo.record.id, actor, &definition, (Some(&field(&form, "expected_revision")), Some(&field(&form, "owner_key"))))
                .await
                .map_err(|e| failure(&ctx, e))?;
        }
        "delete_definition" => {
            if field(&form, "confirm") != "true" {
                return Err(ctx.error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Confirm deletion of this attribute and its assignments.",
                ));
            }
            db::issue_attributes::delete_definition(&ctx.state.pool, repo.record.id, actor, id()?)
                .await
                .map_err(|e| failure(&ctx, e))?;
        }
        "set_type" => {
            let kind = field(&form, "type_id");
            let kind = if kind.is_empty() {
                None
            } else {
                Some(kind.parse().map_err(|_| {
                    ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid issue type.")
                })?)
            };
            db::issue_attributes::set_type(&ctx.state.pool, repo.record.id, actor, issue.id, kind)
                .await
                .map_err(|e| failure(&ctx, e))?;
        }
        "delete_value" => {
            db::issue_attributes::delete_value(
                &ctx.state.pool,
                repo.record.id,
                actor,
                issue.id,
                id()?,
            )
            .await
            .map_err(|e| failure(&ctx, e))?;
        }
        "set_value" => {
            let id = id()?;
            let fields = db::issue_attributes::catalog(
                &ctx.state.pool,
                repo.record.id,
                Some(actor),
                "field",
            )
            .await
            .map_err(|_| unavailable(&ctx))?;
            let attribute = fields
                .iter()
                .find(|a| a.id == id)
                .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Issue field not found."))?;
            let value = if attribute.data_type.as_deref() == Some("multi_select") {
                Value::Array(
                    attribute
                        .options
                        .iter()
                        .filter(|o| form.contains_key(&format!("option_{}", o.id)))
                        .map(|o| Value::String(o.name.clone()))
                        .collect(),
                )
            } else if attribute.data_type.as_deref() == Some("single_select") {
                let chosen = field(&form, "value").parse::<i64>().ok();
                let option = attribute
                    .options
                    .iter()
                    .find(|o| Some(o.id) == chosen)
                    .ok_or_else(|| {
                        ctx.error(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "Invalid issue field option.",
                        )
                    })?;
                Value::String(option.name.clone())
            } else if attribute.data_type.as_deref() == Some("number") {
                serde_json::from_str(&field(&form, "value"))
                    .map_err(|_| ctx.error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid number."))?
            } else {
                Value::String(field(&form, "value"))
            };
            db::issue_attributes::set_values(
                &ctx.state.pool,
                repo.record.id,
                actor,
                issue.id,
                &[ValueInput {
                    field_id: id,
                    value,
                }],
                false,
            )
            .await
            .map_err(|e| failure(&ctx, e))?;
        }
        _ => {
            return Err(ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid attribute action.",
            ))
        }
    }
    Ok(ctx.redirect(&format!(
        "/{}/{}/issues/{number}",
        crate::webui::encode_component(&owner),
        crate::webui::encode_component(&name)
    )))
}
