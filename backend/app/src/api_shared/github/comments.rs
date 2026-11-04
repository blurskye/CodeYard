use axum::routing::get;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::Value;
use std::collections::HashMap;
pub struct Comment {
    pub id: i64,
    pub item: AssignedItem,
    number: i32,
    author_id: i64,
    body: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<codeyard_db_repos::issues::ConversationComment> for Comment {
    fn from(row: codeyard_db_repos::issues::ConversationComment) -> Self {
        Self {
            id: row.id,
            item: if row.pull_request {
                AssignedItem::PullRequest(row.item_id)
            } else {
                AssignedItem::Issue(row.item_id)
            },
            number: row.number,
            author_id: row.author_id,
            body: row.body,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

fn item_key(item: AssignedItem) -> (bool, i64) {
    match item {
        AssignedItem::Issue(id) => (false, id),
        AssignedItem::PullRequest(id) => (true, id),
    }
}

pub async fn find(pool: &codeyard_db_core::DbPool, repo_id: i64, id: i64) -> Result<Comment> {
    if id < 2 {
        return Err(Error::missing());
    }
    let native_id = id / 2;
    if id % 2 == 0 {
        let comment = codeyard_db_repos::issues::get_issue_comment(pool, native_id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let issue = codeyard_db_repos::issues::get_issue_by_id(pool, comment.issue_id)
            .await
            .map_err(Error::unavailable)?
            .filter(|issue| issue.repo_id == repo_id)
            .ok_or_else(Error::missing)?;
        Ok(Comment {
            id: native_id,
            item: AssignedItem::Issue(issue.id),
            number: issue.number,
            author_id: comment.author_id,
            body: comment.body,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
        })
    } else {
        let comment = codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(
            pool, native_id,
        )
        .await
        .map_err(Error::unavailable)?
        .filter(|comment| comment.path.is_none() && comment.review_id.is_none())
        .ok_or_else(Error::missing)?;
        let pull = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
            pool,
            comment.pull_request_id,
        )
        .await
        .map_err(Error::unavailable)?
        .filter(|pull| pull.repo_id == repo_id)
        .ok_or_else(Error::missing)?;
        Ok(Comment {
            id: native_id,
            item: AssignedItem::PullRequest(pull.id),
            number: pull.number,
            author_id: comment.author_id,
            body: comment.body,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
        })
    }
}

pub async fn document(
    pool: &codeyard_db_core::DbPool,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    comment: &Comment,
) -> Result<Value> {
    documents(pool, repo, owner, std::slice::from_ref(comment))
        .await?
        .pop()
        .ok_or_else(Error::missing)
}

pub async fn documents(
    pool: &codeyard_db_core::DbPool,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    comments: &[Comment],
) -> Result<Vec<Value>> {
    if comments.len() > 100 {
        return Err(Error::unavailable("Comment page exceeds its bound"));
    }
    if comments.is_empty() {
        return Ok(Vec::new());
    }
    let mut pins = HashMap::new();
    for pull in [false, true] {
        let mut ids: Vec<_> = comments
            .iter()
            .map(|comment| item_key(comment.item))
            .filter(|(kind, _)| *kind == pull)
            .map(|(_, id)| id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        for pin in codeyard_db_repos::pins::get_pins(pool, pull, &ids)
            .await
            .map_err(Error::unavailable)?
        {
            pins.insert((pull, pin.item_id), pin.pin);
        }
    }
    let mut authors: Vec<_> = comments.iter().map(|comment| comment.author_id).collect();
    authors.sort_unstable();
    authors.dedup();
    let associations =
        codeyard_identity::repository_access::author_associations(pool, repo, &authors)
            .await
            .map_err(Error::unavailable)?;
    let mut ids = authors;
    ids.extend(pins.values().filter_map(|pin| pin.pinned_by));
    ids.sort_unstable();
    ids.dedup();
    let users: HashMap<_, _> = codeyard_db_identity::users::accounts::get_users_by_ids(pool, &ids)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|user| (user.id, user))
        .collect();
    let mut values = Vec::with_capacity(comments.len());
    for comment in comments {
        let user = users
            .get(&comment.author_id)
            .ok_or_else(|| Error::unavailable(()))?;
        let author = crate::webhooks::payloads::Person::of(user);
        let mut value = crate::webhooks::payloads::conversation_comment(
            comment.id,
            &comment.body,
            &author,
            (comment.created_at, comment.updated_at),
            crate::webhooks::payloads::Scope {
                repo,
                owner,
                sender: &author,
            },
            comment.number,
            matches!(comment.item, AssignedItem::PullRequest(_)),
        );
        value["user"] = crate::webhooks::payloads::user(user);
        value["author_association"] =
            serde_json::json!(associations.get(&user.id).map_or("NONE", String::as_str));
        value["pin"] = pins
            .get(&item_key(comment.item))
            .filter(|pin| pin.comment_id == comment.id)
            .map_or(Value::Null, |pin| {
                crate::webhooks::payloads::comment_pin(
                    pin,
                    pin.pinned_by.and_then(|id| users.get(&id)),
                )
            });
        values.push(value);
    }
    Ok(values)
}
