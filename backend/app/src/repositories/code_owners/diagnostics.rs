//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
//! Diagnostics use the same tokenizer, pattern rules and live owner resolution as review gates.
use crate::cluster::context::NodeContext;
use crate::repositories::code_owners::diagnostics::diagnose;
use crate::repositories::code_owners::diagnostics::load_diagnostics;
use crate::repositories::code_owners::diagnostics::Diagnostic;
use crate::repositories::code_owners::parse;
use crate::repositories::code_owners::pattern_error;
use crate::repositories::code_owners::resolve_owners;
use crate::repositories::code_owners::split_line;
use crate::repositories::code_owners::LOCATIONS;
use crate::repositories::code_owners::MAX_BYTES;
use crate::repositories::code_owners::MAX_LINES;
use crate::repositories::store::repo_store;
use crate::storage::range::ByteRange as BlobRange;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Diagnostic {
    pub line: usize,
    pub column: usize,
    pub source: String,
    pub kind: String,
    pub suggestion: Option<String>,
    pub message: String,
    pub path: String,
}
impl Diagnostic {
    fn new(path: &str, source: &str, line: usize, column: usize, kind: &str, detail: &str) -> Self {
        Self {
            line,
            column,
            source: source.to_owned(),
            kind: kind.to_owned(),
            suggestion: None,
            message: format!(
                "{kind} on line {line}:\n\n  {source}\n  {}^\n{detail}",
                " ".repeat(column.saturating_sub(1))
            ),
            path: path.to_owned(),
        }
    }
}
fn invalid(message: &str) -> RepoError {
    RepoError::new(ErrorKind::Invalid, message)
}
fn push(
    errors: &mut Vec<Diagnostic>,
    remaining: &mut usize,
    error: Diagnostic,
) -> Result<(), RepoError> {
    let cost = error
        .source
        .len()
        .saturating_add(error.message.len())
        .saturating_mul(6);
    *remaining = remaining
        .checked_sub(cost)
        .ok_or_else(|| invalid("CODEOWNERS diagnostics exceed their output limit"))?;
    errors.push(error);
    Ok(())
}

pub async fn diagnose(
    pool: &codeyard_db_core::DbPool,
    repository: &Repository,
    path: &str,
    text: &str,
) -> Result<Vec<Diagnostic>, RepoError> {
    if text.len() > MAX_BYTES || text.lines().count() > MAX_LINES {
        return Err(invalid("CODEOWNERS exceeds its size or line limit"));
    }
    parse(text).map_err(|_| invalid("CODEOWNERS exceeds its size or work limit"))?;
    let mut errors = Vec::new();
    let mut occurrences = Vec::new();
    let mut remaining = 4 * 1024 * 1024;
    for (index, source) in text.lines().enumerate() {
        let Some((pattern, owners)) = split_line(source) else {
            continue;
        };
        let start = source.len() - source.trim_start().len();
        if let Some(detail) = pattern_error(pattern) {
            let mut error = Diagnostic::new(
                path,
                source,
                index + 1,
                source[..start].chars().count() + 1,
                "Invalid path pattern",
                detail,
            );
            if pattern == "/" {
                error.suggestion = Some(detail.to_owned());
            }
            push(&mut errors, &mut remaining, error)?;
            continue;
        }
        let mut cursor = start + pattern.len();
        let mut seen = HashSet::new();
        for owner in owners
            .split_whitespace()
            .take_while(|owner| !owner.starts_with('#'))
        {
            let at = cursor
                + source[cursor..]
                    .find(owner)
                    .expect("owner belongs to its source");
            cursor = at + owner.len();
            if !seen.insert(owner) {
                continue;
            }
            let column = source[..at].chars().count() + 1;
            if owner.contains('@') {
                occurrences.push((index + 1, source, column, owner));
            } else {
                push(&mut errors,&mut remaining,Diagnostic::new(path,source,index+1,column,"Invalid owner","Owners must be usernames, visible teams, or email addresses with write access"))?;
            }
        }
    }
    let resolved = resolve_owners(
        pool,
        repository,
        occurrences.iter().map(|(_, _, _, owner)| *owner),
    )
    .await
    .map_err(RepoError::from)?;
    for (line, source, column, owner) in occurrences {
        if resolved.get(owner).is_none_or(Vec::is_empty) {
            push(
                &mut errors,
                &mut remaining,
                Diagnostic::new(
                    path,
                    source,
                    line,
                    column,
                    "Invalid owner",
                    "The owner does not exist or does not have write access to this repository",
                ),
            )?;
        }
    }
    errors.sort_by_key(|error| (error.line, error.column));
    Ok(errors)
}

pub async fn load_diagnostics(
    pool: &codeyard_db_core::DbPool,
    repository: &Repository,
    cluster: Option<&NodeContext>,
    commit: &str,
) -> Result<Option<Vec<Diagnostic>>, RepoError> {
    let store = repo_store(repository, cluster);
    let commit = store.commit(commit).await?.sha;
    for path in LOCATIONS {
        match store
            .blob_range(
                &commit,
                path,
                BlobRange::Window {
                    offset: 0,
                    length: MAX_BYTES as u64,
                },
            )
            .await
        {
            Ok(blob) => {
                if blob.size > MAX_BYTES as u64 || blob.is_binary {
                    return Err(invalid(
                        "CODEOWNERS must be a UTF-8 text file no larger than 3 MiB",
                    ));
                }
                let text = std::str::from_utf8(&blob.bytes)
                    .map_err(|_| invalid("CODEOWNERS must be UTF-8"))?;
                return diagnose(pool, repository, path, text).await.map(Some);
            }
            Err(error) if error.kind == ErrorKind::MissingPath => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}
