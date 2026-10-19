//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

// The last number each repository has allocated to an issue or pull request. A repository
// without a counter has none and is absent from the result.
native_operation!(crate::backend::db::postgres::discussions::last_discussion_numbers,
    crate::backend::db::sqlite::discussions::last_discussion_numbers,
    fn last_discussion_numbers(pool: &DbPool, repo_ids: &[i64]) -> Result<Vec<(i64, i64)>>);

// The usernames of everyone who opened an issue or pull request in a repository, in name order.
native_operation!(crate::backend::db::postgres::discussions::list_repo_authors,
    crate::backend::db::sqlite::discussions::list_repo_authors,
    fn list_repo_authors(pool: &DbPool, repo_id: i64) -> Result<Vec<String>>);

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum DiscussionWriteRefusal {
    #[error("discussion_authority_denied")]
    Denied,
    #[error("discussion_review_invalid")]
    InvalidReview,
    #[error("discussion_review_full")]
    ReviewFull,
}

pub struct DiscussionEdit<'a> {
    pub actor_id: i64,
    pub expected_state: &'a str,
    pub expected_updated: DateTime<Utc>,
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    pub state: Option<&'a str>,
}

impl DiscussionEdit<'_> {
    /// Revision tokens must round-trip exactly through native microsecond storage.
    /// SQLx PostgreSQL timestamp binding otherwise truncates extra nanoseconds,
    /// turning a different caller-supplied revision into a matching CAS token.
    pub(crate) fn has_exact_revision(&self) -> bool {
        DateTime::from_timestamp_micros(self.expected_updated.timestamp_micros())
            == Some(self.expected_updated)
    }
}

/// Counts of a repository's issues or pull requests by state, after the search text.
/// `all` counts every state, so it can exceed `open + closed` when a pull request is merged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateCounts {
    pub open: i64,
    pub closed: i64,
    pub all: i64,
}

impl StateCounts {
    /// Folds the rows of a `GROUP BY state` query.
    pub fn from_rows(rows: Vec<(String, i64)>) -> Self {
        rows.into_iter()
            .fold(Self::default(), |mut counts, (state, count)| {
                counts.all += count;
                match state.as_str() {
                    "open" => counts.open += count,
                    "closed" => counts.closed += count,
                    _ => {}
                }
                counts
            })
    }
}

/// The pattern for a title or body that contains `text`. The search text matches itself, so its
/// `\`, `%` and `_` are escaped. Both databases compare with `ESCAPE '\'`.
pub fn contains_pattern(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    pattern.push('%');
    for character in text.chars() {
        if matches!(character, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

/// A list's narrowing by a linked row: every row, rows with no link, or rows linked to one id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pick {
    #[default]
    Any,
    Unset,
    Is(i64),
}

impl Pick {
    /// The mode and id the list queries bind: 0 for any, 1 for unset, 2 for the id.
    pub(crate) fn binding(self) -> (i64, Option<i64>) {
        match self {
            Self::Any => (0, None),
            Self::Unset => (1, None),
            Self::Is(id) => (2, Some(id)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListSort {
    #[default]
    Newest,
    Oldest,
    RecentlyUpdated,
    LeastRecentlyUpdated,
    MostCommented,
    LeastCommented,
}

impl ListSort {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "newest" => Some(Self::Newest),
            "oldest" => Some(Self::Oldest),
            "recently-updated" => Some(Self::RecentlyUpdated),
            "least-recently-updated" => Some(Self::LeastRecentlyUpdated),
            "most-commented" => Some(Self::MostCommented),
            "least-commented" => Some(Self::LeastCommented),
            _ => None,
        }
    }

    pub fn value(self) -> &'static str {
        match self {
            Self::Newest => "newest",
            Self::Oldest => "oldest",
            Self::RecentlyUpdated => "recently-updated",
            Self::LeastRecentlyUpdated => "least-recently-updated",
            Self::MostCommented => "most-commented",
            Self::LeastCommented => "least-commented",
        }
    }

    /// The ORDER BY terms over the list alias `i`. `comments` and `parent` name the comment
    /// table and its column that points at the list row. `counted` is an extra condition on the
    /// comment alias `c` that leaves out comments the list does not show. All of them come from
    /// code, never from input.
    pub(crate) fn order_by(self, comments: &str, parent: &str, counted: &str) -> String {
        match self {
            Self::Newest => "i.created_at DESC".into(),
            Self::Oldest => "i.created_at ASC".into(),
            Self::RecentlyUpdated => "i.updated_at DESC".into(),
            Self::LeastRecentlyUpdated => "i.updated_at ASC".into(),
            Self::MostCommented => format!(
                "(SELECT COUNT(*) FROM {comments} c WHERE c.{parent} = i.id {counted}) DESC, i.created_at DESC"
            ),
            Self::LeastCommented => format!(
                "(SELECT COUNT(*) FROM {comments} c WHERE c.{parent} = i.id {counted}) ASC, i.created_at DESC"
            ),
        }
    }
}

/// The pull request comments a list counts: those outside a pending review, which the list shows
/// to nobody but its author.
pub(crate) const SHOWN_PR_COMMENT: &str =
    "AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = c.review_id AND v.state = 'pending')";

/// The narrowing of an issue or pull request list. `labels` are label names, matched
/// case-insensitively; an item must carry every one of them. The names must be distinct ignoring
/// case.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListFilter<'a> {
    pub state: Option<&'a str>,
    /// Include merged pull requests in the closed milestone tab.
    pub include_merged: bool,
    pub search: Option<&'a str>,
    pub author_id: Option<i64>,
    pub assignee: Pick,
    pub milestone: Pick,
    pub labels: &'a [String],
    pub sort: ListSort,
}
