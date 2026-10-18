//! Code and commit search. A search of several repositories walks the repositories the viewer can
//! read, most recently updated first, and shows a few hits from each. The walk stops when a request
//! runs out of time or repository bytes, and a page says how far it got. `repo:owner/name` names one
//! repository, and its pages hold every hit in turn.
use super::{owner_name, search::RESULTS_PER_PAGE, DiscoveryReadError};
use crate::backend::{
    app::AppState,
    db,
    db::repositories::records::RepositorySort,
    git::CommitFilter,
    repositories::access::{self, AccessError},
    repositories::store::{repo_store, ErrorKind, RepoError, RepoStore},
};
use crate::webui::pages::discovery::{CodeHit, CommitHit, SearchScan};
use futures::{stream, StreamExt};
use std::time::Duration;
use tokio::time::{timeout_at, Instant};

/// Hits one repository shows in a walk over several repositories. A repository with more says so.
const HITS_PER_REPOSITORY: usize = 5;
/// Repositories read from the database per step of the walk.
const STEP: i64 = 50;
/// Repository searches a request runs at once. Results are kept in walk order.
const CONCURRENCY: usize = 4;
/// Wall-clock time one request may spend searching.
const TIME_BUDGET: Duration = Duration::from_secs(5);
/// Repository bytes one request may search, summed from the stored sizes. The first repository
/// is always searched, so one larger than the budget can still be found.
const BYTE_BUDGET: i64 = 512 * 1024 * 1024;

/// One page of code or commit hits.
#[derive(Default)]
pub(super) struct ContentPage {
    pub code: Vec<CodeHit>,
    pub commits: Vec<CommitHit>,
    /// The text the hits contain, without the scope words, which the results highlight.
    pub needle: String,
    /// Whether a later page has hits.
    pub has_older: bool,
    /// Set when the budget stopped the walk before its end.
    pub scan: Option<SearchScan>,
    /// Repositories the walk reached that could not be read, such as a storage node that is down.
    pub unsearched: usize,
    /// Repositories with hits on this page that have more matches than the walk shows, as
    /// `owner/name`.
    pub truncated: Vec<String>,
}

enum Hit {
    Code(CodeHit),
    Commit(CommitHit),
}

/// The text to find, and whether it is searched in code or in commit messages.
struct Query<'a> {
    needle: &'a str,
    code: bool,
}

/// The repositories a content search covers.
#[derive(Debug, PartialEq)]
enum Scope {
    /// Every repository the viewer can read.
    All,
    /// The repositories of one owner, as the `user:` or `org:` text the repository search reads.
    Owner(String),
    /// The one repository `owner/name`, matched exactly.
    Repository { owner: String, name: String },
}

impl Scope {
    /// The text the repository search reads for this scope. Only an owner narrows it.
    fn repository_query(&self) -> &str {
        match self {
            Scope::Owner(text) => text,
            Scope::All | Scope::Repository { .. } => "",
        }
    }
}

/// The search text without its `repo:`, `user:` and `org:` words, and the scope those words
/// name. The last scope word wins.
fn split_scope(text: &str) -> (String, Scope) {
    let mut needle = Vec::new();
    let mut scope = Scope::All;
    for word in text.split_whitespace() {
        match word.split_once(':') {
            Some(("repo", name)) if !name.is_empty() => {
                let (owner, name) = name.split_once('/').unwrap_or((name, ""));
                scope = Scope::Repository {
                    owner: owner.to_owned(),
                    name: name.to_owned(),
                };
            }
            Some((kind @ ("user" | "org"), name)) if !name.is_empty() => {
                scope = Scope::Owner(format!("{kind}:{name}"))
            }
            _ => needle.push(word),
        }
    }
    (needle.join(" "), scope)
}

/// Up to `limit` hits of one repository, starting at hit `offset`. A repository with no default
/// branch yet has none, and is not an error; any other failure means it could not be searched.
async fn page_hits(
    store: &RepoStore,
    owner: &str,
    repo: &db::models::Repository,
    query: &Query<'_>,
    offset: usize,
    limit: usize,
    deadline: std::time::Instant,
) -> Result<Vec<Hit>, RepoError> {
    if query.code {
        let matches = store
            .search_code(&repo.default_branch, query.needle, offset, limit)
            .await?;
        return Ok(matches
            .into_iter()
            .map(|hit| {
                Hit::Code(CodeHit {
                    owner: owner.to_owned(),
                    repo: repo.name.clone(),
                    branch: repo.default_branch.clone(),
                    path: hit.path,
                    line_number: hit.line_number,
                    line: hit.line,
                })
            })
            .collect());
    }
    let filter = CommitFilter {
        message: Some(query.needle.to_owned()),
        ..Default::default()
    };
    let commits = store
        .commits_until(&repo.default_branch, offset, limit, &filter, deadline)
        .await?;
    Ok(commits
        .into_iter()
        .map(|commit| {
            Hit::Commit(CommitHit {
                owner: owner.to_owned(),
                repo: repo.name.clone(),
                summary: commit.message.lines().next().unwrap_or_default().to_owned(),
                author: commit.author_name,
                date: chrono::DateTime::from_timestamp(commit.timestamp, 0)
                    .map(|time| time.format("%Y-%m-%d").to_string())
                    .unwrap_or_default(),
                sha: commit.sha,
            })
        })
        .collect())
}

fn keep(content: &mut ContentPage, hit: Hit) {
    match hit {
        Hit::Code(hit) => content.code.push(hit),
        Hit::Commit(hit) => content.commits.push(hit),
    }
}

/// The commit or code hits for `text` on page `page` (1-based). Only repositories the viewer can
/// read are searched, private ones included.
pub(super) async fn search_content(
    state: &AppState,
    viewer: Option<i64>,
    text: &str,
    search_code: bool,
    page: usize,
) -> Result<ContentPage, DiscoveryReadError> {
    let (needle, scope) = split_scope(text);
    if needle.is_empty() {
        return Ok(ContentPage::default());
    }
    let query = Query {
        needle: &needle,
        code: search_code,
    };
    // Hits are numbered from the start of the walk. The page keeps those from `skip` on.
    let skip = page.saturating_sub(1).saturating_mul(RESULTS_PER_PAGE);
    let mut content = match scope {
        Scope::Repository { owner, name } => {
            repository_page(state, viewer, &owner, &name, &query, skip).await?
        }
        scope => walk(state, viewer, &query, &scope, skip).await?,
    };
    content.needle = needle;
    Ok(content)
}

/// One page of the hits of the repository `owner/name`. A repository that does not exist, or that
/// the viewer cannot read, has none.
async fn repository_page(
    state: &AppState,
    viewer: Option<i64>,
    owner: &str,
    name: &str,
    query: &Query<'_>,
    skip: usize,
) -> Result<ContentPage, DiscoveryReadError> {
    let mut content = ContentPage::default();
    let Some(repo) = db::repositories::records::get_repository(&state.pool, owner, name)
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?
    else {
        return Ok(content);
    };
    match access::check_read(&state.pool, repo.id, viewer).await {
        Ok(_) => {}
        Err(AccessError::NotFound) => return Ok(content),
        Err(_) => return Err(DiscoveryReadError::Unavailable),
    }
    let store = repo_store(&repo, state.cluster.as_ref());
    let deadline = Instant::now() + TIME_BUDGET;
    // One hit past the page shows that an older page has hits.
    let outcome = timeout_at(
        deadline,
        page_hits(
            &store,
            owner,
            &repo,
            query,
            skip,
            RESULTS_PER_PAGE + 1,
            deadline.into_std(),
        ),
    )
    .await;
    let mut hits = match outcome {
        Ok(Ok(hits)) => hits,
        Ok(Err(error)) if error.kind == ErrorKind::NotFound => Vec::new(),
        Ok(Err(error)) => {
            // The message can name paths, so the log keeps only the kind of failure.
            tracing::warn!(repository = repo.id, kind = ?error.kind, "Search could not read a repository");
            content.unsearched = 1;
            Vec::new()
        }
        Err(_) => {
            content.unsearched = 1;
            Vec::new()
        }
    };
    content.has_older = hits.len() > RESULTS_PER_PAGE;
    hits.truncate(RESULTS_PER_PAGE);
    for hit in hits {
        keep(&mut content, hit);
    }
    Ok(content)
}

/// The hits of the repositories in `scope`, the first `HITS_PER_REPOSITORY` of each, in walk order.
/// The page keeps the hits from `skip` on.
async fn walk(
    state: &AppState,
    viewer: Option<i64>,
    query: &Query<'_>,
    scope: &Scope,
    skip: usize,
) -> Result<ContentPage, DiscoveryReadError> {
    let mut content = ContentPage::default();
    let deadline = Instant::now() + TIME_BUDGET;
    let end = skip.saturating_add(RESULTS_PER_PAGE);
    let mut position = 0usize;
    let mut kept = Vec::new();
    let mut searched = 0usize;
    let mut admitted = 0usize;
    let mut bytes = 0i64;
    let mut stopped = false;
    let mut offset = 0i64;
    'walk: loop {
        let repos = db::repositories::discovery::search_repositories(
            &state.pool,
            viewer,
            scope.repository_query(),
            RepositorySort::Updated,
            offset,
            STEP,
        )
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
        if repos.is_empty() {
            break;
        }
        offset = offset.saturating_add(i64::try_from(repos.len()).unwrap_or(i64::MAX));
        let ids = repos.iter().map(|repo| repo.id).collect::<Vec<_>>();
        let owners = db::repositories::records::repository_owner_names(&state.pool, &ids)
            .await
            .map_err(|_| DiscoveryReadError::Unavailable)?;
        // A repository is admitted by its stored size before it is searched. The first one always is.
        let mut batch = Vec::with_capacity(repos.len());
        let mut over_budget = false;
        for repo in repos {
            let owner = owner_name(&repo, &owners)?;
            if admitted > 0 && bytes.saturating_add(repo.size_bytes) > BYTE_BUDGET {
                over_budget = true;
                break;
            }
            bytes = bytes.saturating_add(repo.size_bytes);
            admitted += 1;
            batch.push((owner, repo));
        }
        let mut searches = stream::iter(batch)
            .map(|(owner, repo)| async move {
                let store = repo_store(&repo, state.cluster.as_ref());
                let full_name = format!("{owner}/{}", repo.name);
                let outcome = timeout_at(
                    deadline,
                    page_hits(
                        &store,
                        &owner,
                        &repo,
                        query,
                        0,
                        HITS_PER_REPOSITORY + 1,
                        deadline.into_std(),
                    ),
                )
                .await;
                (full_name, repo.id, outcome)
            })
            .buffered(CONCURRENCY);
        while let Some((full_name, repo_id, outcome)) = searches.next().await {
            let Ok(outcome) = outcome else {
                stopped = true;
                break 'walk;
            };
            searched += 1;
            let mut hits = match outcome {
                Ok(hits) => hits,
                // No default branch yet: an empty repository has nothing to find.
                Err(error) if error.kind == ErrorKind::NotFound => Vec::new(),
                Err(error) => {
                    // The message can name paths, so the log keeps only the kind of failure.
                    tracing::warn!(repository = repo_id, kind = ?error.kind, "Search could not read a repository");
                    content.unsearched += 1;
                    Vec::new()
                }
            };
            let more = hits.len() > HITS_PER_REPOSITORY;
            hits.truncate(HITS_PER_REPOSITORY);
            if more && position < end && position + hits.len() > skip {
                content.truncated.push(full_name);
            }
            for hit in hits {
                if position >= end {
                    content.has_older = true;
                    break 'walk;
                }
                if position >= skip {
                    kept.push(hit);
                }
                position += 1;
            }
        }
        if over_budget {
            stopped = true;
            break;
        }
    }
    if stopped {
        let total = db::repositories::discovery::count_repositories(
            &state.pool,
            viewer,
            scope.repository_query(),
            i64::MAX,
        )
        .await
        .map_err(|_| DiscoveryReadError::Unavailable)?;
        content.scan = Some(SearchScan {
            searched,
            total: usize::try_from(total).unwrap_or(usize::MAX),
        });
    }
    for hit in kept {
        keep(&mut content, hit);
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_words_are_removed_from_the_text_and_name_the_scope() {
        assert_eq!(
            split_scope("needle repo:alice/proj more"),
            (
                "needle more".into(),
                Scope::Repository {
                    owner: "alice".into(),
                    name: "proj".into()
                }
            )
        );
        assert_eq!(
            split_scope("user:alice needle"),
            ("needle".into(), Scope::Owner("user:alice".into()))
        );
        assert_eq!(
            split_scope("org:team repo:a/b user:c"),
            ("".into(), Scope::Owner("user:c".into()))
        );
        assert_eq!(split_scope("plain text"), ("plain text".into(), Scope::All));
        // An empty qualifier value is text, not a scope.
        assert_eq!(split_scope("repo: x"), ("repo: x".into(), Scope::All));
        // A repository named without its owner names no repository.
        assert_eq!(
            split_scope("repo:alice needle"),
            (
                "needle".into(),
                Scope::Repository {
                    owner: "alice".into(),
                    name: "".into()
                }
            )
        );
    }
}
