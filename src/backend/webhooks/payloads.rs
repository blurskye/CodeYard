//! Payloads in the shape GitHub documents for each event, as far as this data model has the
//! fields. Fields without a Codeyard counterpart are left out rather than invented.
use crate::backend::api::http::config::{http_clone_base, public_origin};
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
        "clone_url": format!("{}/{owner}/{}.git", http_clone_base().trim_end_matches('/'), repo.name),
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
            "html_url": issue_comment_url(action, comment, issue, repo, owner),
            "created_at": time(comment.created_at),
            "updated_at": time(comment.updated_at),
        },
        "repository": repository(repo, owner),
        "sender": person(sender),
    })
}

/// The issue page resolves `?comment=` to the page that holds the comment. A deleted comment
/// has no page, so it links to the issue.
fn issue_comment_url(
    action: &str,
    comment: &IssueComment,
    issue: &Issue,
    repo: &Repository,
    owner: &str,
) -> String {
    let base = format!(
        "{}/issues/{}",
        repository_url(owner, &repo.name),
        issue.number
    );
    if action == "deleted" {
        base
    } else {
        format!("{base}?comment={0}#comment-{0}", comment.id)
    }
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
        "state": if pull.merged || pull.state == "merged" { "closed" } else { &pull.state },
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

/// A conversation comment on a pull request. GitHub sends it as an issue_comment whose issue is
/// the pull request. `link` is the discussion page and anchor, such as `?page=2#comment-7`.
pub fn pull_request_conversation_comment(
    action: &str,
    comment: &PullRequestComment,
    pull: &PullRequest,
    pull_author: &Person,
    comment_author: &Person,
    link: &str,
    scope: Scope,
) -> Value {
    let Scope {
        repo,
        owner,
        sender,
    } = scope;
    // GitHub marks an issue that is a pull request with a `pull_request` link object.
    let mut issue = pull_request_json(pull, pull_author, repo, owner);
    issue["pull_request"] = json!({ "html_url": issue["html_url"].clone() });
    json!({
        "action": action,
        "issue": issue,
        "comment": {
            "id": comment.id,
            "body": comment.body,
            "user": person(comment_author),
            "html_url": format!("{}/pulls/{}{link}", repository_url(owner, &repo.name), pull.number),
            "created_at": time(comment.created_at),
            "updated_at": time(comment.updated_at),
        },
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
            "html_url": format!("{}/releases/tag/{}", repository_url(owner, &repo.name), crate::webui::encode_component(&release.tag_name)),
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

#[derive(Debug, Default)]
pub struct PushedCommits {
    pub commits: Vec<PushedCommit>,
    pub head: Option<PushedCommit>,
}

fn bounded_text(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn pushed_commit(repository: &git2::Repository, revision: &str) -> Option<PushedCommit> {
    let commit =
        crate::backend::git::repository::peel_to_commit_checked(repository, revision).ok()?;
    let author = commit.author();
    Some(PushedCommit {
        id: commit.id().to_string(),
        message: bounded_text(commit.message().unwrap_or(""), 4096),
        time: commit.time().seconds(),
        author_name: bounded_text(author.name().unwrap_or(""), 256),
        author_email: bounded_text(author.email().unwrap_or(""), 256),
    })
}

/// At most 20 introduced commits, with a separate new head even for a rewind. Git's bounded
/// subprocess avoids libgit2's eager hidden-range walk. Called only on a blocking thread.
pub fn pushed_commits(path: &Path, before: Option<&str>, after: &str) -> PushedCommits {
    use crate::backend::git::repository::process;
    let Ok(repository) = git2::Repository::open_bare(path) else {
        return PushedCommits::default();
    };
    let head = pushed_commit(&repository, after);
    let collect = async {
        let mut command = process::git_command()?;
        command
            .current_dir(path)
            .args(["rev-list", "--max-count=20", after]);
        if let Some(before) = before {
            command.args(["--not", before]);
        }
        command.arg("--");
        let (status, output) =
            process::buffered(command, b"", 20 * 41, std::time::Duration::from_secs(2)).await?;
        anyhow::ensure!(status.success(), "Webhook commit listing failed");
        Ok::<_, anyhow::Error>(output)
    };
    let output = tokio::runtime::Handle::current()
        .block_on(collect)
        .unwrap_or_default();
    let commits = String::from_utf8_lossy(&output)
        .lines()
        .take(20)
        .filter_map(|oid| pushed_commit(&repository, oid))
        .collect();
    PushedCommits { commits, head }
}

fn pushed_commit_json(commit: &PushedCommit, repo: &Repository, owner: &str) -> Value {
    json!({
        "id": commit.id,
        "message": commit.message,
        "timestamp": chrono::DateTime::from_timestamp(commit.time, 0).map(time),
        "url": format!("{}/commit/{}", repository_url(owner, &repo.name), commit.id),
        "author": { "name": commit.author_name, "email": commit.author_email },
    })
}

/// A branch or tag push. `before` is absent for a new ref; the caller sends `create` and
/// `delete` for the ref changes that have no commits to list.
pub fn push(
    reference: &str,
    before: &str,
    after: &str,
    commits: &PushedCommits,
    repo: &Repository,
    owner: &str,
    pusher: &Person,
) -> Value {
    let commit_values: Vec<Value> = commits
        .commits
        .iter()
        .map(|commit| pushed_commit_json(commit, repo, owner))
        .collect();
    let head = commits
        .head
        .as_ref()
        .map(|commit| pushed_commit_json(commit, repo, owner));
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

    #[tokio::test]
    async fn rewinds_keep_the_head_and_large_messages_are_bounded() {
        tokio::task::spawn_blocking(|| {
            let root = tempfile::tempdir().unwrap();
            let repo = git2::Repository::init_bare(root.path()).unwrap();
            let tree = repo
                .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            let signature = git2::Signature::now("Alice", "a@example.test").unwrap();
            let first = repo
                .commit(
                    None,
                    &signature,
                    &signature,
                    &"é".repeat(10_000),
                    &tree,
                    &[],
                )
                .unwrap();
            let parent = repo.find_commit(first).unwrap();
            let second = repo
                .commit(None, &signature, &signature, "second", &tree, &[&parent])
                .unwrap();
            let rewound =
                pushed_commits(root.path(), Some(&second.to_string()), &first.to_string());
            assert!(rewound.commits.is_empty());
            let head = rewound.head.unwrap();
            assert_eq!(head.id, first.to_string());
            assert_eq!(head.message.len(), 4096);
            assert!(head.message.is_char_boundary(head.message.len()));
            let forward =
                pushed_commits(root.path(), Some(&first.to_string()), &second.to_string());
            assert_eq!(forward.commits.len(), 1);
            assert_eq!(forward.head.unwrap().id, second.to_string());
            assert_eq!(bounded_text("aé", 2), "a");
        })
        .await
        .unwrap();
    }

    #[test]
    fn push_marks_created_and_deleted_from_zero_ids() {
        const CHILD: &str = "CODEYARD_CLONE_URL_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("push_marks_created_and_deleted_from_zero_ids")
                .env(CHILD, "1")
                .env("PUBLIC_URL", "https://web.example")
                .env("PUBLIC_HTTP_CLONE_BASE", "https://git.example/")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
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
            &PushedCommits::default(),
            &repo,
            "ann",
            &pusher,
        );
        assert_eq!(value["created"], json!(true));
        assert_eq!(value["deleted"], json!(false));
        assert_eq!(value["repository"]["full_name"], json!("ann/app"));
        assert_eq!(
            value["repository"]["html_url"],
            "https://web.example/ann/app"
        );
        assert_eq!(
            value["repository"]["clone_url"],
            "https://git.example/ann/app.git"
        );
        assert_eq!(value["head_commit"], Value::Null);
    }
}
