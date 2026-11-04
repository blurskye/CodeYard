//! Release lists share the native draft visibility rule.
use crate::arg;
use crate::arguments::Arguments;
use crate::missing;
use crate::unavailable;
use crate::RequestContext;
use crate::Result;
use codeyard_app::repositories::store::repo_store;
use serde_json::Value;

pub(super) async fn drafts_visible(
    repo: &codeyard_db_core::models::Repository,
    request: &RequestContext,
) -> Result<bool> {
    if request.private_viewer(repo.id, "contents").await?.is_none() {
        return Ok(false);
    }
    Ok(codeyard_identity::repository_access::user_access(
        &request.state.pool,
        &request.auth.user,
        repo,
    )
    .await
    .map_err(unavailable)?
    .can_write())
}

pub(super) async fn resolve(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    repo: &codeyard_db_core::models::Repository,
) -> Result<Value> {
    request.authorize(repo.id, "contents", false).await?;
    let pool = &request.state.pool;
    let writer = drafts_visible(repo, request).await?;
    match field {
        "release" | "latestRelease" => {
            let row = if field == "release" {
                codeyard_db_repos::releases::get_release_by_tag(
                    pool,
                    repo.id,
                    &arg(ctx, "tagName")?,
                )
                .await
                .map_err(unavailable)?
                .map(|row| row.release)
            } else {
                codeyard_db_repos::releases::latest_release(pool, repo.id)
                    .await
                    .map_err(unavailable)?
            };
            match row {
                Some(row) if !row.is_draft || writer => {
                    crate::node_objects::release(&row, request).await
                }
                _ => Ok(Value::Null),
            }
        }
        _ => {
            let total = codeyard_db_repos::releases::count_releases(pool, repo.id, writer)
                .await
                .map_err(unavailable)?;
            let page = crate::connection::Page::new(ctx, total)?;
            // The schema gives `orderBy` no default; without one, releases come newest first.
            let order = match ctx.args.get("orderBy").filter(|order| !order.is_null()) {
                None => codeyard_db_repos::releases::ReleaseOrder::CreatedDesc,
                Some(order) => {
                    let order = order.object()?;
                    match (
                        order.try_get("field")?.enum_name()?,
                        order.try_get("direction")?.enum_name()?,
                    ) {
                        ("CREATED_AT", "ASC") => {
                            codeyard_db_repos::releases::ReleaseOrder::CreatedAsc
                        }
                        ("CREATED_AT", "DESC") => {
                            codeyard_db_repos::releases::ReleaseOrder::CreatedDesc
                        }
                        ("NAME", "ASC") => codeyard_db_repos::releases::ReleaseOrder::NameAsc,
                        ("NAME", "DESC") => codeyard_db_repos::releases::ReleaseOrder::NameDesc,
                        _ => return Err(missing()),
                    }
                }
            };
            let rows = codeyard_db_repos::releases::list_releases_ordered(
                pool,
                repo.id,
                writer,
                order,
                page.limit,
                page.offset,
            )
            .await
            .map_err(unavailable)?;
            let mut nodes = Vec::new();
            for row in rows {
                nodes.push(crate::node_objects::release(&row.release, request).await?);
            }
            Ok(page.finish(nodes))
        }
    }
}

pub(super) async fn field(
    field: &str,
    args: &Arguments,
    parent: &Value,
    request: &RequestContext,
) -> Result<Value> {
    let repo =
        crate::repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    request.authorize(repo.id, "contents", false).await?;
    match field {
        "reactionGroups" => {
            crate::collaboration::reactions(
                codeyard_db_repos::reactions::Subject::Release,
                parent["databaseId"].as_i64().ok_or_else(missing)?,
                request,
            )
            .await
        }
        "releaseAssets" => {
            crate::connection::validate(args)?;
            let mut rows = codeyard_db_repos::releases::list_release_assets(
                &request.state.pool,
                parent["_release"].as_i64().ok_or_else(missing)?,
            )
            .await
            .map_err(unavailable)?;
            if let Some(name) = args.args.get("name").filter(|v| !v.is_null()) {
                let name = name.string()?;
                rows.retain(|row| row.name == name);
            }
            let page = crate::connection::Page::new(args, rows.len() as i64)?;
            let mut nodes = vec![];
            for row in rows
                .into_iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
            {
                nodes.push(crate::node_objects::asset(repo.id, &row, request).await?);
            }
            Ok(page.finish(nodes))
        }
        "tag" => {
            crate::git_objects::reference(
                &repo,
                &format!(
                    "refs/tags/{}",
                    parent["tagName"].as_str().ok_or_else(missing)?
                ),
                request,
            )
            .await
        }
        "tagCommit" => {
            let name = format!(
                "refs/tags/{}",
                parent["tagName"].as_str().ok_or_else(missing)?
            );
            let store = repo_store(&repo, request.state.cluster.as_ref());
            if !store.refs().await.map_err(unavailable)?.contains_key(&name) {
                return Ok(Value::Null);
            }
            let row = store.commit(&name).await.map_err(unavailable)?;
            crate::git::commit(repo.id, &row, "contents", request)
        }
        _ => Err(async_graphql::Error::new(format!(
            "Codeyard does not support Release.{field} yet"
        ))),
    }
}
