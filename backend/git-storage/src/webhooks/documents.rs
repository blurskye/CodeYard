//! Stored state that GitHub sends with webhook objects: topics, labels, assignees, milestones,
//! counts and reactions. The payload builders run without the database, so `complete` reads
//! these fields before a delivery is recorded and sent.

use codeyard_database::models::Release;
use codeyard_database::models::ReleaseAsset;
use codeyard_database::models::Repository;
use codeyard_database::DbPool;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_db_repos::reactions::Subject;
use serde_json::json;
use serde_json::Value;

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
        let forks =
            codeyard_db_repos::repositories::discovery::public_fork_counts(pool, &[repo.id])
                .await?
                .remove(&repo.id)
                .unwrap_or_default();
        codeyard_webhooks::payloads::set_fork_counts(repository, forks);
        repository["topics"] = json!(topics(pool, repo.id).await?);
    }
    if let Some(item) = issue_item(&payload["issue"]) {
        // A deleted issue has no document. The stored document supplies labels, assignees and
        // the milestone, while the event keeps the fields as its own change left them, even if
        // a later change has since been saved.
        if let Some(mut stored) =
            codeyard_webhooks::issue_documents::webhook_document(pool, repo, owner, item).await
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
            // A transfer committed while the document was read could have swapped in the
            // destination's labels and milestone. A transfer moves the issue and gives it a new
            // number, so the document is kept only if the issue is still here under its number.
            if still_here(pool, repo, item, &payload["issue"]).await? {
                payload["issue"] = stored;
            }
        }
        if payload["comment"].is_object() {
            comment(pool, repo, item, &mut payload["comment"]).await?;
        }
    }
    if let Some(id) = payload["pull_request"]["id"].as_i64() {
        pull_request(pool, repo, owner, id, &mut payload["pull_request"]).await?;
        let repository = payload["repository"].clone();
        if repository.is_object() {
            payload["pull_request"]["base"]["repo"] = repository.clone();
            if payload["pull_request"]["head"]["repo"]["id"].as_i64() == Some(repo.id) {
                payload["pull_request"]["head"]["repo"] = repository;
            }
        }
    }
    // A review comment comes with its pull request and no issue, so the issue branch skipped it.
    if payload["issue"].is_null() && payload["pull_request"].is_object() {
        if let Some(user) = payload["comment"]["user"]["id"].as_i64() {
            payload["comment"]["author_association"] = json!(
                codeyard_identity::repository_access::author_association(pool, repo, user).await?
            );
        }
        if payload["comment"].is_object() {
            review_comment(pool, repo, &mut payload["comment"]).await?;
        }
    }
    if let Some(user) = payload["review"]["user"]["id"].as_i64() {
        payload["review"]["author_association"] = json!(
            codeyard_identity::repository_access::author_association(pool, repo, user).await?
        );
    }
    if let Some(id) = payload["release"]["id"].as_i64() {
        if let Some(row) = codeyard_db_repos::releases::get_release(pool, repo.id, id).await? {
            payload["release"]["assets"] =
                json!(release_assets(pool, repo, owner, &row.release).await?);
        }
    }
    Ok(())
}

async fn topics(pool: &DbPool, repo_id: i64) -> anyhow::Result<Vec<String>> {
    Ok(
        codeyard_db_repos::repositories::topics::repository_topics(pool, &[repo_id])
            .await?
            .remove(&repo_id)
            .unwrap_or_default(),
    )
}

/// Whether `item` is still the issue `event` describes in `repo`. Pull requests do not move.
async fn still_here(
    pool: &DbPool,
    repo: &Repository,
    item: AssignedItem,
    event: &Value,
) -> anyhow::Result<bool> {
    let AssignedItem::Issue(id) = item else {
        return Ok(true);
    };
    Ok(codeyard_db_repos::issues::get_issue_by_id(pool, id)
        .await?
        .is_some_and(|issue| {
            issue.repo_id == repo.id && Some(i64::from(issue.number)) == event["number"].as_i64()
        }))
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
        comment["author_association"] = json!(
            codeyard_identity::repository_access::author_association(pool, repo, user).await?
        );
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
    // `codeyard_types::webhook_ids::conversation_comment_id`.
    let counts =
        codeyard_db_repos::reactions::reaction_counts(pool, subject.name(), &[id / 2], 0).await?;
    comment["reactions"] =
        codeyard_webhooks::payloads::reaction_rollup(&format!("{url}/reactions"), &counts);
    Ok(())
}

/// A review comment's reactions, by the comment document's `id` and `url`.
pub async fn review_comment_reactions(pool: &DbPool, comment: &Value) -> anyhow::Result<Value> {
    let (Some(id), Some(url)) = (comment["id"].as_i64(), comment["url"].as_str()) else {
        return Ok(Value::Null);
    };
    let counts = codeyard_db_repos::reactions::reaction_counts(
        pool,
        Subject::PullRequestComment.name(),
        &[id],
        0,
    )
    .await?;
    Ok(codeyard_webhooks::payloads::reaction_rollup(
        &format!("{url}/reactions"),
        &counts,
    ))
}

/// Fills a review comment's reactions, and its diff hunk and positions from the diff it was
/// written on. The diff is read from this node's copy of the repository; where another node
/// stores it, the hunk stays empty rather than delaying the event.
pub async fn review_comment(
    pool: &DbPool,
    repo: &Repository,
    document: &mut Value,
) -> anyhow::Result<()> {
    let reactions = review_comment_reactions(pool, document).await?;
    document["reactions"] = reactions;
    let Some(id) = document["id"].as_i64() else {
        return Ok(());
    };
    let Some(comment) =
        codeyard_db_repos::pull_requests::reviews::get_pull_request_comment_by_id(pool, id).await?
    else {
        return Ok(());
    };
    let Some(path) = comment.path.as_deref() else {
        return Ok(());
    };
    let Some(pull) = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(
        pool,
        comment.pull_request_id,
    )
    .await?
    .filter(|pull| pull.repo_id == repo.id) else {
        return Ok(());
    };
    let (base, head) = codeyard_webhooks::payloads::review_comment_revisions(&comment, &pull);
    match crate::repositories::store::repo_store(repo, None)
        .diff_file(base, head, path)
        .await
    {
        Ok(diff) => {
            codeyard_webhooks::payloads::review_comment_diff(document, &comment, &pull, &diff.files)
        }
        Err(error) => {
            tracing::warn!(repo_id = repo.id, comment_id = id, %error, "review comment diff could not be read for a webhook")
        }
    }
    Ok(())
}

async fn pull_request(
    pool: &DbPool,
    repo: &Repository,
    owner: &str,
    id: i64,
    document: &mut Value,
) -> anyhow::Result<()> {
    let Some(pull) = codeyard_db_repos::pull_requests::records::get_pull_request_by_id(pool, id)
        .await?
        .filter(|pull| pull.repo_id == repo.id)
    else {
        return Ok(());
    };
    document["author_association"] = json!(
        codeyard_identity::repository_access::author_association(pool, repo, pull.author_id)
            .await?
    );
    let base_user = document["base"]["repo"]["owner"].clone();
    let base_branch = document["base"]["ref"]
        .as_str()
        .unwrap_or(&pull.base_branch)
        .to_owned();
    let head_branch = document["head"]["ref"]
        .as_str()
        .unwrap_or(&pull.head_branch)
        .to_owned();
    document["base"]["label"] = json!(format!("{owner}:{base_branch}"));
    document["base"]["user"] = base_user;
    document["base"]["repo"]["topics"] = json!(topics(pool, repo.id).await?);
    let head = match pull.head_repo_id {
        Some(head_id) if head_id == repo.id => Some((repo.clone(), owner.to_owned())),
        Some(head_id) => {
            match codeyard_db_repos::repositories::records::get_repository_by_id(pool, head_id)
                .await?
            {
                Some(head) => {
                    codeyard_db_repos::repositories::records::repository_owner_name(pool, head.id)
                        .await?
                        .map(|name| (head, name))
                }
                None => None,
            }
        }
        None => None,
    };
    match head {
        Some((head, head_owner)) => {
            let mut value = codeyard_webhooks::payloads::repository(&head, &head_owner);
            document["head"]["label"] = json!(format!("{head_owner}:{head_branch}"));
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
                    || codeyard_db_repos::repositories::records::get_repository_by_id(
                        pool, head.id,
                    )
                    .await?
                    .is_some_and(|now| !now.is_private);
                if public {
                    if fork {
                        let mut payload = json!({"repository":value});
                        codeyard_webhooks::set_open_issues(pool, head.id, &mut payload).await;
                        value = payload["repository"].take();
                    }
                    value
                } else {
                    Value::Null
                }
            };
        }
        // The fork was deleted.
        None => {
            document["head"]["repo"] = Value::Null;
            document["head"]["label"] = json!(head_branch);
            document["head"]["user"] = Value::Null;
        }
    }
    if document["head"]["repo"].is_null() {
        document["head"]["user"] = Value::Null;
        document["head"]["label"] = json!(head_branch);
    }
    let labels =
        codeyard_db_repos::labels::get_pull_request_labels(pool, pull.repo_id, pull.id).await?;
    document["labels"] = json!(labels
        .iter()
        .map(|label| codeyard_webhooks::payloads::label(label, owner, &repo.name))
        .collect::<Vec<_>>());
    if let Some(milestone) = pull.milestone_id {
        if let Some(summary) =
            codeyard_db_repos::milestones::get_milestone_by_id(pool, repo.id, milestone).await?
        {
            let creator = match summary.milestone.creator_id {
                Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(pool, id).await?,
                None => None,
            };
            document["milestone"] = codeyard_webhooks::payloads::milestone(
                &summary,
                creator.as_ref(),
                owner,
                &repo.name,
            );
        }
    }
    let mut assignees = Vec::new();
    for name in
        codeyard_db_repos::assignees::list_assignees(pool, AssignedItem::PullRequest(pull.id))
            .await?
    {
        if let Some(user) =
            codeyard_db_identity::users::accounts::get_user_by_username(pool, &name).await?
        {
            assignees.push(codeyard_webhooks::payloads::user(&user));
        }
    }
    document["assignee"] = assignees.first().cloned().unwrap_or(Value::Null);
    document["assignees"] = json!(assignees);
    let reviewers =
        codeyard_db_repos::pull_requests::reviews::list_pull_request_reviewers(pool, pull.id)
            .await?;
    document["requested_reviewers"] = json!(reviewers
        .iter()
        .map(codeyard_webhooks::payloads::user)
        .collect::<Vec<_>>());
    let teams =
        codeyard_db_repos::pull_requests::team_reviewers::list(pool, repo.id, Some(pull.id), None)
            .await?;
    document["requested_teams"] = json!(teams
        .iter()
        .map(|row| codeyard_webhooks::payloads::team_document(
            &row.team,
            &row.org_slug,
            row.permission
        ))
        .collect::<Vec<_>>());
    let (comments, review_comments) =
        codeyard_db_repos::pull_requests::reviews::pull_request_comment_counts(pool, pull.id)
            .await?;
    document["comments"] = json!(comments);
    document["review_comments"] = json!(review_comments);
    document["auto_merge"] = auto_merge(pool, pull.id).await?;
    document["merged_by"] = match pull.merged_by_id {
        Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(pool, id)
            .await?
            .as_ref()
            .map_or(Value::Null, codeyard_webhooks::payloads::user),
        None => Value::Null,
    };
    Ok(())
}

/// A pull request's `auto_merge` object, or null when auto-merge is not enabled.
pub async fn auto_merge(pool: &DbPool, pull_request_id: i64) -> anyhow::Result<Value> {
    let Some(entry) =
        codeyard_db_repos::pull_requests::auto_merge::get_auto_merge(pool, pull_request_id).await?
    else {
        return Ok(Value::Null);
    };
    let enabled_by =
        codeyard_db_identity::users::accounts::get_user_by_id(pool, entry.request.enabled_by)
            .await?;
    Ok(codeyard_webhooks::payloads::auto_merge(
        &entry.request,
        enabled_by.as_ref(),
    ))
}

/// The documents of a release's assets whose stored files remain.
pub async fn release_assets(
    pool: &DbPool,
    repo: &Repository,
    owner: &str,
    release: &Release,
) -> anyhow::Result<Vec<Value>> {
    Ok(
        codeyard_db_repos::releases::list_asset_files(pool, release.id)
            .await?
            .into_iter()
            .map(|row| {
                let uploader = row.uploader_id.zip(row.uploader_name).map(|(id, login)| {
                    codeyard_webhooks::payloads::simple_person(
                        &codeyard_webhooks::payloads::Person {
                            id,
                            login,
                            site_admin: row.uploader_is_admin.unwrap_or(false),
                        },
                    )
                });
                codeyard_webhooks::payloads::release_asset(
                    &row.asset,
                    release,
                    owner,
                    &repo.name,
                    uploader,
                    &row.sha256_hex,
                )
            })
            .collect(),
    )
}

/// A release asset as REST and webhooks show it. `None` when its stored file is gone.
pub async fn release_asset(
    pool: &DbPool,
    owner: &str,
    name: &str,
    release: &Release,
    asset: &ReleaseAsset,
) -> anyhow::Result<Option<Value>> {
    let Some(file) = codeyard_db_repos::files::get_file_by_id(pool, asset.file_id).await? else {
        return Ok(None);
    };
    let uploader = match file.user_id {
        Some(id) => codeyard_db_identity::users::accounts::get_user_by_id(pool, id)
            .await?
            .map(|user| {
                codeyard_webhooks::payloads::simple_person(
                    &codeyard_webhooks::payloads::Person::of(&user),
                )
            }),
        None => None,
    };
    Ok(Some(codeyard_webhooks::payloads::release_asset(
        asset,
        release,
        owner,
        name,
        uploader,
        &file.sha256_hex,
    )))
}
