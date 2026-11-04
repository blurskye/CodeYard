//! Live default-branch search using the bounded native Git/RPC operations. No independent index.
use crate::backend::cluster::NodeContext;
use crate::backend::repositories::store::repo_store;
use crate::backend::repositories::store::BlobRange;
use crate::backend::repositories::store::CommitFilter;
use crate::backend::repositories::store::CommitInfo;
use crate::backend::repositories::store::RepoStore;

use codeyard_db_repos::catalog_search::Kind;
use codeyard_db_repos::catalog_search::Options;
use codeyard_db_repos::catalog_search::Search;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use codeyard_db_core::models::Repository;
use codeyard_db_repos::discussion_search::Expr;
use codeyard_git as git;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};
use tokio::time::{timeout_at, Instant};
const REPOSITORIES: usize = 250;
const RECORDS: usize = 1001;
const HISTORY: usize = 2000;
const LINES: usize = 5000;
const FILES: usize = 5000;
const MEMORY: usize = 8 * 1024 * 1024;
const REPOSITORY_BYTES: i64 = 512 * 1024 * 1024;
const BUDGET: Duration = Duration::from_secs(8);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindOfSearch {
    Code,
    Commits,
}
#[derive(Clone, Copy)]
pub enum Sort {
    Best,
    Indexed,
    AuthorDate,
    CommitterDate,
}
impl Sort {
    pub fn parse(
        kind: KindOfSearch,
        value: Option<&str>,
    ) -> std::result::Result<Self, &'static str> {
        Ok(match (kind, value.unwrap_or("")) {
            (_, "") => Self::Best,
            (KindOfSearch::Code, "indexed") => Self::Indexed,
            (KindOfSearch::Commits, "author-date") => Self::AuthorDate,
            (KindOfSearch::Commits, "committer-date") => Self::CommitterDate,
            _ => return Err("Invalid search sort"),
        })
    }
}
#[derive(serde::Serialize)]
pub struct TextMatch {
    text: String,
    indices: [usize; 2],
}
pub struct Query {
    kind: KindOfSearch,
    expr: Expr,
    scope: Search,
    file: bool,
    path: bool,
    highlights: Vec<regex::Regex>,
}
impl Query {
    pub fn parse(kind: KindOfSearch, text: &str) -> std::result::Result<Self, &'static str> {
        let mut expr = codeyard_db_repos::discussion_search::expression(text)?;
        let mut fields = Vec::new();
        codeyard_db_repos::catalog_search::extract_fields(&mut expr, &mut fields)?;
        if kind == KindOfSearch::Commits && !fields.is_empty() {
            return Err("Unsupported commit search field");
        }
        if fields
            .iter()
            .any(|f| !matches!(f.as_str(), "file" | "path"))
        {
            return Err("Invalid code search field");
        }
        let (scope, _) = repository_scope(&expr);
        let scope =
            Search::from_expression(Kind::Repositories, scope.unwrap_or(Expr::And(Vec::new())))?
                .with_default_fork_filter();
        let mut words = Vec::new();
        codeyard_db_repos::discussion_search::positive_text(&expr, false, &mut words);
        let words: HashSet<_> = words.into_iter().map(str::to_lowercase).collect();
        let highlights = words
            .into_iter()
            .map(|word| {
                regex::RegexBuilder::new(&regex::escape(&word))
                    .case_insensitive(true)
                    .build()
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "Invalid search terms")?;
        let query = Self {
            kind,
            expr,
            scope,
            highlights,
            file: fields.is_empty() || fields.iter().any(|f| f == "file"),
            path: fields.iter().any(|f| f == "path"),
        };
        query.validate(&query.expr)?;
        Ok(query)
    }
    pub fn text_matches(&self, fragment: &str) -> Vec<TextMatch> {
        let mut matches = Vec::new();
        for pattern in &self.highlights {
            matches.extend(pattern.find_iter(fragment).map(|m| TextMatch {
                text: m.as_str().to_owned(),
                indices: [m.start(), m.end()],
            }));
        }
        matches.sort_by_key(|m| m.indices);
        matches.dedup_by_key(|m| m.indices);
        matches
    }
    pub fn needs_viewer(&self) -> bool {
        self.scope.needs_viewer()
            || codeyard_db_repos::catalog_search::has(&self.expr, "author", Some("@me"))
            || codeyard_db_repos::catalog_search::has(&self.expr, "committer", Some("@me"))
    }
    fn validate(&self, expr: &Expr) -> std::result::Result<(), &'static str> {
        match expr {
            Expr::And(v) | Expr::Or(v) => {
                for e in v {
                    self.validate(e)?;
                }
            }
            Expr::Not(e) => self.validate(e)?,
            Expr::Term(key, value) => {
                if scope_key(key) {
                    Search::from_expression(Kind::Repositories, expr.clone())?;
                    return Ok(());
                }
                match (self.kind, key.as_str()) {
                    // git grep treats a newline in one -e argument as separate alternatives.
                    // Reject multiline phrases rather than returning false-positive files.
                    (KindOfSearch::Code, "text") if value.contains('\n') => {
                        return Err("Code search phrases cannot span lines");
                    }
                    (_, "text") => {}
                    (KindOfSearch::Code, "filename" | "path" | "extension" | "language") => {}
                    (KindOfSearch::Code, "size") => {
                        range_match(0, value, false)?;
                    }
                    (
                        KindOfSearch::Commits,
                        "author" | "committer" | "author-name" | "author-email" | "committer-name"
                        | "committer-email",
                    ) => {}
                    (KindOfSearch::Commits, "author-date" | "committer-date") => {
                        range_match(0, value, true)?;
                    }
                    (KindOfSearch::Commits, "merge")
                        if matches!(value.as_str(), "true" | "false") => {}
                    (KindOfSearch::Commits, "hash" | "parent" | "tree")
                        if value.len() <= 40 && value.bytes().all(|c| c.is_ascii_hexdigit()) => {}
                    _ => return Err("Unsupported content search qualifier"),
                }
            }
        }
        Ok(())
    }
}
fn scope_key(key: &str) -> bool {
    matches!(key, "repo" | "user" | "org" | "fork" | "archived" | "is")
}
/// A necessary (possibly broader) repository condition. Negation is pushed down only when exact.
fn repository_scope(expr: &Expr) -> (Option<Expr>, bool) {
    match expr {
        Expr::Term(k, _) if scope_key(k) => (Some(expr.clone()), true),
        Expr::Term(_, _) => (None, false),
        Expr::Not(v) => {
            let (e, exact) = repository_scope(v);
            if exact {
                (e.map(|e| Expr::Not(Box::new(e))), true)
            } else {
                (None, false)
            }
        }
        Expr::And(v) | Expr::Or(v) => {
            let children = v.iter().map(repository_scope).collect::<Vec<_>>();
            let exact = children.iter().all(|(_, exact)| *exact);
            if matches!(expr, Expr::Or(_)) && children.iter().any(|(e, _)| e.is_none()) {
                return (None, false);
            }
            let parts = children
                .into_iter()
                .filter_map(|(e, _)| e)
                .collect::<Vec<_>>();
            if parts.is_empty() {
                (None, exact)
            } else {
                (
                    Some(if matches!(expr, Expr::And(_)) {
                        Expr::And(parts)
                    } else {
                        Expr::Or(parts)
                    }),
                    exact,
                )
            }
        }
    }
}
pub struct Scope {
    pub repository: Repository,
    pub owner: String,
}
pub struct Fragment {
    pub line: u32,
    pub text: String,
}
pub struct Code {
    pub path: String,
    pub sha: String,
    pub revision: String,
    pub size: u64,
    pub language: Option<&'static str>,
    pub fragments: Vec<Fragment>,
}
pub enum Entry {
    Code(Code),
    Commit(CommitInfo),
}
pub struct Hit {
    pub scope: Arc<Scope>,
    pub score: i64,
    pub entry: Entry,
}
impl Hit {
    fn key(&self) -> &str {
        match &self.entry {
            Entry::Code(c) => &c.path,
            Entry::Commit(c) => &c.sha,
        }
    }
}
#[derive(Default)]
pub struct Results {
    pub items: Vec<Hit>,
    pub incomplete: bool,
}
struct Scan<'a> {
    query: &'a Query,
    viewer: Option<i64>,
    deadline: Instant,
    result: Results,
    bytes: usize,
}
impl Scan<'_> {
    fn full(&self) -> bool {
        self.result.items.len() >= RECORDS
            || self.bytes >= MEMORY
            || Instant::now() >= self.deadline
    }
    fn add(&mut self, scope: &Arc<Scope>, entry: Entry, score: i64) {
        let bytes = match &entry {
            Entry::Code(c) => {
                c.path.len() + c.fragments.iter().map(|f| f.text.len() + 16).sum::<usize>()
            }
            Entry::Commit(c) => {
                c.message.len()
                    + c.author_name.len()
                    + c.author_email.len()
                    + c.committer_name.len()
                    + c.committer_email.len()
                    + c.signature.as_ref().map_or(0, String::len)
                    + c.signature_payload.as_ref().map_or(0, Vec::len)
                    + c.parents.len() * 40
            }
        };
        if self.bytes.saturating_add(bytes) > MEMORY {
            self.result.incomplete = true;
            self.bytes = MEMORY;
            return;
        }
        self.bytes += bytes;
        self.result.items.push(Hit {
            scope: scope.clone(),
            score,
            entry,
        });
    }
    async fn code(&mut self, store: &RepoStore, scope: &Arc<Scope>, tip: &CommitInfo) {
        let mut words = Vec::new();
        all_text(&self.query.expr, &mut words);
        words.sort();
        words.dedup();
        let mut texts = HashMap::<String, TextMatches>::new();
        let mut snippets = HashMap::<String, Vec<Fragment>>::new();
        let mut local_bytes = 0usize;
        if self.query.file {
            for word in &words {
                let Some(hits) = until(
                    self.deadline,
                    store.search_code(&tip.sha, word, 0, LINES + 1),
                )
                .await
                else {
                    self.result.incomplete = true;
                    return;
                };
                let mut complete = hits.len() <= LINES;
                let mut paths = HashSet::new();
                for hit in hits.into_iter().take(LINES) {
                    if !paths.contains(&hit.path) {
                        if local_bytes.saturating_add(hit.path.len() + 32) > MEMORY {
                            complete = false;
                            break;
                        }
                        local_bytes += hit.path.len() + 32;
                        paths.insert(hit.path.clone());
                    }
                    let fragments = snippets.entry(hit.path).or_default();
                    if fragments.len() < 5
                        && !fragments.iter().any(|f| f.line == hit.line_number)
                        && local_bytes.saturating_add(hit.line.len() + 16) <= MEMORY
                    {
                        local_bytes += hit.line.len() + 16;
                        fragments.push(Fragment {
                            line: hit.line_number,
                            text: hit.line,
                        });
                    }
                }
                self.result.incomplete |= !complete;
                texts.insert(word.clone(), TextMatches { paths, complete });
            }
        }
        let paths = if self.query.file && !self.query.path && requires_text(&self.query.expr) {
            let mut positive = Vec::new();
            codeyard_db_repos::discussion_search::positive_text(
                &self.query.expr,
                false,
                &mut positive,
            );
            let mut paths = BTreeSet::new();
            let mut bytes = 0usize;
            for word in positive {
                if let Some(matches) = texts.get(word) {
                    for path in &matches.paths {
                        if !paths.contains(path) {
                            bytes += path.len() + 32;
                            if bytes > MEMORY {
                                self.result.incomplete = true;
                                break;
                            }
                            paths.insert(path.clone());
                        }
                    }
                }
            }
            paths
        } else {
            let Some(files) = until(self.deadline, store.file_paths(&tip.sha, FILES)).await else {
                self.result.incomplete = true;
                return;
            };
            self.result.incomplete |= files.truncated;
            files.paths.into_iter().collect::<BTreeSet<_>>()
        };
        for path in paths {
            if self.full() {
                self.result.incomplete = true;
                break;
            }
            let preliminary = CodeContext {
                scope,
                viewer: self.viewer,
                path: &path,
                size: None,
                language: git::language_for_path(&path),
                texts: &texts,
                file: self.query.file,
                path_text: self.query.path,
            };
            if evaluate(&self.query.expr, &|key, value| preliminary.term(key, value)) == Some(false)
            {
                continue;
            }
            let Some(blob) = until(
                self.deadline,
                store.blob_range(
                    &tip.sha,
                    &path,
                    BlobRange::Window {
                        offset: 0,
                        length: 0,
                    },
                ),
            )
            .await
            else {
                self.result.incomplete = true;
                continue;
            };
            if blob.is_binary {
                continue;
            }
            let language = git::language_for_path(&path);
            let context = CodeContext {
                scope,
                viewer: self.viewer,
                path: &path,
                size: Some(blob.size),
                language,
                texts: &texts,
                file: self.query.file,
                path_text: self.query.path,
            };
            match evaluate(&self.query.expr, &|key, value| context.term(key, value)) {
                Some(true) => {
                    let mut positive = Vec::new();
                    codeyard_db_repos::discussion_search::positive_text(
                        &self.query.expr,
                        false,
                        &mut positive,
                    );
                    let score = positive
                        .into_iter()
                        .filter(|word| context.term("text", word) == Some(true))
                        .count() as i64;
                    self.add(
                        scope,
                        Entry::Code(Code {
                            fragments: snippets.remove(&path).unwrap_or_default(),
                            path,
                            sha: blob.id,
                            revision: tip.sha.clone(),
                            size: blob.size,
                            language,
                        }),
                        score,
                    );
                }
                Some(false) => {}
                None => self.result.incomplete = true,
            }
        }
    }
    async fn commits(
        &mut self,
        pool: &codeyard_db_core::DbPool,
        store: &RepoStore,
        scope: &Arc<Scope>,
        tip: &CommitInfo,
    ) -> Result<()> {
        let needs_people = codeyard_db_repos::catalog_search::has(&self.query.expr, "author", None)
            || codeyard_db_repos::catalog_search::has(&self.query.expr, "committer", None);
        let mut offset = 0usize;
        while offset < HISTORY && !self.full() {
            let Some(commits) = until(
                self.deadline,
                store.commits_until(
                    &tip.sha,
                    offset,
                    100,
                    &CommitFilter::default(),
                    self.deadline.into_std(),
                ),
            )
            .await
            else {
                self.result.incomplete = true;
                return Ok(());
            };
            let exhausted = commits.len() < 100;
            offset += commits.len();
            let people = if needs_people {
                let emails = commits
                    .iter()
                    .flat_map(|c| [c.author_email.clone(), c.committer_email.clone()])
                    .collect::<Vec<_>>();
                crate::backend::repositories::reads::commit_authors(pool, &emails).await?
            } else {
                HashMap::new()
            };
            for commit in commits {
                if self.full() {
                    self.result.incomplete = true;
                    return Ok(());
                }
                if DateTime::from_timestamp(commit.timestamp, 0).is_none()
                    || DateTime::from_timestamp(commit.committer_timestamp, 0).is_none()
                {
                    self.result.incomplete = true;
                    continue;
                }
                let context = CommitContext {
                    scope,
                    viewer: self.viewer,
                    commit: &commit,
                    people: &people,
                };
                if evaluate(&self.query.expr, &|key, value| context.term(key, value)) == Some(true)
                {
                    let mut positive = Vec::new();
                    codeyard_db_repos::discussion_search::positive_text(
                        &self.query.expr,
                        false,
                        &mut positive,
                    );
                    let score = positive
                        .into_iter()
                        .filter(|word| context.term("text", word) == Some(true))
                        .count() as i64;
                    self.add(scope, Entry::Commit(commit), score);
                }
            }
            if exhausted {
                return Ok(());
            }
        }
        self.result.incomplete = true;
        Ok(())
    }
}
struct TextMatches {
    paths: HashSet<String>,
    complete: bool,
}
struct CodeContext<'a> {
    scope: &'a Scope,
    viewer: Option<i64>,
    path: &'a str,
    size: Option<u64>,
    language: Option<&'a str>,
    texts: &'a HashMap<String, TextMatches>,
    file: bool,
    path_text: bool,
}
impl CodeContext<'_> {
    fn term(&self, key: &str, value: &str) -> Option<bool> {
        if scope_key(key) {
            return Some(scope_matches(self.scope, self.viewer, key, value));
        }
        Some(match key {
            "text" => {
                if self.path_text && contains(self.path, value) {
                    return Some(true);
                }
                if !self.file {
                    return Some(false);
                }
                let matches = self.texts.get(value)?;
                if matches.paths.contains(self.path) {
                    true
                } else if matches.complete {
                    false
                } else {
                    return None;
                }
            }
            "path" => contains(self.path, value),
            "filename" => self
                .path
                .rsplit('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case(value)),
            "extension" => std::path::Path::new(self.path)
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case(value.trim_start_matches('.'))),
            "language" => self
                .language
                .is_some_and(|language| language.eq_ignore_ascii_case(value)),
            "size" => range_match(i64::try_from(self.size?).ok()?, value, false).ok()?,
            _ => return None,
        })
    }
}
struct CommitContext<'a> {
    scope: &'a Scope,
    viewer: Option<i64>,
    commit: &'a CommitInfo,
    people: &'a HashMap<String, codeyard_db_core::models::User>,
}
impl CommitContext<'_> {
    fn term(&self, key: &str, value: &str) -> Option<bool> {
        if scope_key(key) {
            return Some(scope_matches(self.scope, self.viewer, key, value));
        }
        let c = self.commit;
        Some(match key {
            "text" => {
                contains(&c.message, value)
                    || ((7..=40).contains(&value.len())
                        && value.bytes().all(|b| b.is_ascii_hexdigit())
                        && c.sha.starts_with(&value.to_ascii_lowercase()))
            }
            "hash" => c.sha.starts_with(&value.to_ascii_lowercase()),
            "parent" => c
                .parents
                .iter()
                .any(|sha| sha.starts_with(&value.to_ascii_lowercase())),
            "tree" => c.tree_sha.starts_with(&value.to_ascii_lowercase()),
            "merge" => (c.parents.len() > 1) == (value == "true"),
            "author-name" => contains(&c.author_name, value),
            "author-email" => contains(&c.author_email, value),
            "committer-name" => contains(&c.committer_name, value),
            "committer-email" => contains(&c.committer_email, value),
            "author" | "committer" => {
                let email = if key == "author" {
                    &c.author_email
                } else {
                    &c.committer_email
                };
                self.people.get(email).is_some_and(|user| {
                    if value == "@me" {
                        Some(user.id) == self.viewer
                    } else {
                        user.username.eq_ignore_ascii_case(value)
                    }
                })
            }
            "author-date" => range_match(c.timestamp, value, true).ok()?,
            "committer-date" => range_match(c.committer_timestamp, value, true).ok()?,
            _ => return None,
        })
    }
}
fn contains(text: &str, value: &str) -> bool {
    text.to_lowercase().contains(&value.to_lowercase())
}
fn scope_matches(scope: &Scope, viewer: Option<i64>, key: &str, value: &str) -> bool {
    let r = &scope.repository;
    match key {
        "repo" => format!("{}/{}", scope.owner, r.name).eq_ignore_ascii_case(value),
        "user" | "org" => {
            (key == "user" || r.owner_kind == "org")
                && if value == "@me" {
                    r.personal_owner_id() == viewer
                } else {
                    scope.owner.eq_ignore_ascii_case(value)
                }
        }
        "fork" => {
            value == "true" || (value == "only" && r.is_fork) || (value == "false" && !r.is_fork)
        }
        "archived" => r.is_archived == (value == "true"),
        "is" => match value {
            "public" => !r.is_private,
            "private" => r.is_private,
            "internal" => false,
            "fork" => r.is_fork,
            "archived" => r.is_archived,
            _ => false,
        },
        _ => false,
    }
}
fn evaluate(expr: &Expr, term: &impl Fn(&str, &str) -> Option<bool>) -> Option<bool> {
    match expr {
        Expr::Term(k, v) => term(k, v),
        Expr::Not(e) => evaluate(e, term).map(|v| !v),
        Expr::And(v) | Expr::Or(v) => {
            let and = matches!(expr, Expr::And(_));
            let mut unknown = false;
            for e in v {
                match evaluate(e, term) {
                    Some(value) if value != and => return Some(!and),
                    Some(_) => {}
                    None => unknown = true,
                }
            }
            if unknown {
                None
            } else {
                Some(and)
            }
        }
    }
}
fn requires_text(expr: &Expr) -> bool {
    match expr {
        Expr::Term(k, _) => k == "text",
        Expr::And(v) => v.iter().any(requires_text),
        Expr::Or(v) => !v.is_empty() && v.iter().all(requires_text),
        Expr::Not(_) => false,
    }
}
fn all_text(expr: &Expr, words: &mut Vec<String>) {
    match expr {
        Expr::Term(k, v) if k == "text" => words.push(v.clone()),
        Expr::And(v) | Expr::Or(v) => {
            for e in v {
                all_text(e, words);
            }
        }
        Expr::Not(e) => all_text(e, words),
        _ => {}
    }
}
async fn until<T, E>(
    deadline: Instant,
    future: impl Future<Output = std::result::Result<T, E>>,
) -> Option<T> {
    timeout_at(deadline, future).await.ok()?.ok()
}

pub async fn search(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    viewer: Option<i64>,
    query: &Query,
    sort: Sort,
    ascending: bool,
) -> Result<Results> {
    let deadline = Instant::now() + BUDGET;
    let (scopes, mut records, owners) = timeout_at(deadline, async {
        let scopes = codeyard_db_repos::catalog_search::search(
            pool,
            viewer,
            &query.scope,
            &Options {
                repository: None,
                sort: Some("updated".into()),
                ascending: false,
                offset: 0,
                limit: REPOSITORIES as i64,
            },
        )
        .await?;
        let ids = scopes
            .items
            .iter()
            .map(|hit| hit.key.parse::<i64>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let records = codeyard_db_repos::repositories::records::get_repositories_by_ids(pool, &ids)
            .await?
            .into_iter()
            .map(|r| (r.id, r))
            .collect::<HashMap<_, _>>();
        let owners =
            codeyard_db_repos::repositories::records::repository_owner_names(pool, &ids).await?;
        Ok::<_, anyhow::Error>((scopes, records, owners))
    })
    .await
    .context("Search scope deadline")??;
    let mut scan = Scan {
        query,
        viewer,
        deadline,
        result: Results {
            incomplete: scopes.total > scopes.items.len() as i64,
            items: Vec::new(),
        },
        bytes: 0,
    };
    let mut bytes = 0i64;
    let mut searched = 0usize;
    for hit in scopes.items {
        if scan.full() {
            scan.result.incomplete = true;
            break;
        }
        let id = hit.key.parse::<i64>()?;
        let Some(repo) = records.remove(&id) else {
            scan.result.incomplete = true;
            continue;
        };
        let owner = owners.get(&id).context("Repository owner missing")?.clone();
        if searched > 0 && bytes.saturating_add(repo.size_bytes) > REPOSITORY_BYTES {
            scan.result.incomplete = true;
            break;
        }
        searched += 1;
        bytes = bytes.saturating_add(repo.size_bytes.max(0));
        let store = repo_store(&repo, cluster);
        let Some(refs) = until(deadline, store.refs()).await else {
            scan.result.incomplete = true;
            continue;
        };
        let Some(revision) = refs.get(&format!("refs/heads/{}", repo.default_branch)) else {
            continue;
        };
        let Some(tip) = until(deadline, store.commit(revision)).await else {
            scan.result.incomplete = true;
            continue;
        };
        let scope = Arc::new(Scope {
            repository: repo,
            owner,
        });
        if query.kind == KindOfSearch::Code {
            scan.code(&store, &scope, &tip).await;
        } else {
            scan.commits(pool, &store, &scope, &tip).await?;
        }
    }
    scan.result.items.sort_by(|a, b| {
        let order = match sort {
            Sort::Best => a.score.cmp(&b.score), // Live indexed ordering follows the last published repository snapshot, not a separate index.
            Sort::Indexed => a
                .scope
                .repository
                .updated_at
                .cmp(&b.scope.repository.updated_at),
            Sort::AuthorDate | Sort::CommitterDate => match (&a.entry, &b.entry) {
                (Entry::Commit(a), Entry::Commit(b)) => {
                    if matches!(sort, Sort::AuthorDate) {
                        a.timestamp.cmp(&b.timestamp)
                    } else {
                        a.committer_timestamp.cmp(&b.committer_timestamp)
                    }
                }
                _ => std::cmp::Ordering::Equal,
            },
        };
        let order = if ascending { order } else { order.reverse() };
        order
            .then_with(|| a.scope.repository.id.cmp(&b.scope.repository.id))
            .then_with(|| a.key().cmp(b.key()))
    });
    Ok(scan.result)
}
fn range_match(actual: i64, value: &str, date: bool) -> std::result::Result<bool, &'static str> {
    if let Some((lo, hi)) = value.split_once("..") {
        if lo == "*" && hi == "*" {
            return Err("Unbounded search range");
        }
        let a = lo == "*" || comparison(actual, ">=", lo, date)?;
        let b = hi == "*" || comparison(actual, "<=", hi, date)?;
        return Ok(a && b);
    }
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(value) = value.strip_prefix(op) {
            return comparison(actual, op, value, date);
        }
    }
    comparison(actual, "=", value, date)
}
fn comparison(
    actual: i64,
    op: &str,
    value: &str,
    date: bool,
) -> std::result::Result<bool, &'static str> {
    let (expected, day) = if date {
        let day = NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .filter(|day| day.format("%Y-%m-%d").to_string() == value);
        let time = day
            .and_then(|day| day.and_hms_opt(0, 0, 0))
            .map(|time| time.and_utc())
            .or_else(|| {
                DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|time| time.with_timezone(&Utc))
            })
            .ok_or("Invalid date search range")?;
        (time.timestamp(), day.is_some())
    } else {
        (
            value
                .parse::<u64>()
                .ok()
                .and_then(|v| i64::try_from(v).ok())
                .ok_or("Invalid numeric search range")?,
            false,
        )
    };
    Ok(match (op, day) {
        ("=", true) => actual >= expected && actual < expected + 86_400,
        ("<=", true) => actual < expected + 86_400,
        (">", true) => actual >= expected + 86_400,
        ("=", _) => actual == expected,
        (">=", _) => actual >= expected,
        ("<=", _) => actual <= expected,
        (">", _) => actual > expected,
        ("<", _) => actual < expected,
        _ => return Err("Invalid range comparison"),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incomplete_negative_terms_never_become_false_positive_matches() {
        let expr = codeyard_db_repos::discussion_search::expression("needle -secret").unwrap();
        assert_eq!(
            evaluate(&expr, &|k, v| if k == "text" && v == "needle" {
                Some(true)
            } else {
                None
            }),
            None
        );
        assert_eq!(
            range_match(1, "2..not-a-number", false),
            Err("Invalid numeric search range")
        );
        assert!(Query::parse(KindOfSearch::Code, "repo:a/b (needle OR filename:main.rs)").is_ok());
        assert!(Query::parse(KindOfSearch::Commits, "author-date:2026-02-30").is_err());
        assert!(Query::parse(KindOfSearch::Code, "\"needle\nmissing\"").is_err());
        assert!(Query::parse(KindOfSearch::Code, "needle\nother").is_ok());
    }
}
