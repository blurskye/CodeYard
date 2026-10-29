use super::{
    arg, connection, db, missing, node_id, repository, unavailable, url, user, Arguments,
    RequestContext, Result, Value,
};
use async_graphql::{Error, ErrorExtensions};
use db::repository_discussions::{self as store, Filter, Order, Write};
use serde_json::json;

pub(super) fn operation(error: anyhow::Error) -> Error {
    let kind = match error.downcast_ref::<store::Refusal>() {
        Some(store::Refusal::Missing) => return missing(),
        Some(store::Refusal::Denied) => "FORBIDDEN",
        Some(store::Refusal::Invalid) => "UNPROCESSABLE",
        None => return unavailable(error),
    };
    Error::new(error.to_string()).extend_with(|_, e| {
        e.set("type", kind);
    })
}
fn id(value: &str, kind: &str) -> Result<i64> {
    node_id::decode(value)
        .filter(|(ty, _)| *ty == kind)
        .map(|(_, id)| id)
        .ok_or_else(missing)
}
pub(super) fn object(d: &store::Discussion) -> Value {
    json!({"__typename":"Discussion","id":node_id::encode("D",d.id),"databaseId":d.id,"_repo":d.repo_id,"_author":d.author_id,"_category":d.category_id,"_answer":d.answer_id,"_answerBy":d.answer_chosen_by,
        "number":d.number,"title":d.title,"body":d.body,"locked":d.locked,"closed":false,"closedAt":null,"stateReason":null,
        "isAnswered":d.answer_id.is_some(),"answerChosenAt":d.answer_chosen_at,"createdAt":d.created_at,"updatedAt":d.updated_at,"reactionGroups":[]})
}
fn category(c: &store::Category) -> Value {
    json!({"__typename":"DiscussionCategory","id":node_id::encode("DIC",c.id),"_repo":c.repo_id,"name":c.name,"slug":c.slug,"emoji":c.emoji,"description":c.description,"isAnswerable":c.answerable,"createdAt":c.created_at})
}
fn comment(c: &store::Comment, d: &store::Discussion) -> Value {
    json!({"__typename":"DiscussionComment","id":node_id::discussion_comment(d.repo_id,c.id),"databaseId":c.id,"_repo":d.repo_id,"_discussion":d.id,"_number":d.number,"_author":c.author_id,"_reply":c.reply_to_id,
        "body":c.body,"isAnswer":c.is_answer,"createdAt":c.created_at,"updatedAt":c.updated_at,"reactionGroups":[],"upvoteCount":0})
}
async fn authorized(d: &store::Discussion, request: &RequestContext) -> Result<()> {
    repository::by_id(d.repo_id, request).await?;
    request.authorize(d.repo_id, "discussions", false).await
}
async fn load(id: i64, request: &RequestContext) -> Result<store::Discussion> {
    let d = store::get(&request.state.pool, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    authorized(&d, request).await?;
    Ok(d)
}
pub(super) async fn node(value: &str, request: &RequestContext) -> Result<Value> {
    if value.starts_with("DC_") {
        let (repo, id) = node_id::decode_discussion_comment(value).ok_or_else(missing)?;
        let c = store::comment(&request.state.pool, id)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        let d = load(c.discussion_id, request).await?;
        if d.repo_id != repo {
            return Err(missing());
        }
        return Ok(comment(&c, &d));
    }
    let (kind, id) = node_id::decode(value).ok_or_else(missing)?;
    match kind {
        "D" => Ok(object(&load(id, request).await?)),
        "DIC" => {
            // Category IDs carry no authority; resolve the owning repository before exposing one.
            let c = store::category_by_id(&request.state.pool, id)
                .await
                .map_err(unavailable)?
                .ok_or_else(missing)?;
            repository::by_id(c.repo_id, request).await?;
            request.authorize(c.repo_id, "discussions", false).await?;
            Ok(category(&c))
        }
        _ => Err(missing()),
    }
}
pub(super) async fn repository_field(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    repo: i64,
) -> Result<Value> {
    request.authorize(repo, "discussions", false).await?;
    let pool = &request.state.pool;
    match field {
        "hasDiscussionsEnabled" => Ok(json!(store::enabled(pool, repo)
            .await
            .map_err(unavailable)?)),
        "discussionCategories" => {
            connection::validate(ctx)?;
            if ctx
                .args
                .get("filterByAssignable")
                .filter(|v| !v.is_null())
                .map(|v| v.deserialize::<bool>())
                .transpose()?
                .unwrap_or(false)
            {
                return Err(Error::new(
                    "Codeyard does not support assignable category filtering yet",
                ));
            }
            connection::slice(
                ctx,
                store::categories(pool, repo)
                    .await
                    .map_err(unavailable)?
                    .iter()
                    .map(category)
                    .collect(),
            )
        }
        "discussion" => Ok(
            store::by_number(pool, repo, ctx.args.try_get("number")?.i64()? as i32)
                .await
                .map_err(unavailable)?
                .as_ref()
                .map(object)
                .unwrap_or(Value::Null),
        ),
        "discussions" => {
            let mut filter = Filter {
                repo: Some(repo),
                ..Default::default()
            };
            if let Some(v) = ctx.args.get("categoryId").filter(|v| !v.is_null()) {
                filter.category = Some(id(v.string()?, "DIC")?);
            }
            if let Some(v) = ctx.args.get("answered").filter(|v| !v.is_null()) {
                filter.answered = Some(v.deserialize()?);
            }
            if let Some(v) = ctx.args.get("states").filter(|v| !v.is_null()) {
                let states: Vec<String> = v.deserialize()?;
                filter.closed_only = !states.is_empty() && !states.iter().any(|v| v == "OPEN");
            }
            if let Some(v) = ctx.args.get("orderBy").filter(|v| !v.is_null()) {
                let order = v.object()?;
                filter.ascending = order.try_get("direction")?.enum_name()? == "ASC";
                filter.order = match order.try_get("field")?.enum_name()? {
                    "CREATED_AT" => Order::Created,
                    "UPDATED_AT" => Order::Updated,
                    _ => return Err(Error::new("Unsupported discussion order")),
                };
            }
            list(ctx, request, &filter, false).await
        }
        _ => Err(missing()),
    }
}
async fn list(
    ctx: &Arguments,
    request: &RequestContext,
    filter: &Filter,
    search: bool,
) -> Result<Value> {
    connection::validate(ctx)?;
    let pool = &request.state.pool;
    let (count, _) = store::list(pool, request.auth.user.id, filter, 0, 0)
        .await
        .map_err(operation)?;
    let page = connection::Page::new(ctx, if search { count.min(1000) } else { count })?;
    let (_, rows) = store::list(pool, request.auth.user.id, filter, page.offset, page.limit)
        .await
        .map_err(operation)?;
    let mut nodes = Vec::new();
    for d in rows {
        authorized(&d, request).await?;
        nodes.push(object(&d));
    }
    let mut result = page.finish(nodes);
    if search {
        result["discussionCount"] = json!(count);
    }
    Ok(result)
}
pub(super) async fn search(ctx: &Arguments, request: &RequestContext) -> Result<Value> {
    let filter = Filter::search(&arg(ctx, "query")?).map_err(operation)?;
    list(ctx, request, &filter, true).await
}
pub(super) async fn resolve(
    ty: &str,
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
    parent: &Value,
) -> Result<Value> {
    let repo_id = parent["_repo"].as_i64().ok_or_else(missing)?;
    let repo = repository::by_id(repo_id, request).await?;
    request.authorize(repo_id, "discussions", false).await?;
    if let Some(value) = parent.get(field) {
        return Ok(value.clone());
    }
    let pool = &request.state.pool;
    match field {
        "repository" => Ok(repository::object(&repo)),
        "author" => user::retained_author(parent["_author"].as_i64().ok_or_else(missing)?, request).await,
        "answerChosenBy" => match parent["_answerBy"].as_i64() {
            Some(id) => user::retained_author(id, request).await,
            None => Ok(Value::Null),
        },
        "answer" => match parent["_answer"].as_i64() {
            Some(id) => {
                let c = store::comment(pool, id)
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(missing)?;
                let d = load(c.discussion_id, request).await?;
                Ok(comment(&c, &d))
            }
            None => Ok(Value::Null),
        },
        "category" => {
            let c = store::category_by_id(pool, parent["_category"].as_i64().ok_or_else(missing)?)
                .await
                .map_err(unavailable)?
                .filter(|c| c.repo_id == repo_id)
                .ok_or_else(missing)?;
            Ok(category(&c))
        }
        "discussion" => Ok(object(
            &load(parent["_discussion"].as_i64().ok_or_else(missing)?, request).await?,
        )),
        "labels" => repository::label_connection(ctx,
            store::labels(pool, parent["databaseId"].as_i64().ok_or_else(missing)?)
                .await.map_err(unavailable)?, "discussions"),
        "comments" | "replies" => {
            connection::validate(ctx)?;
            let (discussion, reply) = if ty == "Discussion" {
                (parent["databaseId"].as_i64().ok_or_else(missing)?, None)
            } else {
                (
                    parent["_discussion"].as_i64().ok_or_else(missing)?,
                    Some(parent["databaseId"].as_i64().ok_or_else(missing)?),
                )
            };
            let d = load(discussion, request).await?;
            let (count, _) = store::comments(pool, discussion, reply, 0, 0)
                .await
                .map_err(unavailable)?;
            let page = connection::Page::new(ctx, count)?;
            let (_, rows) = store::comments(pool, discussion, reply, page.offset, page.limit)
                .await
                .map_err(unavailable)?;
            Ok(page.finish(rows.iter().map(|c| comment(c, &d)).collect()))
        }
        "url" => {
            let number = if ty == "Discussion" {
                &parent["number"]
            } else {
                &parent["_number"]
            };
            let mut path = format!(
                "/{}/{}/discussions/{number}",
                repository::owner(&repo, request).await?,
                repo.name
            );
            if ty == "DiscussionComment" {
                path.push_str(&format!("#discussioncomment-{}", parent["databaseId"]));
            }
            Ok(json!(url(&path)))
        }
        _ => Err(Error::new(format!(
            "Codeyard does not support {ty}.{field} yet"
        ))),
    }
}

pub(super) async fn mutation(
    field: &str,
    ctx: &Arguments,
    request: &RequestContext,
) -> Result<Value> {
    request
        .auth
        .require_write()
        .map_err(|_| Error::new("Resource not accessible by token"))?;
    let input: Value = ctx.args.try_get("input")?.deserialize()?;
    let string = |name| {
        input[name]
            .as_str()
            .ok_or_else(|| Error::new(format!("Missing {name}")))
    };
    let allowed: &[&str] = match field {
        "createDiscussion" => &[
            "repositoryId",
            "categoryId",
            "title",
            "body",
            "clientMutationId",
        ],
        "updateDiscussion" => &[
            "discussionId",
            "categoryId",
            "title",
            "body",
            "clientMutationId",
        ],
        "deleteDiscussion" => &["id", "clientMutationId"],
        "addDiscussionComment" => &["discussionId", "body", "replyToId", "clientMutationId"],
        "updateDiscussionComment" => &["commentId", "body", "clientMutationId"],
        "deleteDiscussionComment" => &["id", "clientMutationId"],
        "markDiscussionCommentAsAnswer" | "unmarkDiscussionCommentAsAnswer" => {
            &["id", "clientMutationId"]
        }
        "addLabelsToLabelable" | "removeLabelsFromLabelable" => {
            &["labelableId", "labelIds", "clientMutationId"]
        }
        _ => return Err(Error::new("Unsupported discussion mutation")),
    };
    if let Some(name) = input
        .as_object()
        .into_iter()
        .flat_map(|v| v.keys())
        .find(|k| !allowed.contains(&k.as_str()) && !input[*k].is_null())
    {
        return Err(Error::new(format!(
            "Codeyard does not support {field}.{name} yet"
        )));
    }
    let pool = &request.state.pool;
    let mut payload = json!({"clientMutationId":input["clientMutationId"]});
    if field == "createDiscussion" {
        let repo = id(string("repositoryId")?, "R")?;
        repository::by_id(repo, request).await?;
        request.authorize(repo, "discussions", true).await?;
        let change = Write::Create {
            category: id(string("categoryId")?, "DIC")?,
            title: string("title")?,
            body: string("body")?,
        };
        let saved = store::write(pool, request.auth.user.id, repo, None, &change)
            .await
            .map_err(operation)?;
        payload["discussion"] = object(&load(saved, request).await?);
        return Ok(payload);
    }
    let is_comment = matches!(
        field,
        "updateDiscussionComment"
            | "deleteDiscussionComment"
            | "markDiscussionCommentAsAnswer"
            | "unmarkDiscussionCommentAsAnswer"
    );
    let target = string(if field == "updateDiscussionComment" {
        "commentId"
    } else if is_comment || field == "deleteDiscussion" {
        "id"
    } else if field.ends_with("Labelable") {
        "labelableId"
    } else {
        "discussionId"
    })?;
    let old = node(target, request).await?;
    let discussion = if is_comment {
        old["_discussion"].as_i64().ok_or_else(missing)?
    } else {
        id(target, "D")?
    };
    let d = load(discussion, request).await?;
    request.authorize(d.repo_id, "discussions", true).await?;
    let comment_id = old["databaseId"].as_i64().ok_or_else(missing)?;
    let labels = super::metadata_mutations::ids(&input["labelIds"], "L")?;
    let category = input["categoryId"]
        .as_str()
        .map(|v| id(v, "DIC"))
        .transpose()?;
    let reply = if let Some(value) = input["replyToId"].as_str() {
        let reply = node(value, request).await?;
        if reply["_discussion"].as_i64() != Some(d.id) {
            return Err(missing());
        }
        Some(reply["databaseId"].as_i64().ok_or_else(missing)?)
    } else {
        None
    };
    let change = match field {
        "updateDiscussion" => Write::Edit {
            title: input["title"].as_str(),
            body: input["body"].as_str(),
            category,
        },
        "deleteDiscussion" => Write::Delete,
        "addDiscussionComment" => Write::AddComment {
            body: string("body")?,
            reply,
        },
        "updateDiscussionComment" => Write::EditComment {
            comment: comment_id,
            body: string("body")?,
        },
        "deleteDiscussionComment" => Write::DeleteComment(comment_id),
        "markDiscussionCommentAsAnswer" | "unmarkDiscussionCommentAsAnswer" => Write::Answer {
            comment: comment_id,
            mark: field == "markDiscussionCommentAsAnswer",
        },
        "addLabelsToLabelable" | "removeLabelsFromLabelable" => Write::Labels {
            labels: &labels,
            add: field == "addLabelsToLabelable",
        },
        _ => return Err(missing()),
    };
    let saved = store::write(pool, request.auth.user.id, d.repo_id, Some(d.id), &change)
        .await
        .map_err(operation)?;
    let key = if field.ends_with("AsAnswer") {
        "discussion"
    } else if is_comment || field == "addDiscussionComment" {
        "comment"
    } else if field.ends_with("Labelable") {
        "labelable"
    } else {
        "discussion"
    };
    payload[key] = if matches!(field, "deleteDiscussion" | "deleteDiscussionComment") {
        old
    } else if key == "comment" {
        let c = store::comment(pool, saved)
            .await
            .map_err(unavailable)?
            .ok_or_else(missing)?;
        comment(&c, &d)
    } else {
        object(&load(d.id, request).await?)
    };
    Ok(payload)
}
