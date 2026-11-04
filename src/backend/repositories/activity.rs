//! Accepted ref activity shared by editor/branch/merge publication and Git HTTP/SSH hooks.

use crate::backend::webhooks::ref_changes::RefMove;
use anyhow::{Context, Result};
use codeyard_db_core::DbPool;
use codeyard_db_repos::repositories::activity::Kind;
use codeyard_db_repos::repositories::activity::RefActivity;
use codeyard_git as git;
use std::path::Path;
use uuid::Uuid;

/// Keep accepted-write bookkeeping alive if an HTTP caller disconnects after publication.
/// Only real branch moves are recorded; tags and no-op updates are not repository activity.
pub async fn record_moves(
    pool: &DbPool,
    repo: i64,
    actor: i64,
    path: &Path,
    moves: &[RefMove],
) -> Result<()> {
    let (pool, path, moves) = (pool.clone(), path.to_owned(), moves.to_vec());
    tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(30), async move {
            let operation = Uuid::new_v4();
            let mut changes = Vec::new();
            for (reference, before, after) in &moves {
                if !reference.starts_with("refs/heads/") || before == after {
                    continue;
                }
                let is_zero = |sha: &str| sha.bytes().all(|byte| byte == b'0');
                let kind = if is_zero(before) {
                    Kind::BranchCreation
                } else if is_zero(after) {
                    Kind::BranchDeletion
                } else if git::commit_is_ancestor(&path, before, after).await? {
                    Kind::Push
                } else {
                    Kind::ForcePush
                };
                changes.push(RefActivity::new(reference, before, after, kind)?);
            }
            if !changes.is_empty() {
                codeyard_db_repos::repositories::activity::record(
                    &pool, repo, actor, &operation, &changes,
                )
                .await?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Accepted activity bookkeeping exceeded its time limit")?
    })
    .await
    .context("Activity recording task failed")?
}

#[derive(Default, Clone, Debug, serde::Deserialize)]
pub struct Query {
    pub direction: Option<String>,
    pub per_page: Option<usize>,
    pub before: Option<String>,
    pub after: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub actor: Option<String>,
    pub time_period: Option<String>,
    pub activity_type: Option<String>,
}
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidQuery(pub &'static str);
pub struct Page {
    pub rows: Vec<codeyard_db_repos::repositories::activity::Activity>,
    pub previous: Option<String>,
    pub next: Option<String>,
}

pub fn cursor(id: i64) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id.to_be_bytes())
}
/// Preserves the effective filters for REST Link headers and native navigation alike.
pub fn query_string(input: &Query, before: Option<&str>, after: Option<&str>) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair(
        "per_page",
        &input.per_page.unwrap_or(30).clamp(1, 100).to_string(),
    );
    for (key, value) in [
        ("direction", input.direction.as_deref()),
        ("ref", input.reference.as_deref()),
        ("actor", input.actor.as_deref()),
        ("time_period", input.time_period.as_deref()),
        ("activity_type", input.activity_type.as_deref()),
        ("before", before),
        ("after", after),
    ] {
        if let Some(value) = value {
            query.append_pair(key, value);
        }
    }
    query.finish()
}
fn parse_cursor(value: &str) -> Option<i64> {
    use base64::Engine;
    if value.len() != 11 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()?;
    let id = i64::from_be_bytes(bytes.try_into().ok()?);
    (id > 0 && cursor(id) == value).then_some(id)
}
/// Both native insights and REST use the same filters and stable, bidirectional cursor window.
pub async fn page(pool: &DbPool, repo: i64, input: &Query, viewer: Option<i64>) -> Result<Page> {
    let descending = match input.direction.as_deref().unwrap_or("desc") {
        "desc" => true,
        "asc" => false,
        _ => return Err(InvalidQuery("Invalid activity direction").into()),
    };
    if input.before.is_some() && input.after.is_some() {
        return Err(InvalidQuery("Use only one of before and after").into());
    }
    let boundary = input
        .before
        .as_ref()
        .or(input.after.as_ref())
        .map(|value| parse_cursor(value).ok_or(InvalidQuery("Invalid activity cursor")))
        .transpose()?;
    if input
        .reference
        .as_ref()
        .is_some_and(|value| value.len() > 1024)
        || input
            .actor
            .as_ref()
            .is_some_and(|value| value.len() > 1024 || value.chars().count() > 255)
    {
        return Err(InvalidQuery("Activity filter is too long").into());
    }
    let kind = input
        .activity_type
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| {
            [
                Kind::Push,
                Kind::ForcePush,
                Kind::BranchCreation,
                Kind::BranchDeletion,
                Kind::PrMerge,
                Kind::MergeQueueMerge,
            ]
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or(InvalidQuery("Invalid activity type"))
        })
        .transpose()?;
    let now = chrono::Utc::now();
    let since = match input.time_period.as_deref() {
        None | Some("") => None,
        Some("day") => Some(now - chrono::Duration::days(1)),
        Some("week") => Some(now - chrono::Duration::weeks(1)),
        Some("month") => now.checked_sub_months(chrono::Months::new(1)),
        Some("quarter") => now.checked_sub_months(chrono::Months::new(3)),
        Some("year") => now.checked_sub_months(chrono::Months::new(12)),
        Some(_) => return Err(InvalidQuery("Invalid activity time period").into()),
    };
    let reverse = input.before.is_some();
    let limit = input.per_page.unwrap_or(30).clamp(1, 100);
    let filter = codeyard_db_repos::repositories::activity::Filter {
        boundary,
        descending: descending != reverse,
        reference: input.reference.as_deref().filter(|value| !value.is_empty()),
        actor: input.actor.as_deref().filter(|value| !value.is_empty()),
        kind,
        since,
    };
    let mut rows = codeyard_db_repos::repositories::activity::list(
        pool,
        repo,
        &filter,
        (limit + 1) as i64,
        viewer,
    )
    .await?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    if reverse {
        rows.reverse();
    }
    let previous = rows
        .first()
        .filter(|_| if reverse { more } else { boundary.is_some() })
        .map(|row| cursor(row.id));
    let next = rows
        .last()
        .filter(|_| if reverse { boundary.is_some() } else { more })
        .map(|row| cursor(row.id));
    Ok(Page {
        rows,
        previous,
        next,
    })
}
