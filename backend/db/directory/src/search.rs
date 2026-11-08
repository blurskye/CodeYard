//! The global search index lives in the directory database. A cell writes only documents of
//! repositories homed in it, and a query returns a private document only to the readers that
//! its cell named.
use super::{Directory, Refusal};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_database::cells::search::{
    Batch, Code, Filter, Fragment, Hit, Indexed, Kind, Query, Results, Version, MAXIMUM_CODE_BYTES,
    MAXIMUM_FILES,
};
use codeyard_types::cells::{Object, ObjectKind};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder};

/// Replies stay under the transport limit; a page of very large documents comes back short.
const MAXIMUM_REPLY_BYTES: usize = 3 * 1024 * 1024;
const MAXIMUM_FRAGMENTS: usize = 3;
const MAXIMUM_FRAGMENT_CHARS: usize = 200;
const MAXIMUM_NAMES: usize = 100;
/// Matches are counted up to the result window a client can page through.
const MAXIMUM_TOTAL: i64 = 1000;

fn text(document: &Value, field: &str) -> Option<String> {
    document.get(field)?.as_str().map(str::to_owned)
}
fn flag(document: &Value, field: &str) -> bool {
    document
        .get(field)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
fn count(document: &Value, field: &str) -> i64 {
    document
        .get(field)
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0)
}
fn time(document: &Value, field: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(document.get(field)?.as_str()?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}
/// Lowercased strings of an array, or of one field of each object in it.
fn names(document: &Value, field: &str, key: Option<&str>) -> Vec<String> {
    document
        .get(field)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match key {
                    Some(key) => item.get(key)?.as_str(),
                    None => item.as_str(),
                })
                .take(MAXIMUM_NAMES)
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default()
}
fn repository(id: i64) -> Object {
    Object {
        kind: ObjectKind::Repository,
        id,
    }
}

impl Directory {
    /// `peer` is the authenticated transport key of the writing cell.
    pub async fn index(&self, peer: &str, batch: &Batch) -> Result<Indexed> {
        self.run(async {
            batch.validate().map_err(|_| Refusal::Invalid)?;
            let mut tx = self.begin().await?;
            let cell = super::owner(&mut tx, peer, false).await?;
            let cell_id = i32::from(cell.get());
            let root = match batch {
                Batch::Repository { id, .. } => *id,
                Batch::Issue { repository_id, .. } | Batch::Code { repository_id, .. } => {
                    *repository_id
                }
            };
            self.require_home(&mut tx, repository(root), cell).await?;
            let indexed = match batch {
                Batch::Repository {
                    id,
                    version,
                    document,
                    public,
                    classic_allowed,
                    readers,
                    packages,
                } => {
                    if let Some(document) = document {
                        ensure!(
                            document.get("id").and_then(Value::as_i64) == Some(*id)
                                && document.get("private").and_then(Value::as_bool)
                                    == Some(!public),
                            Refusal::Invalid
                        );
                    }
                    let current: Option<(i32, i64, i64, Option<String>)> = sqlx::query_as(
                        "SELECT cell_id,event_id,attempt,code_head FROM search_repositories WHERE id=$1 FOR UPDATE",
                    )
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?;
                    if let Some((owner, event, attempt, code_head)) = current {
                        if owner == cell_id && (Version { event, attempt }) >= *version {
                            return Ok(Indexed { code_head });
                        }
                    }
                    let empty = Value::Null;
                    let d = document.as_ref().unwrap_or(&empty);
                    let owner = d
                        .get("owner")
                        .and_then(|owner| owner.get("login"))
                        .and_then(Value::as_str);
                    let code_head: Option<String> = sqlx::query_scalar(
                        "INSERT INTO search_repositories(id,cell_id,event_id,attempt,document,public,classic_allowed,full_name,owner,name,description,language,fork,archived,stars,forks,topics,created_at,updated_at,pushed_at) \
                         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20) \
                         ON CONFLICT(id) DO UPDATE SET cell_id=excluded.cell_id,event_id=excluded.event_id,attempt=excluded.attempt,document=excluded.document,public=excluded.public,classic_allowed=excluded.classic_allowed,full_name=excluded.full_name,owner=excluded.owner,name=excluded.name,description=excluded.description,language=excluded.language,fork=excluded.fork,archived=excluded.archived,stars=excluded.stars,forks=excluded.forks,topics=excluded.topics,created_at=excluded.created_at,updated_at=excluded.updated_at,pushed_at=excluded.pushed_at,\
                         code_head=CASE WHEN excluded.document IS NULL OR search_repositories.cell_id<>excluded.cell_id THEN NULL ELSE search_repositories.code_head END \
                         RETURNING code_head",
                    )
                    .bind(id)
                    .bind(cell_id)
                    .bind(version.event)
                    .bind(version.attempt)
                    .bind(document.clone())
                    .bind(document.is_some() && *public)
                    .bind(classic_allowed)
                    .bind(text(d, "full_name"))
                    .bind(owner)
                    .bind(text(d, "name"))
                    .bind(text(d, "description"))
                    .bind(text(d, "language"))
                    .bind(flag(d, "fork"))
                    .bind(flag(d, "archived"))
                    .bind(count(d, "stargazers_count"))
                    .bind(count(d, "forks_count"))
                    .bind(names(d, "topics", None))
                    .bind(time(d, "created_at"))
                    .bind(time(d, "updated_at"))
                    .bind(time(d, "pushed_at"))
                    .fetch_one(&mut *tx)
                    .await?;
                    sqlx::query("DELETE FROM search_readers WHERE repository_id=$1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    if document.is_some() && !public {
                        sqlx::query("INSERT INTO search_readers(repository_id,user_id) SELECT $1,u FROM unnest($2::bigint[]) AS r(u) ON CONFLICT DO NOTHING")
                            .bind(id)
                            .bind(readers)
                            .execute(&mut *tx)
                            .await?;
                    }
                    // "Used by" covers public repositories only, so a private one keeps none.
                    sqlx::query("DELETE FROM search_packages WHERE repository_id=$1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    if document.is_some() && *public && !packages.is_empty() {
                        sqlx::query("INSERT INTO search_packages(repository_id,ecosystem,name,provided) SELECT $1,p.ecosystem,p.name,p.provided FROM unnest($2::text[],$3::text[],$4::boolean[]) AS p(ecosystem,name,provided) ON CONFLICT DO NOTHING")
                            .bind(id)
                            .bind(packages.iter().map(|p| p.ecosystem.clone()).collect::<Vec<_>>())
                            .bind(packages.iter().map(|p| p.name.clone()).collect::<Vec<_>>())
                            .bind(packages.iter().map(|p| p.provided).collect::<Vec<_>>())
                            .execute(&mut *tx)
                            .await?;
                    }
                    if code_head.is_none() {
                        delete_code(&mut tx, *id).await?;
                    }
                    if document.is_none() {
                        sqlx::query("DELETE FROM search_issues WHERE repository_id=$1")
                            .bind(id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    Indexed { code_head }
                }
                Batch::Issue {
                    id,
                    pull,
                    repository_id,
                    version,
                    document,
                    comments,
                } => {
                    let current: Option<(i64, i32, i64, i64)> = sqlx::query_as(
                        "SELECT repository_id,cell_id,event_id,attempt FROM search_issues WHERE pull=$1 AND id=$2 FOR UPDATE",
                    )
                    .bind(pull)
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?;
                    if let Some((previous, owner, event, attempt)) = current {
                        // An item that moved between repositories is rewritten only by the
                        // home of the repository that held it.
                        if previous != *repository_id {
                            self.require_home(&mut tx, repository(previous), cell)
                                .await?;
                        }
                        if owner == cell_id && (Version { event, attempt }) >= *version {
                            return Ok(Indexed::default());
                        }
                    }
                    let empty = Value::Null;
                    let d = document.as_ref().unwrap_or(&empty);
                    let merged = d
                        .get("pull_request")
                        .and_then(|pull| pull.get("merged_at"))
                        .is_some_and(|merged| !merged.is_null());
                    let author = d
                        .get("user")
                        .and_then(|user| user.get("login"))
                        .and_then(Value::as_str);
                    sqlx::query(
                        "INSERT INTO search_issues(pull,id,repository_id,cell_id,event_id,attempt,document,state,merged,draft,locked,author,labels,assignees,comments,created_at,updated_at,title,body,comment_text) \
                         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20) \
                         ON CONFLICT(pull,id) DO UPDATE SET repository_id=excluded.repository_id,cell_id=excluded.cell_id,event_id=excluded.event_id,attempt=excluded.attempt,document=excluded.document,state=excluded.state,merged=excluded.merged,draft=excluded.draft,locked=excluded.locked,author=excluded.author,labels=excluded.labels,assignees=excluded.assignees,comments=excluded.comments,created_at=excluded.created_at,updated_at=excluded.updated_at,title=excluded.title,body=excluded.body,comment_text=excluded.comment_text",
                    )
                    .bind(pull)
                    .bind(id)
                    .bind(repository_id)
                    .bind(cell_id)
                    .bind(version.event)
                    .bind(version.attempt)
                    .bind(document.clone())
                    .bind(text(d, "state"))
                    .bind(merged)
                    .bind(flag(d, "draft"))
                    .bind(flag(d, "locked"))
                    .bind(author)
                    .bind(names(d, "labels", Some("name")))
                    .bind(names(d, "assignees", Some("login")))
                    .bind(count(d, "comments"))
                    .bind(time(d, "created_at"))
                    .bind(time(d, "updated_at"))
                    .bind(text(d, "title"))
                    .bind(text(d, "body"))
                    .bind(comments)
                    .execute(&mut *tx)
                    .await?;
                    Indexed::default()
                }
                Batch::Code {
                    repository_id,
                    version,
                    head,
                    files,
                    first,
                    last,
                } => {
                    let mut paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
                    paths.sort_unstable();
                    paths.dedup();
                    ensure!(paths.len() == files.len(), Refusal::Invalid);
                    let current: Option<(i32, i64, i64, bool, Option<String>)> = sqlx::query_as(
                        "SELECT cell_id,event_id,attempt,document IS NOT NULL,code_head FROM search_repositories WHERE id=$1 FOR UPDATE",
                    )
                    .bind(repository_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                    let Some((owner, event, attempt, live, code_head)) = current else {
                        return Ok(Indexed::default());
                    };
                    // Code belongs to the repository document written by the same claim.
                    if owner != cell_id || (Version { event, attempt }) != *version || !live {
                        return Ok(Indexed { code_head });
                    }
                    if *first {
                        delete_code(&mut tx, *repository_id).await?;
                        sqlx::query("UPDATE search_repositories SET code_head=NULL WHERE id=$1")
                            .bind(repository_id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
                    let replaced: Vec<Vec<u8>> = sqlx::query_scalar(
                        "SELECT digest FROM search_code WHERE repository_id=$1 AND path=ANY($2)",
                    )
                    .bind(repository_id)
                    .bind(&paths)
                    .fetch_all(&mut *tx)
                    .await?;
                    let contents: Vec<String> =
                        files.iter().map(|file| file.content.clone()).collect();
                    sqlx::query("INSERT INTO search_blobs(digest,content) SELECT sha256(convert_to(c,'UTF8')),c FROM unnest($1::text[]) AS f(c) ON CONFLICT DO NOTHING")
                        .bind(&contents)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("INSERT INTO search_code(repository_id,path,sha,size,digest) SELECT $1,f.path,f.sha,f.size,sha256(convert_to(f.content,'UTF8')) FROM unnest($2::text[],$3::text[],$4::bigint[],$5::text[]) AS f(path,sha,size,content) ON CONFLICT(repository_id,path) DO UPDATE SET sha=excluded.sha,size=excluded.size,digest=excluded.digest")
                        .bind(repository_id)
                        .bind(&paths)
                        .bind(files.iter().map(|file| file.sha.to_ascii_lowercase()).collect::<Vec<_>>())
                        .bind(files.iter().map(|file| file.size).collect::<Vec<_>>())
                        .bind(&contents)
                        .execute(&mut *tx)
                        .await?;
                    delete_unused_blobs(&mut tx, &replaced).await?;
                    let (stored, bytes): (i64, i64) = sqlx::query_as("SELECT COUNT(*),COALESCE(SUM(octet_length(c.path)+octet_length(b.content)),0)::BIGINT FROM search_code c JOIN search_blobs b ON b.digest=c.digest WHERE c.repository_id=$1")
                        .bind(repository_id)
                        .fetch_one(&mut *tx)
                        .await?;
                    ensure!(
                        stored as usize <= MAXIMUM_FILES && bytes as usize <= MAXIMUM_CODE_BYTES,
                        Refusal::Invalid
                    );
                    let code_head = if *last {
                        let head = head.to_ascii_lowercase();
                        sqlx::query("UPDATE search_repositories SET code_head=$2 WHERE id=$1")
                            .bind(repository_id)
                            .bind(&head)
                            .execute(&mut *tx)
                            .await?;
                        Some(head)
                    } else {
                        None
                    };
                    Indexed { code_head }
                }
            };
            tx.commit().await?;
            Ok(indexed)
        })
        .await
    }

    /// Any enrolled cell may search on behalf of its signed-in viewer.
    pub async fn search(&self, peer: &str, query: &Query) -> Result<Results> {
        self.run(async {
            query.validate().map_err(|_| Refusal::Invalid)?;
            ensure!(query_bytes(&query.filter) <= self.config.search_query_bytes, Refusal::Invalid);
            let mut tx = self.begin().await?;
            super::owner(&mut tx, peer, false).await?;
            let fields = text_fields(query.kind, &query.filter)?;
            let mut counted = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM (SELECT 1 FROM ");
            matching(&mut counted, query, &fields)?;
            counted.push(" LIMIT ");
            counted.push_bind(MAXIMUM_TOTAL);
            counted.push(") AS found");
            let total: i64 = counted.build_query_scalar::<i64>().fetch_one(&mut *tx).await?;
            let mut page = QueryBuilder::<Postgres>::new(match query.kind {
                Kind::Repositories => "SELECT r.id,FALSE AS pull,r.id AS repository_id,r.cell_id,r.public,r.classic_allowed,r.document,NULL::TEXT AS path,NULL::TEXT AS sha,NULL::BIGINT AS size,NULL::TEXT AS content,r.code_head,",
                Kind::Issues => "SELECT i.id,i.pull,i.repository_id,r.cell_id,r.public,r.classic_allowed,i.document,NULL::TEXT AS path,NULL::TEXT AS sha,NULL::BIGINT AS size,NULL::TEXT AS content,r.code_head,",
                Kind::Code => "SELECT r.id,FALSE AS pull,r.id AS repository_id,r.cell_id,r.public,r.classic_allowed,r.document,c.path,c.sha,c.size,b.content,r.code_head,",
            });
            score(&mut page, query.kind, &fields, &query.filter);
            page.push(" AS score FROM ");
            matching(&mut page, query, &fields)?;
            page.push(" ORDER BY ");
            page.push(order(query));
            page.push(" LIMIT ");
            page.push_bind(i64::from(query.limit).min(MAXIMUM_TOTAL - i64::from(query.offset)));
            page.push(" OFFSET ");
            page.push_bind(i64::from(query.offset));
            let rows: Vec<Row> = page.build_query_as::<Row>().fetch_all(&mut *tx).await?;
            tx.commit().await?;
            let words: Vec<String> = query
                .filter
                .positive_text()
                .into_iter()
                .map(str::to_lowercase)
                .collect();
            let mut items = Vec::with_capacity(rows.len());
            let mut bytes = 0;
            for row in rows {
                let Some(document) = row.document else {
                    continue;
                };
                let code = match (row.path, row.sha, row.size, row.content) {
                    (Some(path), Some(sha), Some(size), Some(content)) => Some(Code {
                        path,
                        sha,
                        size,
                        revision: row.code_head.clone().unwrap_or_default(),
                        fragments: fragments(&content, &words),
                    }),
                    _ => None,
                };
                let hit = Hit {
                    id: row.id,
                    pull: row.pull,
                    repository_id: row.repository_id,
                    cell: row.cell_id,
                    public: row.public,
                    classic_allowed: row.classic_allowed,
                    document,
                    code,
                    score: row.score,
                };
                bytes += serde_json::to_vec(&hit)?.len();
                if bytes > MAXIMUM_REPLY_BYTES {
                    break;
                }
                items.push(hit);
            }
            Ok(Results { total, items })
        })
        .await
    }

    /// Empties the index; each cell then queues its documents again with `cell search reindex`.
    pub async fn reset_search(&self) -> Result<()> {
        self.run_for(self.config.migration_seconds, async {
            sqlx::query("TRUNCATE search_code,search_blobs,search_readers,search_packages,search_issues,search_repositories")
                .execute(&self.pool)
                .await?;
            Ok(())
        })
        .await
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    pull: bool,
    repository_id: i64,
    cell_id: i32,
    public: bool,
    classic_allowed: bool,
    document: Option<Value>,
    path: Option<String>,
    sha: Option<String>,
    size: Option<i64>,
    content: Option<String>,
    code_head: Option<String>,
    score: f64,
}

/// The columns free text matches: the kind's defaults, or the ones `in:` names.
fn text_fields(kind: Kind, filter: &Filter) -> Result<Vec<&'static str>> {
    fn named<'a>(filter: &'a Filter, values: &mut Vec<&'a str>) {
        match filter {
            Filter::All(items) | Filter::Any(items) => {
                items.iter().for_each(|item| named(item, values))
            }
            Filter::Not(item) => named(item, values),
            Filter::Term(key, value) if key == "in" => values.extend(value.split(',')),
            Filter::Term(..) => {}
        }
    }
    let mut values = Vec::new();
    named(filter, &mut values);
    if values.is_empty() {
        return Ok(match kind {
            Kind::Repositories => vec!["r.name", "r.description"],
            Kind::Issues => vec!["i.title", "i.body", "i.comment_text"],
            Kind::Code => vec!["b.content"],
        });
    }
    let mut fields = Vec::new();
    for value in values {
        let field = match (kind, value) {
            (Kind::Repositories, "name") => Some("r.name"),
            (Kind::Repositories, "description") => Some("r.description"),
            (Kind::Repositories, "topics") => Some("array_to_string(r.topics,' ')"),
            // README text is not indexed.
            (Kind::Repositories, "readme") => None,
            (Kind::Issues, "comments") => Some("i.comment_text"),
            (Kind::Issues, "title") => Some("i.title"),
            (Kind::Issues, "body") => Some("i.body"),
            (Kind::Code, "file") => Some("b.content"),
            (Kind::Code, "path") => Some("c.path"),
            _ => anyhow::bail!(Refusal::Invalid),
        };
        if let Some(field) = field {
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
    }
    Ok(fields)
}

/// All terms, including negations, share the directory's configured input budget.
fn query_bytes(filter: &Filter) -> usize {
    match filter {
        Filter::All(items) | Filter::Any(items) => items.iter().map(query_bytes).sum(),
        Filter::Not(item) => query_bytes(item),
        Filter::Term(key, value) => key.len() + value.len(),
    }
}

/// Only these schema-owned columns use word matching; names, paths and code stay literal.
fn vector(field: &str) -> Option<&'static str> {
    match field {
        "r.description" => Some("r.description_words"),
        "i.title" => Some("i.title_words"),
        "i.body" => Some("i.body_words"),
        "i.comment_text" => Some("i.comment_words"),
        _ => None,
    }
}

fn word_query(sql: &mut QueryBuilder<Postgres>, value: &str) {
    // GitHub's parser already owns operators and quoting. A term is plain text, never SQL
    // or tsquery syntax, including a quoted term containing punctuation or operators.
    sql.push("plainto_tsquery('simple'::regconfig,");
    sql.push_bind(value.to_owned());
    sql.push(")");
}

fn like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn push_text(sql: &mut QueryBuilder<Postgres>, fields: &[&'static str], value: &str) {
    if fields.is_empty() {
        sql.push("FALSE");
        return;
    }
    sql.push("(");
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            sql.push(" OR ");
        }
        if let Some(vector) = vector(field) {
            sql.push(vector);
            sql.push(" @@ ");
            word_query(sql, value);
        } else {
            // Bare columns retain trigram index use; NOT folds nulls to false.
            sql.push(*field);
            sql.push(" ILIKE ");
            sql.push_bind(format!("%{}%", like(value)));
            sql.push(" ESCAPE '\\'");
        }
    }
    sql.push(")");
}

/// FROM and WHERE: the kind's tables, what the viewer may read, then the filter.
fn matching(
    sql: &mut QueryBuilder<Postgres>,
    query: &Query,
    fields: &[&'static str],
) -> Result<()> {
    sql.push(match query.kind {
        Kind::Repositories => "search_repositories r WHERE r.document IS NOT NULL",
        Kind::Issues => "search_issues i JOIN search_repositories r ON r.id=i.repository_id WHERE i.document IS NOT NULL AND r.document IS NOT NULL",
        Kind::Code => "search_code c JOIN search_blobs b ON b.digest=c.digest JOIN search_repositories r ON r.id=c.repository_id WHERE r.document IS NOT NULL AND r.code_head IS NOT NULL",
    });
    match query.viewer {
        Some(viewer) => {
            sql.push(" AND (r.public OR EXISTS(SELECT 1 FROM search_readers x WHERE x.repository_id=r.id AND x.user_id=");
            sql.push_bind(viewer);
            sql.push("))");
        }
        None => {
            sql.push(" AND r.public");
        }
    }
    sql.push(" AND (");
    filter(sql, query.kind, fields, &query.filter)?;
    sql.push(")");
    Ok(())
}

fn filter(
    sql: &mut QueryBuilder<Postgres>,
    kind: Kind,
    fields: &[&'static str],
    value: &Filter,
) -> Result<()> {
    match value {
        Filter::All(items) | Filter::Any(items) => {
            let all = matches!(value, Filter::All(_));
            if items.is_empty() {
                sql.push(if all { "TRUE" } else { "FALSE" });
                return Ok(());
            }
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    sql.push(if all { " AND " } else { " OR " });
                }
                sql.push("(");
                filter(sql, kind, fields, item)?;
                sql.push(")");
            }
        }
        Filter::Not(item) => {
            sql.push("NOT COALESCE((");
            filter(sql, kind, fields, item)?;
            sql.push("),FALSE)");
        }
        Filter::Term(key, value) => term(sql, kind, fields, key, value)?,
    }
    Ok(())
}

fn equal_lower(sql: &mut QueryBuilder<Postgres>, column: &str, value: &str) {
    sql.push("lower(");
    sql.push(column);
    sql.push(")=lower(");
    sql.push_bind(value.to_owned());
    sql.push(")");
}
fn contains(sql: &mut QueryBuilder<Postgres>, column: &str, value: &str) {
    sql.push_bind(value.to_lowercase());
    sql.push("=ANY(");
    sql.push(column);
    sql.push(")");
}
fn boolean(column: &str, value: &str) -> Result<String> {
    Ok(match value {
        "true" => column.to_owned(),
        "false" => format!("NOT {column}"),
        _ => anyhow::bail!(Refusal::Invalid),
    })
}

fn term(
    sql: &mut QueryBuilder<Postgres>,
    kind: Kind,
    fields: &[&'static str],
    key: &str,
    value: &str,
) -> Result<()> {
    let fixed = match (kind, key) {
        (_, "text") => {
            push_text(sql, fields, value);
            return Ok(());
        }
        (_, "in") => "TRUE".to_owned(),
        (_, "repo") => {
            equal_lower(sql, "r.full_name", value);
            return Ok(());
        }
        // The cell resolves @me to the viewer's login before it asks.
        (_, "user" | "org" | "owner") => {
            equal_lower(sql, "r.owner", value);
            return Ok(());
        }
        (Kind::Repositories, "language") => {
            equal_lower(sql, "r.language", value);
            return Ok(());
        }
        (Kind::Repositories, "topic") => {
            contains(sql, "r.topics", value);
            return Ok(());
        }
        (Kind::Repositories, "uses") => {
            let provider: i64 = value.parse().map_err(|_| Refusal::Invalid)?;
            sql.push("(r.public AND r.id<>");
            sql.push_bind(provider);
            sql.push(" AND EXISTS(SELECT 1 FROM search_packages d JOIN search_packages p ON p.ecosystem=d.ecosystem AND p.name=d.name AND p.provided WHERE d.repository_id=r.id AND NOT d.provided AND p.repository_id=");
            sql.push_bind(provider);
            sql.push("))");
            return Ok(());
        }
        (Kind::Repositories, "fork") => match value {
            "true" => "TRUE".to_owned(),
            "false" => "NOT r.fork".to_owned(),
            "only" => "r.fork".to_owned(),
            _ => anyhow::bail!(Refusal::Invalid),
        },
        (Kind::Repositories | Kind::Issues, "archived") => boolean("r.archived", value)?,
        (Kind::Repositories, "is") => match value {
            "public" => "r.public",
            "private" => "NOT r.public",
            "fork" => "r.fork",
            "archived" => "r.archived",
            _ => anyhow::bail!(Refusal::Invalid),
        }
        .to_owned(),
        (Kind::Issues, "author") => {
            equal_lower(sql, "i.author", value);
            return Ok(());
        }
        (Kind::Issues, "assignee") => {
            contains(sql, "i.assignees", value);
            return Ok(());
        }
        (Kind::Issues, "label") => {
            contains(sql, "i.labels", value);
            return Ok(());
        }
        (Kind::Issues, "is" | "type" | "state") => match value {
            "issue" => "NOT i.pull",
            "pr" | "pull-request" => "i.pull",
            "open" => "i.state='open'",
            "closed" => "i.state='closed'",
            "merged" => "i.merged",
            "unmerged" => "(i.pull AND NOT i.merged)",
            "draft" => "i.draft",
            "locked" => "i.locked",
            "unlocked" => "NOT i.locked",
            "public" => "r.public",
            "private" => "NOT r.public",
            _ => anyhow::bail!(Refusal::Invalid),
        }
        .to_owned(),
        (Kind::Issues, "no") => match value {
            "label" => "cardinality(i.labels)=0",
            "assignee" => "cardinality(i.assignees)=0",
            _ => anyhow::bail!(Refusal::Invalid),
        }
        .to_owned(),
        (Kind::Issues, "draft") => boolean("i.draft", value)?,
        (Kind::Code, "path") => {
            push_text(sql, &["c.path"], value);
            return Ok(());
        }
        (Kind::Code, "filename") => {
            sql.push("(lower(c.path)=lower(");
            sql.push_bind(value.to_owned());
            sql.push(") OR c.path ILIKE ");
            sql.push_bind(format!("%/{}", like(value)));
            sql.push(" ESCAPE '\\')");
            return Ok(());
        }
        (Kind::Code, "extension") => {
            sql.push("c.path ILIKE ");
            sql.push_bind(format!("%.{}", like(value)));
            sql.push(" ESCAPE '\\'");
            return Ok(());
        }
        _ => anyhow::bail!(Refusal::Invalid),
    };
    sql.push(fixed);
    Ok(())
}

/// Weighted word frequency for prose, plus the existing literal score for names and code.
/// Negative terms filter only; they never raise a document's rank.
fn score(sql: &mut QueryBuilder<Postgres>, kind: Kind, fields: &[&'static str], filter: &Filter) {
    let words = filter.positive_text();
    if words.is_empty() {
        sql.push("0::DOUBLE PRECISION");
        return;
    }
    let fields: Vec<&'static str> = if kind == Kind::Code {
        vec!["b.content"]
    } else {
        fields.to_vec()
    };
    sql.push("(");
    for (index, word) in words.into_iter().enumerate() {
        if index > 0 {
            sql.push("+");
        }
        sql.push("(0");
        for field in &fields {
            sql.push("+");
            if let Some(vector) = vector(field) {
                sql.push("ts_rank(");
                sql.push(vector);
                sql.push(",");
                word_query(sql, word);
                sql.push(")");
            } else {
                sql.push("CASE WHEN ");
                push_text(sql, &[*field], word);
                sql.push(" THEN 1 ELSE 0 END");
            }
        }
        sql.push(")");
    }
    sql.push(")::DOUBLE PRECISION");
}

fn order(query: &Query) -> String {
    let direction = if query.ascending { "ASC" } else { "DESC" };
    let column = match (query.kind, query.sort.as_deref()) {
        (Kind::Repositories, Some("stars")) => "r.stars",
        (Kind::Repositories, Some("forks")) => "r.forks",
        (Kind::Repositories, Some("updated")) => "r.updated_at",
        (Kind::Repositories, Some("created")) => "r.created_at",
        (Kind::Issues, Some("created")) => "i.created_at",
        (Kind::Issues, Some("updated")) => "i.updated_at",
        (Kind::Issues, Some("comments")) => "i.comments",
        (Kind::Code, Some("indexed")) => "r.pushed_at",
        _ => "score",
    };
    let tie = match query.kind {
        Kind::Repositories => "r.id ASC",
        Kind::Issues => "i.id ASC,i.pull ASC",
        Kind::Code => "r.id ASC,c.path ASC",
    };
    if column == "score" {
        format!("score DESC,{tie}")
    } else {
        format!("score DESC,{column} {direction} NULLS LAST,{tie}")
    }
}

/// Removes a repository's code and any file text no other repository uses.
async fn delete_code(tx: &mut sqlx::Transaction<'static, Postgres>, repository: i64) -> Result<()> {
    let removed: Vec<Vec<u8>> =
        sqlx::query_scalar("DELETE FROM search_code WHERE repository_id=$1 RETURNING digest")
            .bind(repository)
            .fetch_all(&mut **tx)
            .await?;
    delete_unused_blobs(tx, &removed).await
}

/// Deletes the file text among `digests` that no code row uses. A concurrent writer that
/// starts using one of them makes either this delete or its own insert fail on the foreign
/// key, and the outbox retries the failed batch.
async fn delete_unused_blobs(
    tx: &mut sqlx::Transaction<'static, Postgres>,
    digests: &[Vec<u8>],
) -> Result<()> {
    if digests.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM search_blobs b WHERE b.digest=ANY($1) AND NOT EXISTS(SELECT 1 FROM search_code c WHERE c.digest=b.digest)")
        .bind(digests)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Lines of a file that hold a searched word, or its first line.
fn fragments(content: &str, words: &[String]) -> Vec<Fragment> {
    let clip = |line: &str| {
        line.chars()
            .take(MAXIMUM_FRAGMENT_CHARS)
            .collect::<String>()
    };
    let mut found = Vec::new();
    for (number, line) in content.lines().enumerate() {
        if found.len() == MAXIMUM_FRAGMENTS {
            break;
        }
        let lower = line.to_lowercase();
        if words.iter().any(|word| lower.contains(word.as_str())) {
            found.push(Fragment {
                line: number as i64 + 1,
                text: clip(line),
            });
        }
    }
    if found.is_empty() {
        if let Some(line) = content.lines().next() {
            found.push(Fragment {
                line: 1,
                text: clip(line),
            });
        }
    }
    found
}
