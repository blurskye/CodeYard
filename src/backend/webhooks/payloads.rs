//! Payloads in the shape GitHub documents for each event, as far as this data model has the
//! fields. Fields without a Codeyard counterpart are left out rather than invented.
use crate::backend::api::http::config::public_origin;
use crate::backend::db::models::{
    Issue, IssueComment, PullRequest, PullRequestComment, PullRequestReview, Release, Repository,
    User,
};
use serde_json::{json, Value};
use std::path::Path;

/// The repository an event happened in, and the user who caused it.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub repo: &'a Repository,
    pub owner: &'a str,
    pub sender: &'a Person,
}

/// A user as a payload names them.
#[derive(Debug, Clone)]
pub struct Person {
    pub login: String,
    pub id: i64,
}

impl Person {
    pub fn of(user: &User) -> Self {
        Self {
            login: user.username.clone(),
            id: user.id,
        }
    }
}

fn time(value: chrono::DateTime<chrono::Utc>) -> Value {
    json!(value.to_rfc3339())
}

fn optional_time(value: Option<chrono::DateTime<chrono::Utc>>) -> Value {
    value.map_or(Value::Null, time)
}

pub fn person(person: &Person) -> Value {
    json!({
        "login": person.login,
        "id": person.id,
        "type": "User",
        "html_url": format!("{}/{}", public_origin(), person.login),
    })
}

fn repository_url(owner: &str, name: &str) -> String {
    format!("{}/{owner}/{name}", public_origin())
}

pub fn repository(repo: &Repository, owner: &str) -> Value {
    let full_name = format!("{owner}/{}", repo.name);
    let html_url = repository_url(owner, &repo.name);
    json!({
        "id": repo.id,
        "name": repo.name,
        "full_name": full_name,
        "private": repo.is_private,
        "owner": {
            "login": owner,
            "id": repo.owner_user_id.or(repo.owner_org_id),
            "type": if repo.owner_org_id.is_some() { "Organization" } else { "User" },
        },
        "html_url": html_url,
        "clone_url": format!("{html_url}.git"),
        "description": repo.description,
        "fork": repo.is_fork,
        "archived": repo.is_archived,
        "default_branch": repo.default_branch,
        "stargazers_count": repo.stars_count,
        "watchers_count": repo.watchers_count,
        "forks_count": repo.forks_count,
        "open_issues_count": repo.open_issues_count,
        "created_at": time(repo.created_at),
        "updated_at": time(repo.updated_at),
    })
}

pub fn issues(action: &str, issue: &Issue, author: &Person, scope: Scope) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "issue": issue_json(issue, author, repo, owner),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn issue_comment(
    action: &str,
    comment: &IssueComment,
    issue: &Issue,
    issue_author: &Person,
    comment_author: &Person,
    scope: Scope,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "issue": issue_json(issue, issue_author, repo, owner),
        "comment": {
            "id": comment.id,
            "body": comment.body,
            "user": person(comment_author),
            "html_url": format!("{}/issues/{}#issuecomment-{}", repository_url(owner, &repo.name), issue.number, comment.id),
            "created_at": time(comment.created_at),
            "updated_at": time(comment.updated_at),
        },
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

fn issue_json(issue: &Issue, author: &Person, repo: &Repository, owner: &str) -> Value {
    json!({
        "id": issue.id,
        "number": issue.number,
        "title": issue.title,
        "body": issue.body,
        "state": issue.state,
        "state_reason": issue.state_reason,
        "locked": issue.locked,
        "user": person(author),
        "html_url": format!("{}/issues/{}", repository_url(owner, &repo.name), issue.number),
        "created_at": time(issue.created_at),
        "updated_at": time(issue.updated_at),
        "closed_at": optional_time(issue.closed_at),
    })
}

pub fn pull_request(action: &str, pull: &PullRequest, author: &Person, scope: Scope) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "number": pull.number,
        "pull_request": pull_request_json(pull, author, repo, owner),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

fn pull_request_json(pull: &PullRequest, author: &Person, repo: &Repository, owner: &str) -> Value {
    json!({
        "id": pull.id,
        "number": pull.number,
        "title": pull.title,
        "body": pull.body,
        "state": pull.state,
        "draft": pull.is_draft,
        "merged": pull.merged,
        "merged_at": optional_time(pull.merged_at),
        "merge_commit_sha": pull.merge_commit_sha,
        "user": person(author),
        "head": { "ref": pull.head_branch, "sha": pull.head_sha },
        "base": { "ref": pull.base_branch, "sha": pull.base_sha },
        "html_url": format!("{}/pulls/{}", repository_url(owner, &repo.name), pull.number),
        "additions": pull.additions,
        "deletions": pull.deletions,
        "changed_files": pull.changed_files,
        "created_at": time(pull.created_at),
        "updated_at": time(pull.updated_at),
        "closed_at": optional_time(pull.closed_at),
    })
}

pub fn pull_request_review(
    action: &str,
    review: &PullRequestReview,
    pull: &PullRequest,
    pull_author: &Person,
    reviewer: &Person,
    scope: Scope,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    // GitHub spells the state in lower case, and names a dismissed review "dismissed".
    let state = review.state.to_ascii_lowercase();
    json!({
        "action": action,
        "review": {
            "id": review.id,
            "state": state,
            "body": review.body,
            "commit_id": review.commit_sha,
            "user": person(reviewer),
            "submitted_at": time(review.created_at),
        },
        "pull_request": pull_request_json(pull, pull_author, repo, owner),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn pull_request_review_comment(
    action: &str,
    comment: &PullRequestComment,
    pull: &PullRequest,
    pull_author: &Person,
    comment_author: &Person,
    scope: Scope,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "comment": {
            "id": comment.id,
            "body": comment.body,
            "path": comment.path,
            "line": comment.line,
            "side": comment.side,
            "commit_id": comment.commit_sha,
            "user": person(comment_author),
            "created_at": time(comment.created_at),
            "updated_at": time(comment.updated_at),
        },
        "pull_request": pull_request_json(pull, pull_author, repo, owner),
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn release(action: &str, release: &Release, author: &Person, scope: Scope) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "action": action,
        "release": {
            "id": release.id,
            "tag_name": release.tag_name,
            "target_commitish": release.target_commitish,
            "name": release.name,
            "body": release.body,
            "draft": release.is_draft,
            "prerelease": release.is_prerelease,
            "author": person(author),
            "created_at": time(release.created_at),
            "published_at": optional_time(release.published_at),
            "html_url": format!("{}/releases/tag/{}", repository_url(owner, &repo.name), release.tag_name),
        },
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

/// `create` and `delete` for a branch or tag. `ref_type` is `branch` or `tag`.
pub fn ref_change(ref_type: &str, name: &str, scope: Scope) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    json!({
        "ref": name,
        "ref_type": ref_type,
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

pub fn fork(
    forkee: &Repository,
    forker: &Person,
    repo: &Repository,
    owner: &str,
    forkee_owner: &str,
) -> Value {
    json!({
        "forkee": repository(forkee, forkee_owner),
        "repository": repository(repo, owner),
        "sender": person(forker),
    })
}

/// `created` or `deleted`, for a star. Starred and unstarred follow GitHub's wording.
pub fn star(action: &str, starrer: &Person, repo: &Repository, owner: &str) -> Value {
    json!({
        "action": action,
        "starred_at": chrono::Utc::now().to_rfc3339(),
        "repository": repository(repo, owner),
        "sender": person(starrer),
    })
}

/// One commit of a push, as GitHub lists it.
#[derive(Debug, Clone)]
pub struct PushedCommit {
    pub id: String,
    pub message: String,
    pub time: i64,
    pub author_name: String,
    pub author_email: String,
}

/// The commits a push added: reachable from `after`, not from `before`, newest first and at most
/// 20. Runs git2 and blocks, so it is called from the blocking payload builder.
pub fn pushed_commits(path: &Path, before: Option<&str>, after: &str) -> Vec<PushedCommit> {
    let collect = || -> Result<Vec<PushedCommit>, git2::Error> {
        let repository = git2::Repository::open_bare(path)?;
        let mut walk = repository.revwalk()?;
        walk.push(git2::Oid::from_str(after)?)?;
        if let Some(before) = before {
            walk.hide(git2::Oid::from_str(before)?)?;
        }
        let mut commits = Vec::new();
        for oid in walk.take(20) {
            let commit = repository.find_commit(oid?)?;
            let author = commit.author();
            commits.push(PushedCommit {
                id: commit.id().to_string(),
                message: commit.message().unwrap_or("").to_owned(),
                time: commit.time().seconds(),
                author_name: author.name().unwrap_or("").to_owned(),
                author_email: author.email().unwrap_or("").to_owned(),
            });
        }
        Ok(commits)
    };
    collect().unwrap_or_default()
}

/// A branch or tag push. `before` is absent for a new ref; the caller sends `create` and
/// `delete` for the ref changes that have no commits to list.
pub fn push(
    reference: &str,
    before: &str,
    after: &str,
    commits: &[PushedCommit],
    repo: &Repository,
    owner: &str,
    pusher: &Person,
) -> Value {
    let commit_values: Vec<Value> = commits
        .iter()
        .map(|commit| {
            json!({
                "id": commit.id,
                "message": commit.message,
                "timestamp": chrono::DateTime::from_timestamp(commit.time, 0).map(time),
                "url": format!("{}/commit/{}", repository_url(owner, &repo.name), commit.id),
                "author": { "name": commit.author_name, "email": commit.author_email },
            })
        })
        .collect();
    let head = commit_values.first().cloned().unwrap_or(Value::Null);
    json!({
        "ref": reference,
        "before": before,
        "after": after,
        "created": before.chars().all(|c| c == '0'),
        "deleted": after.chars().all(|c| c == '0'),
        "compare": format!("{}/compare/{before}...{after}", repository_url(owner, &repo.name)),
        "commits": commit_values,
        "head_commit": head,
        "repository": repository(repo, owner),
        "pusher": { "name": pusher.login, "id": pusher.id },
        "sender": person(pusher),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_marks_created_and_deleted_from_zero_ids() {
        let repo = Repository {
            id: 1,
            owner_kind: "user".into(),
            owner_user_id: Some(7),
            owner_org_id: None,
            name: "app".into(),
            description: None,
            website: None,
            is_private: false,
            is_archived: false,
            wiki_enabled: true,
            is_fork: false,
            forked_from_id: None,
            default_branch: "main".into(),
            allow_merge_commit: true,
            allow_squash_merge: true,
            allow_rebase_merge: true,
            disk_path: String::new(),
            storage_node_id: None,
            size_bytes: 0,
            stars_count: 0,
            forks_count: 0,
            watchers_count: 0,
            open_issues_count: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let pusher = Person {
            login: "ann".into(),
            id: 2,
        };
        let zero = "0".repeat(40);
        let value = push(
            "refs/heads/new",
            &zero,
            &"a".repeat(40),
            &[],
            &repo,
            "ann",
            &pusher,
        );
        assert_eq!(value["created"], json!(true));
        assert_eq!(value["deleted"], json!(false));
        assert_eq!(value["repository"]["full_name"], json!("ann/app"));
        assert_eq!(value["head_commit"], Value::Null);
    }
}
