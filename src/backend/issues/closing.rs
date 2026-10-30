//! Closing keyword references in pull request descriptions.
use super::db;
use std::{collections::HashMap, sync::LazyLock};

static REFERENCE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s+(?:([a-z0-9_.-]+/[a-z0-9_.-]+)?#([1-9][0-9]*)\b|(https?://[^\s<>]+))").unwrap()
});

pub fn links(
    body: &str,
    same_repo: bool,
    owner: &str,
    repo: &str,
    number: i32,
    base: &str,
) -> bool {
    REFERENCE.captures_iter(body).any(|capture| {
        if let Some(url) = capture.get(3) {
            let expected = format!(
                "{}/{owner}/{repo}/issues/{number}",
                base.trim_end_matches('/')
            );
            return url.as_str().trim_end_matches(['.', ',', ')', ']']) == expected;
        }
        capture[2].parse::<i32>().ok() == Some(number)
            && match capture.get(1) {
                Some(scope) => scope
                    .as_str()
                    .eq_ignore_ascii_case(&format!("{owner}/{repo}")),
                None => same_repo,
            }
    })
}

/// Issue numbers in `body` that a closing keyword names in this repository, without a repeat.
/// They are the numbers `links` accepts: `#N`, `owner/repo#N` and this forge's issue URLs.
/// Qualified targets use the same native closing-keyword and forge-URL grammar.
pub fn referenced_targets(body: &str, owner: &str, repo: &str, base: &str) -> Vec<(String, String, i32)> {
    let prefix = format!("{}/", base.trim_end_matches('/'));
    let mut targets: Vec<_> = REFERENCE.captures_iter(body).filter_map(|capture| {
        let (owner, repo, number) = if let Some(url) = capture.get(3) {
            let parts: Vec<_> = url.as_str().trim_end_matches(['.', ',', ')', ']']).strip_prefix(&prefix)?.split('/').collect();
            if parts.len() != 4 || parts[2] != "issues" { return None; }
            (parts[0], parts[1], parts[3].parse::<i32>().ok()?)
        } else {
            let (owner, repo) = capture.get(1).and_then(|scope| scope.as_str().split_once('/')).unwrap_or((owner, repo));
            (owner, repo, capture.get(2)?.as_str().parse::<i32>().ok()?)
        };
        (number > 0).then(|| (owner.to_ascii_lowercase(), repo.to_ascii_lowercase(), number))
    }).collect();
    targets.sort_unstable(); targets.dedup();
    targets
}

pub fn referenced_numbers(body: &str, owner: &str, repo: &str, base: &str) -> Vec<i32> {
    let scope = format!("{owner}/{repo}");
    let prefix = format!("{}/{owner}/{repo}/issues/", base.trim_end_matches('/'));
    let mut numbers: Vec<i32> = REFERENCE
        .captures_iter(body)
        .filter_map(|capture| {
            if let Some(url) = capture.get(3) {
                return url
                    .as_str()
                    .trim_end_matches(['.', ',', ')', ']'])
                    .strip_prefix(&prefix)?
                    .parse()
                    .ok()
                    .filter(|number| *number > 0);
            }
            capture
                .get(1)
                .is_none_or(|found| found.as_str().eq_ignore_ascii_case(&scope))
                .then(|| capture.get(2)?.as_str().parse().ok())
                .flatten()
        })
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

pub async fn references(
    pool: &db::DbPool,
    viewer: Option<&db::models::User>,
    issue: &db::models::Issue,
    owner: &str,
    name: &str,
    base: &str,
) -> anyhow::Result<Vec<db::models::PullRequest>> {
    let mut result = Vec::new();
    let scope = format!("{owner}/{name}");
    let candidates = db::issues::management::closing_candidates(
        pool,
        viewer.map(|user| user.id),
        issue.repo_id,
        &scope,
        issue.number,
    )
    .await?;
    let mut visible = HashMap::new();
    for pull in candidates {
        if !links(
            &pull.body,
            pull.repo_id == issue.repo_id,
            owner,
            name,
            issue.number,
            base,
        ) {
            continue;
        }
        if !visible.contains_key(&pull.repo_id) {
            let allowed = match db::repositories::records::get_repository_by_id(pool, pull.repo_id).await? {
                Some(repo) => crate::backend::repositories::access::can_read(pool, viewer, &repo).await?,
                None => false,
            };
            visible.insert(pull.repo_id, allowed);
        }
        if visible[&pull.repo_id] {
            result.push(pull);
        }
    }
    Ok(result)
}
