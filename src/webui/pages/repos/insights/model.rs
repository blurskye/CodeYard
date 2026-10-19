//! What the Insights pages show. The server computes these figures and the browser renders them,
//! so the types are shared by both builds.
use serde::{Deserialize, Serialize};

/// The sections, as the `section` query names them, with their labels.
pub const SECTIONS: [(&str, &str); 4] = [
    ("pulse", "Pulse"),
    ("contributors", "Contributors"),
    ("commits", "Commits"),
    ("code-frequency", "Code frequency"),
];

/// Pulse windows, as the `period` query names them, with their labels.
pub const PULSE_PERIODS: [(&str, &str); 4] = [
    ("1d", "24 hours"),
    ("3d", "3 days"),
    ("1w", "1 week"),
    ("1m", "1 month"),
];

/// Contributor windows, as the `period` query names them, with their labels.
pub const CONTRIBUTOR_PERIODS: [(&str, &str); 2] = [("all", "All time"), ("year", "Last year")];

/// Whether a section's figures cover all of the history they summarize.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryNote {
    Complete,
    /// Only the newest `commits` commits were read.
    Capped {
        commits: u32,
    },
    /// The history output was too large to summarize; the figures from Git are absent.
    TooLarge,
}

/// A pull request or issue in a list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListedItem {
    pub number: i32,
    pub title: String,
    /// The date the list orders it by, as YYYY-MM-DD.
    pub date: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pulse {
    /// The window key from `PULSE_PERIODS`.
    pub period: String,
    pub merged_pulls: i64,
    pub opened_pulls: i64,
    pub closed_issues: i64,
    pub opened_issues: i64,
    pub merged: Vec<ListedItem>,
    pub issues: Vec<ListedItem>,
    pub commits: u64,
    pub authors: u64,
    pub files: u64,
    pub additions: u64,
    pub deletions: u64,
    pub history: HistoryNote,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContributorRow {
    pub name: String,
    pub commits: u64,
    /// Commits per week for the last 52 weeks, oldest first.
    pub weekly: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Contributors {
    /// The window key from `CONTRIBUTOR_PERIODS`.
    pub period: String,
    /// The most active contributors, at most 100.
    pub rows: Vec<ContributorRow>,
    pub commits: u64,
    pub history: HistoryNote,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeekCount {
    /// The first day of the week, as YYYY-MM-DD.
    pub start: String,
    pub count: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitPattern {
    pub weeks: Vec<WeekCount>,
    /// Commits per weekday (Monday first) and UTC hour, over every commit read.
    pub punch: Vec<Vec<u32>>,
    pub commits: u64,
    pub history: HistoryNote,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeekChange {
    pub start: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodeFrequency {
    pub weeks: Vec<WeekChange>,
    pub additions: u64,
    pub deletions: u64,
    pub history: HistoryNote,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InsightsView {
    /// The repository has no commits yet.
    Empty,
    Pulse(Pulse),
    Contributors(Contributors),
    Commits(CommitPattern),
    CodeFrequency(CodeFrequency),
}

/// One Insights page: the section chosen and what it shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InsightsPage {
    /// The section key from `SECTIONS`.
    pub section: String,
    pub view: InsightsView,
}
