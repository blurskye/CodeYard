//! Stored state that GitHub sends with webhook objects: topics, labels, assignees, milestones,
//! counts and reactions. The payload builders run without the database, so `complete` reads
//! these fields before a delivery is recorded and sent.
use crate::backend::db::{
    self,
    assignees::AssignedItem,
    models::{Release, ReleaseAsset, Repository},
    reactions::Subject,
    DbPool,
};
use crate::backend::repositories::access;
use crate::backend::webhooks::payloads;
use serde_json::{json, Value};

/// Fills what it can. A field that cannot be read is left as the builder made it.
pub(crate) async fn complete(pool: &DbPool, repo: &Repository, owner: &str, payload: &mut Value) {
    if let Err(error) = fill(pool, repo, owner, payload).await {
        tracing::warn!(repo_id = repo.id, %error, "stored webhook fields could not be read");
    }
}

async fn fill(
    pool: &DbPool,
    repo: &Repository,
    owner: &str,
    payload: &mut Value,
) -> anyhow::Result<()> {
    if let Some(repository) = payload
        .get_mut("repository")
        .filter(|value| value.is_object())
    {
        // A source subscription does not grant access to its private remediation forks.
        let forks = db::repositories::discovery::public_fork_counts(pool, &[repo.id]).await?
            .remove(&repo.id).unwrap_or_default();
        payloads::set_fork_counts(repository, forks);
        repository["topics"] = json!(topics(pool, repo.id).await?);
    }
    if let Some(item) = issue_item(&payload["issue"]) {
        // A deleted issue has no document. The stored document supplies labels, assignees and
        // the milestone, while the event keeps the fields as its own change left them, even if
        // a later change has since been saved.
        if let Some(mut stored) =
            crate::backend::api::github::issue_webhook_document(pool, repo, owner, item).await
        {
            if let (Some(event), Some(fields)) =
                (payload["issue"].as_object(), stored.as_object_mut())
            {
                for key in [
                    "title",
                    "body",
                    "state",
                    "state_reason",
                    "locked",
                    "active_lock_reason",
                    "created_at",
                    "updated_at",
                    "closed_at",
                ] {
                    if let Some(value) = event.get(key) {
                        fields.insert(key.to_owned(), value.clone());
                    }
                }
            }
            payload["issue"] = stored;
        }
        if payload["comment"].is_object() {
            comment(pool, repo, item, &mut payload["comment"]).await?;
        }
    }
    if let Some(id) = payload["pull_request"]["id"].as_i64() {
        pull_request(pool, repo, owner, id, &mut payload["pull_request"]).await?;
    }
    // A review comment comes with its pull request and no issue, so the issue branch skipped it.
    if payload["issue"].is_null() && payload["pull_request"].is_object() {
        if let Some(user) = payload["comment"]["user"]["id"].as_i64() {
            payload["comment"]["author_association"] =
                json!(access::author_association(pool, repo, user).await?);
        }
    }
    if let Some(user) = payload["review"]["user"]["id"].as_i64() {
        payload["review"]["author_association"] =
            json!(access::author_association(pool, repo, user).await?);
    }
    if let Some(id) = payload["release"]["id"].as_i64() {
        if let Some(row) = db::releases::get_release(pool, repo.id, id).await? {
            payload["release"]["assets"] =
                json!(release_assets(pool, repo, owner, &row.release).await?);
        }
    }
    Ok(())
}

async fn topics(pool: &DbPool, repo_id: i64) -> anyhow::Result<Vec<String>> {
    Ok(
        db::repositories::topics::repository_topics(pool, &[repo_id])
            .await?
            .remove(&repo_id)
            .unwrap_or_default(),
    )
}

/// A pull request's issue document has a `pull_request` object and the pull request's id.
fn issue_item(issue: &Value) -> Option<AssignedItem> {
    let id = issue["id"].as_i64()?;
    Some(if issue["pull_request"].is_object() {
        AssignedItem::PullRequest(id)
    } else {
        AssignedItem::Issue(id)
    })
}

async fn comment(
    pool: &DbPool,
    repo: &Repository,
    item: AssignedItem,
    comment: &mut Value,
) -> anyhow::Result<()> {
    if let Some(user) = comment["user"]["id"].as_i64() {
        comment["author_association"] = json!(access::author_association(pool, repo, user).await?);
    }
    if comment.get("performed_via_github_app").is_none() {
        comment["performed_via_github_app"] = Value::Null;
    }
    let (Some(id), Some(url)) = (
        comment["id"].as_i64(),
        comment["url"].as_str().map(str::to_owned),
    ) else {
        return Ok(());
    };
    let subject = match item {
        AssignedItem::Issue(_) => Subject::IssueComment,
        AssignedItem::PullRequest(_) => Subject::PullRequestComment,
    };
    // Conversation comment ids interleave the issue and pull request tables; see
    // `payloads::conversation_comment_id`.
    let counts = db::reactions::reaction_counts(pool, subject.name(), &[id / 2], 0).await?;
    comment["reactions"] = payloads::reaction_rollup(&format!("{url}/reactions"), &counts);
    Ok(())
}

async fn pull_request(
    pool: &DbPool,
    repo: &Repository,
    owner: &str,
    id: i64,
    document: &mut Value,
) -> anyhow::Result<()> {
    let Some(pull) = db::pull_requests::records::get_pull_request_by_id(pool, id)
        .await?
        .filter(|pull| pull.repo_id == repo.id)
    else {
        return Ok(());
    };
    document["author_association"] =
        json!(access::author_association(pool, repo, pull.author_id).await?);
    let base_user = document["base"]["repo"]["owner"].clone();
    document["base"]["label"] = json!(format!("{owner}:{}", pull.base_branch));
    document["base"]["user"] = base_user;
    document["base"]["repo"]["topics"] = json!(topics(pool, repo.id).await?);
    let head = match pull.head_repo_id {
        Some(head_id) if head_id == repo.id => Some((repo.clone(), owner.to_owned())),
        Some(head_id) => {
            match db::repositories::records::get_repository_by_id(pool, head_id).await? {
                Some(head) => db::repositories::records::repository_owner_name(pool, head.id)
                    .await?
                    .map(|name| (head, name)),
                None => None,
            }
        }
        None => None,
    };
    match head {
        Some((head, head_owner)) => {
            let mut value = payloads::repository(&head, &head_owner);
            document["head"]["label"] = json!(format!("{head_owner}:{}", pull.head_branch));
            document["head"]["user"] = value["owner"].clone();
            // A private fork is described only to its own readers. The base repository's
            // readers and hooks get the branch, not the fork's metadata.
            let fork = head.id != repo.id;
            document["head"]["repo"] = if fork && head.is_private {
                Value::Null
            } else {
                value["topics"] = json!(topics(pool, head.id).await?);
                // The fork may have turned private while its topics were read.
                let public = !fork
                    || db::repositories::records::get_repository_by_id(pool, head.id)
                        .await?
                        .is_some_and(|now| !now.is_private);
                if public {
                    value
                } else {
                    Value::Null
                }
            };
        }
        // The fork was deleted.
        None => {
            document["head"]["label"] = Value::Null;
            document["head"]["user"] = Value::Null;
        }
    }
    let labels = db::labels::get_pull_request_labels(pool, pull.id).await?;
    document["labels"] = json!(labels
        .iter()
        .map(|label| payloads::label(label, owner, &repo.name))
        .collect::<Vec<_>>());
    if let Some(milestone) = pull.milestone_id {
        if let Some(summary) = db::milestones::get_milestone_by_id(pool, repo.id, milestone).await?
        {
            let creator = match summary.milestone.creator_id {
                Some(id) => db::users::accounts::get_user_by_id(pool, id).await?,
                None => None,
            };
            document["milestone"] =
                payloads::milestone(&summary, creator.as_ref(), owner, &repo.name);
        }
    }
    let mut assignees = Vec::new();
    for name in db::assignees::list_assignees(pool, AssignedItem::PullRequest(pull.id)).await? {
        if let Some(user) = db::users::accounts::get_user_by_username(pool, &name).await? {
            assignees.push(payloads::user(&user));
        }
    }
    document["assignee"] = assignees.first().cloned().unwrap_or(Value::Null);
    document["assignees"] = json!(assignees);
    let reviewers = db::pull_requests::reviews::list_pull_request_reviewers(pool, pull.id).await?;
    document["requested_reviewers"] =
        json!(reviewers.iter().map(payloads::user).collect::<Vec<_>>());
    let (comments, review_comments) =
        db::pull_requests::reviews::pull_request_comment_counts(pool, pull.id).await?;
    document["comments"] = json!(comments);
    document["review_comments"] = json!(review_comments);
    document["merged_by"] = match pull.merged_by_id {
        Some(id) => db::users::accounts::get_user_by_id(pool, id)
            .await?
            .as_ref()
            .map_or(Value::Null, payloads::user),
        None => Value::Null,
    };
    Ok(())
}

/// The documents of a release's assets whose stored files remain.
pub(crate) async fn release_assets(
    pool: &DbPool,
    repo: &Repository,
    owner: &str,
    release: &Release,
) -> anyhow::Result<Vec<Value>> {
    let mut assets = Vec::new();
    for asset in db::releases::list_release_assets(pool, release.id).await? {
        if let Some(value) = release_asset(pool, owner, &repo.name, release, &asset).await? {
            assets.push(value);
        }
    }
    Ok(assets)
}

/// A release asset as REST and webhooks show it. `None` when its stored file is gone.
pub(crate) async fn release_asset(
    pool: &DbPool,
    owner: &str,
    name: &str,
    release: &Release,
    asset: &ReleaseAsset,
) -> anyhow::Result<Option<Value>> {
    let Some(file) = db::files::get_file_by_id(pool, asset.file_id).await? else {
        return Ok(None);
    };
    let uploader = match file.user_id {
        Some(id) => db::users::accounts::get_user_by_id(pool, id)
            .await?
            .map(|user| payloads::simple_person(&payloads::Person::of(&user))),
        None => None,
    };
    Ok(Some(payloads::release_asset(
        asset,
        release,
        owner,
        name,
        uploader,
        &file.sha256_hex,
    )))
}
