//! gh's gist picker uses creation order and loads file text only when requested.
use base64::Engine;
use codeyard_model::node_id;

use crate::arguments::Arguments;
use crate::missing;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use async_graphql::Error;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_db_repos::gists::Gist;
use serde_json::json;
use serde_json::Value;

async fn authorize(request: &RequestContext) -> Result<()> {
    if !request.account_permission("gists", "gist").await? {
        return Err(crate::token_denied());
    }
    Ok(())
}

fn object(gist: &Gist) -> Value {
    json!({"__typename":"Gist","id":node_id::git("GIST",&gist.id),"name":gist.id,"description":gist.description,"isPublic":gist.is_public,"updatedAt":gist.updated_at})
}

/// The gist a REST `node_id` names. Like the REST read, its ID is enough to read a secret gist.
pub(super) async fn node(key: &str, request: &RequestContext) -> Result<Value> {
    authorize(request).await?;
    let id = URL_SAFE_NO_PAD
        .decode(key)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(missing)?;
    let gist = codeyard_db_repos::gists::get(&request.state.pool, &id)
        .await
        .map_err(unavailable)?
        .filter(|gist| gist.ready)
        .ok_or_else(missing)?;
    Ok(object(&gist))
}

pub(super) async fn list(ctx: &Arguments, request: &RequestContext, user: &Value) -> Result<Value> {
    authorize(request).await?;
    crate::connection::validate(ctx)?;
    if let Some(order) = ctx.args.get("orderBy").filter(|value| !value.is_null()) {
        let order = order.object()?;
        if order.try_get("field")?.enum_name()? != "CREATED_AT"
            || order.try_get("direction")?.enum_name()? != "DESC"
        {
            return Err(Error::new(
                "Only CREATED_AT DESC gist ordering is supported",
            ));
        }
    }
    let owner = user["databaseId"].as_i64().ok_or_else(missing)?;
    let secret = owner == request.auth.user.id;
    let privacy = match ctx.args.get("privacy").filter(|value| !value.is_null()) {
        Some(value) => match value.enum_name()? {
            "PUBLIC" => Some(true),
            "SECRET" => Some(false),
            _ => None,
        },
        None => None,
    };
    if secret {
        codeyard_app::gists::recover_owned(&request.state, owner)
            .await
            .map_err(unavailable)?;
    }
    let pool = &request.state.pool;
    let total = codeyard_db_repos::gists::count_owned(pool, owner, secret, privacy)
        .await
        .map_err(unavailable)?;
    let page = crate::connection::Page::new(ctx, total)?;
    let entries = codeyard_db_repos::gists::list_created(
        pool,
        owner,
        secret,
        privacy,
        page.limit,
        page.offset,
    )
    .await
    .map_err(unavailable)?;
    Ok(page.finish(entries.iter().map(object).collect()))
}

pub(super) async fn files(
    ctx: &Arguments,
    request: &RequestContext,
    gist: &Value,
) -> Result<Value> {
    authorize(request).await?;
    let id = gist["name"].as_str().ok_or_else(missing)?;
    let revision = ctx
        .args
        .get("oid")
        .filter(|value| !value.is_null())
        .map(|value| value.string())
        .transpose()?;
    let loaded = codeyard_app::gists::load(&request.state, id, revision)
        .await
        .map_err(unavailable)?;
    let limit = ctx
        .args
        .get("limit")
        .filter(|value| !value.is_null())
        .map(|value| value.i64())
        .transpose()?
        .unwrap_or(100);
    if !(0..=100).contains(&limit) {
        return Err(Error::new("File limit must be between 0 and 100"));
    }
    Ok(json!(loaded
        .snapshot
        .files
        .into_iter()
        .take(limit as usize)
        .map(|file| { json!({"name":file.path,"repoId":loaded.repository.id,"sha":file.sha}) })
        .collect::<Vec<_>>()))
}

pub(super) async fn text(ctx: &Arguments, request: &RequestContext, file: &Value) -> Result<Value> {
    authorize(request).await?;
    let limit = ctx
        .args
        .get("truncate")
        .filter(|value| !value.is_null())
        .map(|value| value.i64())
        .transpose()?
        .unwrap_or(1024 * 1024);
    if !(0..=1024 * 1024).contains(&limit) {
        return Err(Error::new("Text limit must be between 0 and 1048576"));
    }
    let repo = file["repoId"].as_i64().ok_or_else(missing)?;
    let gist = codeyard_db_repos::gists::by_repository(&request.state.pool, repo)
        .await
        .map_err(unavailable)?
        .filter(|gist| gist.ready)
        .ok_or_else(missing)?;
    let repository = codeyard_db_repos::gists::git_repository(&request.state.pool, &gist.id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let sha = file["sha"].as_str().ok_or_else(missing)?;
    let bytes = repo_store(&repository, request.state.cluster.as_ref())
        .blob_by_id(sha)
        .await
        .map_err(unavailable)?;
    request.charge(bytes.len())?;
    Ok(json!(codeyard_app::gists::text_preview(
        &bytes,
        limit as usize
    )))
}
