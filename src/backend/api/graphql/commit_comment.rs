//! Commit discussion reads retain REST's repository visibility and author-association rules.
use super::{
    connection::Page, db, missing, node_id, repository, unavailable, url, user, Arguments,
    RequestContext, Result, Value,
};
use crate::backend::{
    db::repositories::commit_comments as records,
    repositories::{access, commit_comments},
};
use serde_json::json;
pub(super) async fn object(
    row: &codeyard_db_core::models::CommitComment,
    request: &RequestContext,
) -> Result<Value> {
    let repo = repository::by_id(row.repo_id, request).await?;
    request.authorize(repo.id, "contents", false).await?;
    let authored = row.author_id == Some(request.auth.user.id);
    let writable = !repo.is_archived
        && !request.auth.user.is_banned
        && !request.auth.user.must_change_password;
    let owner = repository::owner(&repo, request).await?;
    Ok(
        json!({"__typename":"CommitComment","_repo":repo.id,"id":node_id::encode("CC",row.id),"databaseId":row.id,"body":row.body,"path":row.path,"position":row.position,
        "author":match row.author_id {Some(id)=>user::retained_author(id,request).await?,None=>Value::Null},"createdAt":row.created_at,"updatedAt":row.updated_at,
        "includesCreatedEdit":row.updated_at>row.created_at,"lastEditedAt":if row.updated_at>row.created_at {Some(row.updated_at)} else {None},"isMinimized":false,"minimizedReason":null,
        "viewerDidAuthor":authored,"viewerCanUpdate":writable&&authored,"viewerCanDelete":writable&&(authored || access::can_maintain(&request.state.pool,&request.auth.user,&repo).await.map_err(unavailable)?),
        "repository":repository::object(&repo),"commit":{"__typename":"Commit","_repo":repo.id,"_scope":"contents","oid":row.sha},"url":url(&commit_comments::comment_link(&owner,&repo.name,row))}),
    )
}
pub(super) async fn field(field: &str, parent: &Value, request: &RequestContext) -> Result<Value> {
    if field == "reactionGroups" {
        return super::collaboration::reactions(
            db::reactions::Subject::CommitComment,
            parent["databaseId"].as_i64().ok_or_else(missing)?,
            request,
        )
        .await;
    }
    if field != "authorAssociation" {
        return Err(async_graphql::Error::new(format!(
            "Codeyard does not support CommitComment.{field} yet"
        )));
    }
    let repo = repository::by_id(parent["_repo"].as_i64().ok_or_else(missing)?, request).await?;
    request.authorize(repo.id, "contents", false).await?;
    let row = records::get_comment(
        &request.state.pool,
        repo.id,
        parent["databaseId"].as_i64().ok_or_else(missing)?,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(missing)?;
    let details = commit_comments::comment_details(
        &request.state.pool,
        request.state.cluster.as_ref(),
        &repo,
        vec![row],
    )
    .await
    .map_err(unavailable)?;
    Ok(json!(details.first().ok_or_else(missing)?.association))
}
pub(super) async fn list(
    repo: i64,
    oid: &str,
    args: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    let repository = repository::by_id(repo, request).await?;
    request.authorize(repo, "contents", false).await?;
    let total = records::count_comments(&request.state.pool, repo, oid, None)
        .await
        .map_err(unavailable)?;
    let page = Page::new(args, total)?;
    let rows = if page.limit == 0 {
        vec![]
    } else {
        records::list_comments(
            &request.state.pool,
            repo,
            Some(oid),
            page.offset,
            page.limit,
        )
        .await
        .map_err(unavailable)?
    };
    let details = commit_comments::comment_details(
        &request.state.pool,
        request.state.cluster.as_ref(),
        &repository,
        rows,
    )
    .await
    .map_err(unavailable)?;
    let mut nodes = vec![];
    for row in details {
        let mut value = object(&row.comment, request).await?;
        value["authorAssociation"] = json!(row.association);
        nodes.push(value);
    }
    Ok(page.finish(nodes))
}
