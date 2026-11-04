use crate::api_shared::github::repository;
use crate::app::AppState;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use codeyard_db_repos::assignees::AssignedItem;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
/// Who reads an issue document. Its sub-issue and dependency counts include related issues in
/// other repositories only where this reader may read them; the issue's own repository always
/// counts.
#[derive(Clone, Copy)]
pub enum Reader<'a> {
    /// An API request, within its credential's bounds.
    Request(
        &'a AppState,
        Option<&'a crate::api_shared::github::auth::Authentication>,
    ),
    /// A webhook delivery or an export, which goes wherever the repository's administrators send
    /// it: other repositories count only when they are public.
    Repository,
}

/// The REST issue document that webhook payloads send. `None` when the item is gone.
pub(crate) async fn webhook_document(
    pool: &codeyard_db_core::DbPool,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    item: AssignedItem,
) -> Option<Value> {
    document(pool, Reader::Repository, repo, owner, item)
        .await
        .ok()
}

pub async fn document(
    pool: &codeyard_db_core::DbPool,
    reader: Reader<'_>,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    item: AssignedItem,
) -> Result<Value> {
    documents(pool, reader, repo, owner, &[item])
        .await?
        .pop()
        .ok_or_else(Error::missing)
}

/// The issue documents of `items`, all in `repo`, in the same order. An item that is gone or in
/// another repository is missing.
pub async fn documents(
    pool: &codeyard_db_core::DbPool,
    reader: Reader<'_>,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    items: &[AssignedItem],
) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(items.len());
    for page in items.chunks(100) {
        values.extend(
            documents_page(pool, reader, repo, owner, page)
                .await?
                .into_iter()
                .filter(|value| !value.is_null()),
        );
    }
    // The pages were read by separate queries after the request's access check. Checking the
    // reader's access again afterwards means nothing returned was read after it was lost.
    if let Reader::Request(_, credential) = reader {
        let current = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo.id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        if !codeyard_identity::repository_access::can_read(
            pool,
            credential.map(|auth| &auth.user),
            &current,
        )
        .await
        .map_err(Error::unavailable)?
        {
            return Err(Error::missing());
        }
    }
    Ok(values)
}

/// The ids of each kind, issues first.
pub fn split(items: &[AssignedItem]) -> (Vec<i64>, Vec<i64>) {
    let mut issues = Vec::new();
    let mut pulls = Vec::new();
    for item in items {
        match *item {
            AssignedItem::Issue(id) => issues.push(id),
            AssignedItem::PullRequest(id) => pulls.push(id),
        }
    }
    (issues, pulls)
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Up to 100 documents. Each kind of metadata is read in one batch for the whole page.
pub async fn documents_page(
    pool: &codeyard_db_core::DbPool,
    reader: Reader<'_>,
    repo: &codeyard_db_core::models::Repository,
    owner: &str,
    items: &[AssignedItem],
) -> Result<Vec<Value>> {
    let (issue_ids, pull_ids) = split(items);
    let issues: HashMap<i64, codeyard_db_core::models::Issue> =
        codeyard_db_repos::issues::get_issues_by_ids(pool, repo.id, &issue_ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|issue| (issue.id, issue))
            .collect();
    let pulls: HashMap<i64, codeyard_db_core::models::PullRequest> =
        codeyard_db_repos::pull_requests::records::get_pull_requests_by_ids(
            pool, repo.id, &pull_ids,
        )
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|pull| (pull.id, pull))
        .collect();
    let found = |item: &AssignedItem| match *item {
        AssignedItem::Issue(id) => issues.contains_key(&id),
        AssignedItem::PullRequest(id) => pulls.contains_key(&id),
    };
    let milestones: HashMap<i64, codeyard_db_core::models::MilestoneSummary> =
        codeyard_db_repos::milestones::get_milestones_by_ids(
            pool,
            repo.id,
            &sorted(
                issues
                    .values()
                    .filter_map(|issue| issue.milestone_id)
                    .chain(pulls.values().filter_map(|pull| pull.milestone_id))
                    .collect(),
            ),
        )
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|summary| (summary.milestone.id, summary))
        .collect();
    // People: authors, closers and milestone creators by id, assignees by name, each read once.
    let users: HashMap<i64, codeyard_db_core::models::User> =
        codeyard_db_identity::users::accounts::get_users_by_ids(
            pool,
            &sorted(
                issues
                    .values()
                    .flat_map(|issue| [Some(issue.author_id), issue.closed_by_id])
                    .chain(
                        pulls
                            .values()
                            .flat_map(|pull| [Some(pull.author_id), pull.closed_by_id]),
                    )
                    .flatten()
                    .chain(
                        milestones
                            .values()
                            .filter_map(|summary| summary.milestone.creator_id),
                    )
                    .collect(),
            ),
        )
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|user| (user.id, user))
        .collect();
    let mut assigned: HashMap<(bool, i64), Vec<String>> = HashMap::new();
    for (pull, ids) in [(false, &issue_ids), (true, &pull_ids)] {
        if ids.is_empty() {
            continue;
        }
        for (id, name) in codeyard_db_repos::assignees::list_item_assignees(pool, pull, ids)
            .await
            .map_err(Error::unavailable)?
        {
            assigned.entry((pull, id)).or_default().push(name);
        }
    }
    let mut names: Vec<String> = assigned.values().flatten().cloned().collect();
    names.sort_unstable();
    names.dedup();
    let assignees: HashMap<String, codeyard_db_core::models::User> = if names.is_empty() {
        HashMap::new()
    } else {
        codeyard_db_identity::users::accounts::get_users_by_usernames(pool, &names)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|user| (user.username.clone(), user))
            .collect()
    };
    let authors = sorted(
        issues
            .values()
            .map(|issue| issue.author_id)
            .chain(pulls.values().map(|pull| pull.author_id))
            .collect(),
    );
    let associations =
        codeyard_identity::repository_access::author_associations(pool, repo, &authors)
            .await
            .map_err(Error::unavailable)?;
    let issue_labels = codeyard_db_repos::labels::get_issues_labels(pool, repo.id, &issue_ids)
        .await
        .map_err(Error::unavailable)?;
    let pull_labels = codeyard_db_repos::labels::get_pull_requests_labels(pool, repo.id, &pull_ids)
        .await
        .map_err(Error::unavailable)?;
    let issue_comments: HashMap<i64, i64> =
        codeyard_db_repos::issues::count_issue_comments(pool, &issue_ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .collect();
    let pull_comments: HashMap<i64, i64> =
        codeyard_db_repos::pull_requests::reviews::count_pull_request_conversation_comments_for(
            pool, &pull_ids,
        )
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .collect();
    let issue_reactions = codeyard_db_repos::reactions::reaction_counts(
        pool,
        codeyard_db_repos::reactions::Subject::Issue.name(),
        &issue_ids,
        0,
    )
    .await
    .map_err(Error::unavailable)?;
    let pull_reactions = codeyard_db_repos::reactions::reaction_counts(
        pool,
        codeyard_db_repos::reactions::Subject::PullRequest.name(),
        &pull_ids,
        0,
    )
    .await
    .map_err(Error::unavailable)?;
    let types: HashMap<i64, codeyard_db_repos::issue_attributes::Attribute> =
        codeyard_db_repos::issue_attributes::assigned_types(pool, repo.id, &issue_ids)
            .await
            .map_err(Error::unavailable)?
            .into_iter()
            .map(|row| (row.issue_id, row.attribute))
            .collect();
    let summaries = relationship_summaries(pool, reader, repo, &issue_ids).await?;
    let mut pins: HashMap<(bool, i64), codeyard_db_repos::pins::Pin> = HashMap::new();
    for (pull, ids) in [(false, &issue_ids), (true, &pull_ids)] {
        for row in codeyard_db_repos::pins::get_pins(pool, pull, ids)
            .await
            .map_err(Error::unavailable)?
        {
            pins.insert((pull, row.item_id), row.pin);
        }
    }
    let person = |id: i64| -> Result<crate::webhooks::payloads::Person> {
        users
            .get(&id)
            .map(crate::webhooks::payloads::Person::of)
            .ok_or_else(|| Error::unavailable(()))
    };
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        if !found(item) {
            values.push(Value::Null);
            continue;
        }
        let (pull, id) = match *item {
            AssignedItem::Issue(id) => (false, id),
            AssignedItem::PullRequest(id) => (true, id),
        };
        let (mut value, author, milestone, closed_by, labels, comments, reactions) = if pull {
            let pull = &pulls[&id];
            (
                crate::webhooks::payloads::pull_request_issue(
                    pull,
                    &person(pull.author_id)?,
                    repo,
                    owner,
                ),
                pull.author_id,
                pull.milestone_id,
                pull.closed_by_id,
                pull_labels.get(&id),
                pull_comments.get(&id),
                &pull_reactions,
            )
        } else {
            let issue = &issues[&id];
            (
                crate::webhooks::payloads::issue_json(
                    issue,
                    &person(issue.author_id)?,
                    repo,
                    owner,
                ),
                issue.author_id,
                issue.milestone_id,
                issue.closed_by_id,
                issue_labels.get(&id),
                issue_comments.get(&id),
                &issue_reactions,
            )
        };
        value["user"] = crate::webhooks::payloads::user(&users[&author]);
        value["author_association"] =
            json!(associations.get(&author).map_or("NONE", String::as_str));
        value["closed_by"] = closed_by
            .and_then(|id| users.get(&id))
            .map_or(Value::Null, crate::webhooks::payloads::user);
        value["labels"] = json!(labels
            .into_iter()
            .flatten()
            .map(|label| crate::webhooks::payloads::label(label, owner, &repo.name))
            .collect::<Vec<_>>());
        value["comments"] = json!(comments.copied().unwrap_or(0));
        value["milestone"] =
            milestone
                .and_then(|id| milestones.get(&id))
                .map_or(Value::Null, |summary| {
                    let creator = summary.milestone.creator_id.and_then(|id| users.get(&id));
                    crate::webhooks::payloads::milestone(summary, creator, owner, &repo.name)
                });
        let item_assignees: Vec<Value> = assigned
            .get(&(pull, id))
            .into_iter()
            .flatten()
            .filter_map(|name| assignees.get(name))
            .map(crate::webhooks::payloads::user)
            .collect();
        value["assignee"] = item_assignees.first().cloned().unwrap_or(Value::Null);
        value["assignees"] = json!(item_assignees);
        let url = value["url"]
            .as_str()
            .ok_or_else(|| Error::unavailable(()))?
            .to_owned();
        let counts: Vec<codeyard_db_repos::reactions::ReactionCount> = reactions
            .iter()
            .filter(|count| count.subject_id == id)
            .cloned()
            .collect();
        value["reactions"] =
            crate::webhooks::payloads::reaction_rollup(&format!("{url}/reactions"), &counts);
        if !pull {
            value["type"] = types
                .get(&id)
                .map_or(Value::Null, crate::webhooks::payloads::issue_type);
            let summary = summaries.get(&id).copied().unwrap_or_default();
            value["sub_issues_summary"] = json!({
                "total": summary.total,
                "completed": summary.completed,
                "percent_completed": if summary.total == 0 { 0 } else { summary.completed * 100 / summary.total },
            });
            value["issue_dependencies_summary"] = json!({
                "blocked_by": summary.blocked_by,
                "blocking": summary.blocking,
                "total_blocked_by": summary.total_blocked_by,
                "total_blocking": summary.total_blocking,
            });
        }
        value["pinned_comment"] = match pins.get(&(pull, id)) {
            Some(pin) => {
                let comment_id =
                    codeyard_model::webhook_ids::conversation_comment_id(pin.comment_id, pull);
                let comment =
                    crate::api_shared::github::comments::find(pool, repo.id, comment_id).await?;
                crate::api_shared::github::comments::document(pool, repo, owner, &comment).await?
            }
            None => Value::Null,
        };
        values.push(value);
    }
    // Nothing read after an earlier ACL snapshot may escape through a now-private repository.
    if let Reader::Request(state, credential) = reader {
        for (pull, ids, scope) in [
            (false, &issue_ids, "issues"),
            (true, &pull_ids, "pull-requests"),
        ] {
            if !ids.is_empty()
                && !crate::api_shared::github::auth::readable_repositories(
                    state,
                    credential,
                    &[repo.id],
                    scope,
                )
                .await?
                .contains(&repo.id)
            {
                for (value, item) in values.iter_mut().zip(items) {
                    if matches!(item, AssignedItem::PullRequest(_)) == pull {
                        *value = Value::Null;
                    }
                }
            }
        }
    }
    Ok(values)
}

/// Sub-issue and dependency counts of one issue.
#[derive(Clone, Copy, Default)]
struct Summary {
    total: i64,
    completed: i64,
    blocked_by: i64,
    blocking: i64,
    total_blocked_by: i64,
    total_blocking: i64,
}

/// The relationship counts of `issue_ids`, all in `repo`, from the related issues the reader may
/// read.
async fn relationship_summaries(
    pool: &codeyard_db_core::DbPool,
    reader: Reader<'_>,
    repo: &codeyard_db_core::models::Repository,
    issue_ids: &[i64],
) -> Result<HashMap<i64, Summary>> {
    let mut rows = Vec::new();
    for chunk in issue_ids.chunks(100) {
        rows.extend(
            codeyard_db_identity::relationships::summaries(pool, chunk)
                .await
                .map_err(Error::unavailable)?,
        );
    }
    let mut others: Vec<i64> = rows
        .iter()
        .map(|row| row.repo_id)
        .filter(|id| *id != repo.id)
        .collect();
    others.sort_unstable();
    others.dedup();
    let readable: std::collections::HashSet<i64> = match reader {
        Reader::Request(state, credential) => {
            crate::api_shared::github::auth::readable_repositories(
                state, credential, &others, "issues",
            )
            .await?
        }
        Reader::Repository => {
            let mut public = std::collections::HashSet::new();
            for chunk in others.chunks(100) {
                public.extend(
                    codeyard_db_repos::repositories::access::readable_repository_ids(
                        pool, None, chunk,
                    )
                    .await
                    .map_err(Error::unavailable)?,
                );
            }
            public
        }
    };
    let mut summaries: HashMap<i64, Summary> = HashMap::new();
    for row in rows {
        if row.repo_id != repo.id && !readable.contains(&row.repo_id) {
            continue;
        }
        let summary = summaries.entry(row.root).or_default();
        summary.total += row.total;
        summary.completed += row.completed;
        summary.blocked_by += row.blocked_by;
        summary.blocking += row.blocking;
        summary.total_blocked_by += row.total_blocked_by;
        summary.total_blocking += row.total_blocking;
    }
    Ok(summaries)
}

pub async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let item =
        crate::api_shared::github::labels::item(&state.pool, repo.id, &p["issue_number"]).await?;
    Ok(Json(
        document(
            &state.pool,
            Reader::Request(&state, auth.as_ref()),
            &repo,
            &p["owner"],
            item,
        )
        .await?,
    ))
}

pub fn assignee_names(names: Vec<String>) -> Result<Vec<String>> {
    let mut unique = std::collections::BTreeSet::new();
    for name in names {
        unique.insert(name);
        if unique.len() > codeyard_db_repos::assignees::MAX_ASSIGNEES as usize {
            return Err(Error::invalid("Too many assignees"));
        }
    }
    Ok(unique.into_iter().collect())
}

pub async fn resolve_assignees(
    pool: &codeyard_db_core::DbPool,
    names: &[String],
) -> Result<Vec<codeyard_db_core::models::User>> {
    let users = codeyard_db_identity::users::accounts::get_users_by_usernames(pool, names)
        .await
        .map_err(Error::unavailable)?;
    if users.len() != names.len() {
        return Err(Error::invalid("User cannot be assigned"));
    }
    Ok(users)
}

#[derive(Default)]
pub struct Metadata {
    pub issue_type: Option<Option<i64>>,
    pub labels: Option<Vec<i64>>,
    pub assignees: Option<Vec<codeyard_db_core::models::User>>,
    pub milestone: Option<Option<i32>>,
}

impl Metadata {
    pub fn creation(&self) -> codeyard_db_repos::issues::OwnedIssueMetadata {
        codeyard_db_repos::issues::OwnedIssueMetadata {
            labels: self.labels.clone().unwrap_or_default(),
            assignees: self
                .assignees
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|user| user.id)
                .collect(),
            milestone: self.milestone.flatten(),
            issue_type: self.issue_type.flatten(),
            ..Default::default()
        }
    }
}

pub async fn metadata(
    pool: &codeyard_db_core::DbPool,
    repo: &codeyard_db_core::models::Repository,
    actor: &codeyard_db_core::models::User,
    input: &Value,
) -> Result<Metadata> {
    if !codeyard_identity::repository_access::can_triage(pool, actor, repo)
        .await
        .map_err(Error::unavailable)?
    {
        return Ok(Metadata::default());
    }
    let mut result = Metadata::default();
    if let Some(value) = input.get("type") {
        result.issue_type = Some(if value.is_null() {
            None
        } else {
            let name = value
                .as_str()
                .ok_or_else(|| Error::invalid("Invalid issue type"))?;
            Some(
                codeyard_db_repos::issue_attributes::catalog(pool, repo.id, Some(actor.id), "type")
                    .await
                    .map_err(Error::unavailable)?
                    .into_iter()
                    .find(|a| a.is_enabled && a.name.to_lowercase() == name.to_lowercase())
                    .ok_or_else(|| Error::invalid("Issue type does not exist"))?
                    .id,
            )
        });
    }
    if let Some(labels) = input.get("labels") {
        let labels = labels
            .as_array()
            .ok_or_else(|| Error::invalid("Invalid labels"))?;
        if labels.len() > codeyard_db_repos::labels::MAX_ISSUE_LABELS {
            return Err(Error::invalid("Too many labels"));
        }
        let catalogue = codeyard_db_repos::labels::list_repo_labels(pool, repo.id)
            .await
            .map_err(Error::unavailable)?;
        let mut ids = Vec::new();
        for label in labels {
            let found = catalogue.iter().find(|candidate| {
                label
                    .as_str()
                    .or_else(|| label["name"].as_str())
                    .is_some_and(|name| candidate.name.eq_ignore_ascii_case(name))
                    || label["id"].as_i64() == Some(candidate.id)
            });
            ids.push(
                found
                    .ok_or_else(|| Error::invalid("Label does not exist"))?
                    .id,
            );
        }
        result.labels = Some(ids);
    }
    if input.get("assignees").is_some() || input.get("assignee").is_some() {
        let names: Vec<String> = if let Some(names) = input.get("assignees") {
            serde_json::from_value(names.clone())
                .map_err(|_| Error::invalid("Invalid assignees"))?
        } else {
            match input.get("assignee").and_then(Value::as_str) {
                Some(name) => vec![name.to_owned()],
                None => Vec::new(),
            }
        };
        let names = assignee_names(names)?;
        let users = resolve_assignees(pool, &names).await?;
        let ids: Vec<_> = users.iter().map(|user| user.id).collect();
        if codeyard_db_repos::assignees::assignable_user_ids(pool, repo.id, &ids)
            .await
            .map_err(Error::unavailable)?
            .len()
            != ids.len()
        {
            return Err(Error::invalid("User cannot be assigned"));
        }
        result.assignees = Some(users);
    }
    if let Some(number) = input.get("milestone") {
        result.milestone = Some(if number.is_null() {
            None
        } else {
            let number = number
                .as_i64()
                .or_else(|| number.as_str().and_then(|number| number.parse().ok()))
                .and_then(|number| i32::try_from(number).ok())
                .ok_or_else(|| Error::invalid("Invalid milestone"))?;
            codeyard_db_repos::milestones::get_milestone(pool, repo.id, number)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(|| Error::invalid("Milestone does not exist"))?;
            Some(number)
        });
    }
    Ok(result)
}
