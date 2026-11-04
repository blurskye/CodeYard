use crate::arg;
use crate::arguments::Arguments;
use crate::db;
use crate::missing;
use crate::unavailable;
use crate::url;
use crate::RequestContext;
use crate::Result;
use codeyard_api_github::node_id;
use serde_json::json;
use serde_json::Value;

pub(super) fn object(user: &codeyard_db_core::models::User, viewer: i64) -> Value {
    json!({"__typename":"User","id":node_id::encode("U",user.id),"databaseId":user.id,
        "login":user.username,"name":user.display_name,
        "url":url(&codeyard_webui::profile_path(&user.username)),"avatarUrl":url(&format!("/avatars/{}",user.username)),"createdAt":user.created_at,
        "bio":user.bio,"location":user.location,"websiteUrl":user.website,
        "isViewer":viewer==user.id})
}

pub(super) fn organization(org: &codeyard_db_core::models::Organization) -> Value {
    json!({"__typename":"Organization","id":node_id::encode("O",org.id),"databaseId":org.id,
        "login":org.name,"name":org.display_name,"email":null,"url":url(&codeyard_webui::profile_path(&org.name)),
        "avatarUrl":url(&format!("/avatars/{}",org.name)),"createdAt":org.created_at,
        "description":org.description,"websiteUrl":null})
}

pub(super) async fn by_id(id: i64, request: &RequestContext) -> Result<Value> {
    let user = db::users::accounts::get_user_by_id(&request.state.pool, id)
        .await
        .map_err(unavailable)?
        .filter(|user| !user.is_banned)
        .ok_or_else(missing)?;
    Ok(object(&user, request.auth.user.id))
}

fn actor_value(mut value: Value) -> Value {
    if value["login"]
        .as_str()
        .is_some_and(|login| login.ends_with("[bot]"))
    {
        value["__typename"] = json!("Bot");
    }
    value
}
pub(super) fn actor_object(user: &codeyard_db_core::models::User, viewer: i64) -> Value {
    actor_value(object(user, viewer))
}
pub(super) async fn actor_by_id(id: i64, request: &RequestContext) -> Result<Value> {
    Ok(actor_value(by_id(id, request).await?))
}

/// Authored content keeps its author after the account is suspended; a suspended author reads as null.
pub(super) async fn retained_author(id: i64, request: &RequestContext) -> Result<Value> {
    let user = db::users::accounts::get_user_by_id(&request.state.pool, id)
        .await
        .map_err(unavailable)?;
    Ok(match user.filter(|user| !user.is_banned) {
        Some(user) => actor_object(&user, request.auth.user.id),
        None => Value::Null,
    })
}

/// An organization as another object's projection, such as a repository owner. The standalone
/// organization queries check the read:org grant themselves.
pub(super) async fn organization_by_id(id: i64, request: &RequestContext) -> Result<Value> {
    let org = db::organizations::profiles::get_public_organization_by_id(&request.state.pool, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    Ok(organization(&org))
}

pub(super) async fn resolve(
    ty: &str,
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let id = parent["databaseId"].as_i64().ok_or_else(missing)?;
    match field {
        "team" if ty == "Organization" => {
            crate::team::by_slug(id, &crate::arg(ctx, "slug")?, request).await
        }
        "email" if ty == "User" => Ok(json!(db::users::profiles::public_email(
            &request.state.pool,
            id
        )
        .await
        .map_err(unavailable)?
        .unwrap_or_default())),
        "company" if ty == "User" => Ok(json!(
            db::users::profiles::get_profile_settings(&request.state.pool, id)
                .await
                .map_err(unavailable)?
                .company
        )),
        "repositories" => crate::repository_list::list(ctx, request, parent).await,
        "gists" if ty == "User" => crate::gists::list(ctx, request, parent).await,
        "viewerCanFollow" => Ok(json!(
            id != request.auth.user.id
                && !db::users::blocks::is_blocked(&request.state.pool, request.auth.user.id, id)
                    .await
                    .map_err(unavailable)?
                && !db::users::blocks::is_blocked(&request.state.pool, id, request.auth.user.id)
                    .await
                    .map_err(unavailable)?
        )),
        "viewerIsFollowing" => Ok(json!(
            id != request.auth.user.id
                && db::following::is_user_following(&request.state.pool, request.auth.user.id, id)
                    .await
                    .map_err(unavailable)?
        )),
        _ => Err(async_graphql::Error::new(format!(
            "Codeyard does not support {ty}.{field} yet"
        ))),
    }
}

pub(super) async fn query(field: &str, ctx: &Arguments, request: &RequestContext) -> Result<Value> {
    if field == "viewer" {
        return by_id(request.auth.user.id, request).await;
    }
    if request.bounded() {
        return Err(missing());
    }
    let login = arg(ctx, "login")?;
    if field == "repositoryOwner" {
        if let Some(user) = db::users::accounts::get_user_by_username(&request.state.pool, &login)
            .await
            .map_err(unavailable)?
            .filter(|user| !user.is_banned)
        {
            return Ok(object(&user, request.auth.user.id));
        }
    }
    if field == "user" {
        let user = db::users::accounts::get_user_by_username(&request.state.pool, &login)
            .await
            .map_err(unavailable)?
            .filter(|user| !user.is_banned)
            .ok_or_else(|| {
                codeyard_model::graphql_error::error(
                    format!("Could not resolve to a User with the login of '{login}'."),
                    "NOT_FOUND",
                )
            })?;
        Ok(object(&user, request.auth.user.id))
    } else if field == "repositoryOwner" {
        if let Some(user) = db::users::accounts::get_user_by_username(&request.state.pool, &login)
            .await
            .map_err(unavailable)?
            .filter(|user| !user.is_banned)
        {
            return Ok(object(&user, request.auth.user.id));
        }
        request.require_oauth_scope("read:org")?;
        let org = db::organizations::profiles::get_public_organization(&request.state.pool, &login)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        request.authorize_organization(org.id).await?;
        Ok(organization(&org))
    } else {
        request.require_oauth_scope("read:org")?;
        // gh skips the classic-projects lookup only on GitHub's exact wording.
        let org = db::organizations::profiles::get_public_organization(&request.state.pool, &login)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| {
                codeyard_model::graphql_error::error(
                    format!("Could not resolve to an Organization with the login of '{login}'."),
                    "NOT_FOUND",
                )
            })?;
        request.authorize_organization(org.id).await?;
        Ok(organization(&org))
    }
}
