//! The global search index holds every cell's repository, issue, pull request and code
//! documents. A cell writes only its own repositories' documents, through the outbox, and
//! names who may read a private one. Search reads the index instead of asking every cell.
use crate::{pool::PgPool, DbPool};
use anyhow::{ensure, Result};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub const TOPIC: &str = "search";
pub const MAXIMUM_READERS: usize = 10_000;
pub const MAXIMUM_DOCUMENT_BYTES: usize = 262_144;
pub const MAXIMUM_FILE_BYTES: usize = 131_072;
pub const MAXIMUM_FILES: usize = 2_000;
pub const MAXIMUM_CHUNK_BYTES: usize = 2 * 1024 * 1024;
pub const MAXIMUM_CODE_BYTES: usize = 16 * 1024 * 1024;
pub const MAXIMUM_PAGE: u32 = 100;
pub const MAXIMUM_OFFSET: u32 = 1000;
/// Packages a public repository depends on or provides, for "used by".
pub const MAXIMUM_PACKAGES: usize = 2_000;
const MAXIMUM_PACKAGE_NAME_BYTES: usize = 256;
const MAXIMUM_TERMS: usize = 32;
const MAXIMUM_DEPTH: usize = 8;
const MAXIMUM_VALUE_BYTES: usize = 256;
const MAXIMUM_ID: i64 = 9_007_199_254_740_991;

/// What one outbox event names: the repository itself, or one of its issues or pull requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "snake_case", deny_unknown_fields)]
pub enum Item {
    Repository,
    Issue { id: i64 },
    Pull { id: i64 },
}
impl Item {
    pub fn validate(self) -> Result<()> {
        match self {
            Self::Repository => Ok(()),
            Self::Issue { id } | Self::Pull { id } => valid_id(id),
        }
    }
}

/// Orders the writes for one document: a later event, or a later claim of the same event, wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub event: i64,
    pub attempt: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: String,
    pub sha: String,
    pub size: i64,
    pub content: String,
}

/// A package a repository depends on, or one it provides when `provided` is set.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub ecosystem: String,
    pub name: String,
    pub provided: bool,
}
impl Package {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=32).contains(&self.ecosystem.len())
                && self
                    .ecosystem
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".+-".contains(&b))
                && (1..=MAXIMUM_PACKAGE_NAME_BYTES).contains(&self.name.len())
                && !self.name.contains('\0')
                && self.name == self.name.to_lowercase(),
            "Invalid search package"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Batch {
    /// `document` is `None` once the repository is gone. `packages` is empty for a private one.
    Repository {
        id: i64,
        version: Version,
        document: Option<Value>,
        public: bool,
        classic_allowed: bool,
        readers: Vec<i64>,
        #[serde(default)]
        packages: Vec<Package>,
    },
    Issue {
        id: i64,
        pull: bool,
        repository_id: i64,
        version: Version,
        document: Option<Value>,
    },
    /// The default branch's files, sent after the repository document of the same version.
    Code {
        repository_id: i64,
        version: Version,
        head: String,
        files: Vec<File>,
        first: bool,
        last: bool,
    },
}
fn valid_id(id: i64) -> Result<()> {
    ensure!((1..=MAXIMUM_ID).contains(&id), "Invalid search object");
    Ok(())
}
fn valid_document(document: &Option<Value>) -> Result<()> {
    if let Some(document) = document {
        ensure!(
            document.is_object() && serde_json::to_vec(document)?.len() <= MAXIMUM_DOCUMENT_BYTES,
            "Invalid search document"
        );
    }
    Ok(())
}
fn valid_head(head: &str) -> bool {
    matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit())
}
impl Batch {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Repository {
                id,
                version,
                document,
                readers,
                packages,
                ..
            } => {
                valid_id(*id)?;
                valid_id(version.event)?;
                valid_document(document)?;
                ensure!(readers.len() <= MAXIMUM_READERS, "Too many search readers");
                for reader in readers {
                    valid_id(*reader)?;
                }
                ensure!(
                    packages.len() <= MAXIMUM_PACKAGES,
                    "Too many search packages"
                );
                for package in packages {
                    package.validate()?;
                }
            }
            Self::Issue {
                id,
                repository_id,
                version,
                document,
                ..
            } => {
                valid_id(*id)?;
                valid_id(*repository_id)?;
                valid_id(version.event)?;
                valid_document(document)?;
            }
            Self::Code {
                repository_id,
                version,
                head,
                files,
                ..
            } => {
                valid_id(*repository_id)?;
                valid_id(version.event)?;
                ensure!(valid_head(head), "Invalid search code head");
                let mut bytes = 0;
                for file in files {
                    ensure!(
                        !file.path.is_empty()
                            && file.path.len() <= 4096
                            && !file.path.contains('\0')
                            && valid_head(&file.sha)
                            && file.size >= 0
                            && file.content.len() <= MAXIMUM_FILE_BYTES
                            && !file.content.contains('\0'),
                        "Invalid search code file"
                    );
                    bytes += file.path.len() + file.content.len();
                }
                ensure!(
                    files.len() <= MAXIMUM_FILES && bytes <= MAXIMUM_CHUNK_BYTES,
                    "Search code chunk is too large"
                );
            }
        }
        Ok(())
    }
}

/// The index's reply to a write: which default-branch head it holds code for.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Indexed {
    pub code_head: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Repositories,
    Issues,
    Code,
}

/// A parsed GitHub search query. `Term("text", word)` is free text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Filter {
    All(Vec<Filter>),
    Any(Vec<Filter>),
    Not(Box<Filter>),
    Term(String, String),
}
impl Filter {
    /// One rule for the cell that sends a query and the index that runs it.
    pub fn validate(&self, kind: Kind) -> Result<(), &'static str> {
        fn walk(
            filter: &Filter,
            kind: Kind,
            depth: usize,
            terms: &mut usize,
        ) -> Result<(), &'static str> {
            if depth > MAXIMUM_DEPTH {
                return Err("Search expression is too deep");
            }
            match filter {
                Filter::All(items) | Filter::Any(items) => {
                    for item in items {
                        walk(item, kind, depth + 1, terms)?;
                    }
                    Ok(())
                }
                Filter::Not(item) => walk(item, kind, depth + 1, terms),
                Filter::Term(key, value) => {
                    *terms += 1;
                    if *terms > MAXIMUM_TERMS {
                        return Err("Search query has too many terms");
                    }
                    if value.is_empty() || value.len() > MAXIMUM_VALUE_BYTES || value.contains('\0')
                    {
                        return Err("Invalid search value");
                    }
                    term_allowed(kind, key, value)
                }
            }
        }
        walk(self, kind, 0, &mut 0)
    }
    /// Words that raise a document's score.
    pub fn positive_text(&self) -> Vec<&str> {
        fn walk<'a>(filter: &'a Filter, negative: bool, words: &mut Vec<&'a str>) {
            match filter {
                Filter::All(items) | Filter::Any(items) => {
                    items.iter().for_each(|item| walk(item, negative, words))
                }
                Filter::Not(item) => walk(item, !negative, words),
                Filter::Term(key, value) if key == "text" && !negative => words.push(value),
                Filter::Term(..) => {}
            }
        }
        let mut words = Vec::new();
        walk(self, false, &mut words);
        words
    }
    /// Repositories that a top-level `repo:` term confines the query to, if every match must be in one of them.
    pub fn confining_repositories(&self) -> Option<Vec<&str>> {
        match self {
            Self::Term(key, value) if key == "repo" => Some(vec![value.as_str()]),
            Self::All(items) => items.iter().find_map(Self::confining_repositories),
            Self::Any(items) if !items.is_empty() => {
                let mut all = Vec::new();
                for item in items {
                    all.extend(item.confining_repositories()?);
                }
                Some(all)
            }
            _ => None,
        }
    }
}
fn term_allowed(kind: Kind, key: &str, value: &str) -> Result<(), &'static str> {
    let boolean = matches!(value, "true" | "false");
    let allowed = match (kind, key) {
        (_, "text" | "repo" | "user" | "org" | "owner" | "in") => true,
        (Kind::Repositories, "language" | "topic") => true,
        (Kind::Repositories, "fork") => boolean || value == "only",
        (Kind::Repositories, "archived") => boolean,
        (Kind::Repositories, "is") => matches!(value, "public" | "private" | "fork" | "archived"),
        // Public repositories that depend on a package the named repository provides. The
        // GitHub query parsers never produce it; "used by" pages do.
        (Kind::Repositories, "uses") => value.parse::<i64>().is_ok_and(|id| valid_id(id).is_ok()),
        (Kind::Issues, "author" | "assignee" | "label") => true,
        (Kind::Issues, "is") => matches!(
            value,
            "issue"
                | "pr"
                | "pull-request"
                | "open"
                | "closed"
                | "merged"
                | "unmerged"
                | "draft"
                | "locked"
                | "unlocked"
                | "public"
                | "private"
        ),
        (Kind::Issues, "type") => matches!(value, "issue" | "pr"),
        (Kind::Issues, "state") => matches!(value, "open" | "closed"),
        (Kind::Issues, "no") => matches!(value, "label" | "assignee"),
        (Kind::Issues, "archived" | "draft") => boolean,
        (Kind::Code, "path" | "filename") => true,
        (Kind::Code, "extension") => value.bytes().all(|byte| byte.is_ascii_alphanumeric()),
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err("Unsupported search qualifier")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub kind: Kind,
    pub viewer: Option<i64>,
    pub filter: Filter,
    pub sort: Option<String>,
    pub ascending: bool,
    pub offset: u32,
    pub limit: u32,
}
impl Query {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.viewer.is_some_and(|viewer| valid_id(viewer).is_err()) {
            return Err("Invalid search viewer");
        }
        if self.offset >= MAXIMUM_OFFSET || !(1..=MAXIMUM_PAGE).contains(&self.limit) {
            return Err("Only the first 1000 search results are available");
        }
        let sorts: &[&str] = match self.kind {
            Kind::Repositories => &["stars", "forks", "updated", "created", "help-wanted-issues"],
            Kind::Issues => &["created", "updated", "comments"],
            Kind::Code => &["indexed"],
        };
        if self
            .sort
            .as_deref()
            .is_some_and(|sort| !sorts.contains(&sort))
        {
            return Err("Invalid search sort");
        }
        self.filter.validate(self.kind)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fragment {
    pub line: i64,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Code {
    pub path: String,
    pub sha: String,
    pub size: i64,
    pub revision: String,
    pub fragments: Vec<Fragment>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hit {
    /// The repository, issue or pull request id; code hits carry their repository's id.
    pub id: i64,
    pub pull: bool,
    pub repository_id: i64,
    pub cell: i32,
    pub public: bool,
    /// False when the owning organization refuses classic tokens.
    pub classic_allowed: bool,
    /// The item's document; code hits carry their repository's document.
    pub document: Value,
    pub code: Option<Code>,
    pub score: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Results {
    pub total: i64,
    pub items: Vec<Hit>,
}

/// Only the authenticated directory client implements this; cells never hold index credentials.
pub trait Index: Send + Sync {
    fn index<'a>(&'a self, batch: &'a Batch) -> BoxFuture<'a, Result<Indexed>>;
    fn search<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, Result<Results>>;
}
pub type Provider = Arc<dyn Index>;

/// Search errors that callers show: the index refused the query, or cannot be reached.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Search request is invalid")]
    Invalid,
    #[error("Search is unavailable")]
    Unavailable,
}

/// Triggers queue changes only while this cell writes to a search index.
pub async fn enable(pool: &DbPool, enabled: bool) -> Result<()> {
    match pool {
        DbPool::Postgres(pool) => {
            sqlx::query("UPDATE node_configuration SET search_index=$1 WHERE id=1")
                .bind(enabled)
                .execute(pool)
                .await?;
        }
        DbPool::Sqlite(pool) => {
            ensure!(
                !enabled,
                "Copy this SQLite cell to PostgreSQL before indexing it"
            );
            sqlx::query("UPDATE node_configuration SET search_index=0 WHERE id=1")
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// Who may read a repository's private documents, and whether its organization takes classic tokens.
pub struct Access {
    pub readers: Vec<i64>,
    pub classic_allowed: bool,
}

/// Readers are the repository's grantees and members, kept by the live read rule.
pub async fn access(pool: &PgPool, repository: i64) -> Result<Access> {
    let sql = format!(
        "SELECT u.id FROM repositories r JOIN users u ON u.id IN (SELECT a.user_id FROM repo_access a WHERE a.repo_id=r.id UNION SELECT m.user_id FROM org_memberships m WHERE m.org_id=r.owner_org_id UNION SELECT r.owner_user_id) WHERE r.id=$1 AND r.is_private AND NOT u.is_banned AND NOT u.must_change_password AND {} ORDER BY u.id LIMIT $2",
        crate::queries::postgres::repositories::access::readable_condition()
    );
    let readers: Vec<i64> = sqlx::query_scalar(&sql)
        .bind(repository)
        .bind(MAXIMUM_READERS as i64 + 1)
        .fetch_all(pool)
        .await?;
    if readers.len() > MAXIMUM_READERS {
        tracing::warn!(
            repository,
            "Private repository has more search readers than the index keeps"
        );
    }
    let classic_allowed: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM repositories r JOIN organization_token_policies p ON p.org_id=r.owner_org_id WHERE r.id=$1 AND NOT p.classic_allowed)")
        .bind(repository)
        .fetch_one(pool)
        .await?;
    Ok(Access {
        readers: readers.into_iter().take(MAXIMUM_READERS).collect(),
        classic_allowed,
    })
}

/// The repository that holds an issue or pull request now.
pub async fn item_repository(pool: &PgPool, item: Item) -> Result<Option<i64>> {
    let (sql, id) = match item {
        Item::Repository => return Ok(None),
        Item::Issue { id } => ("SELECT repo_id FROM issues WHERE id=$1", id),
        Item::Pull { id } => ("SELECT repo_id FROM pull_requests WHERE id=$1", id),
    };
    Ok(sqlx::query_scalar(sql)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Queues every native repository after `after`, with its issues and pull requests, so an
/// index rebuilt from nothing fills again. Returns the last repository and the count.
pub async fn queue_all(pool: &DbPool, after: i64, limit: i64) -> Result<(i64, i64)> {
    ensure!(after >= 0 && (1..=1000).contains(&limit), "Invalid page");
    let DbPool::Postgres(pool) = pool else {
        anyhow::bail!("Copy this SQLite cell to PostgreSQL before indexing it");
    };
    let enabled: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT search_index FROM node_configuration WHERE id=1),FALSE)",
    )
    .fetch_one(pool)
    .await?;
    ensure!(
        enabled,
        "Start this cell with a search index before reindexing it"
    );
    let mut tx = pool.begin().await?;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE id>$1 AND NOT is_stub AND NOT is_gist AND home_cell=COALESCE((SELECT cell_id FROM node_configuration WHERE id=1),0) ORDER BY id LIMIT $2")
        .bind(after).bind(limit).fetch_all(&mut *tx).await?;
    sqlx::query("SELECT queue_search(id,'repository',NULL) FROM unnest($1::bigint[]) AS r(id)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "SELECT queue_search(repo_id,'issue',id) FROM issues WHERE repo_id=ANY($1::bigint[])",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "SELECT queue_search(repo_id,'pull',id) FROM pull_requests WHERE repo_id=ANY($1::bigint[])",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((ids.last().copied().unwrap_or(after), ids.len() as i64))
}
