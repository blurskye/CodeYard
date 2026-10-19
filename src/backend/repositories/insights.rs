//! Repository Insights: turns commit activity and database counts into the figures the pages
//! show. The bucketing is pure and takes `now` as input, so it can be tested without a clock.
use crate::backend::db::repository_insights::{ActivityCounts, NumberedItem};
use crate::backend::git::{CommitActivity, CommitActivityLog, MAX_ACTIVITY_COMMITS};
use crate::webui::pages::repos::insights::{
    CodeFrequency, CommitPattern, ContributorRow, Contributors, HistoryNote, ListedItem, Pulse,
    WeekChange, WeekCount, CONTRIBUTOR_PERIODS, PULSE_PERIODS,
};
use chrono::{DateTime, Datelike, Timelike};
use std::collections::HashMap;

/// Weeks in the weekly charts, the last one being the week that ends now.
pub const WEEKS: usize = 52;
/// Rows the contributor list shows.
pub const CONTRIBUTOR_LIMIT: usize = 100;
/// Pull requests and issues each Pulse list shows; the counts are exact.
pub const LIST_LIMIT: i64 = 20;

const DAY: i64 = 24 * 60 * 60;
const WEEK: i64 = 7 * DAY;
const YEAR: i64 = 365 * DAY;

/// The Pulse window for a `period` query value. Unknown values mean the last 24 hours.
pub fn pulse_period(value: Option<&str>) -> &'static str {
    PULSE_PERIODS
        .iter()
        .map(|(key, _)| *key)
        .find(|key| Some(*key) == value)
        .unwrap_or("1d")
}

/// The Contributors window for a `period` query value. Unknown values mean all time.
pub fn contributor_period(value: Option<&str>) -> &'static str {
    CONTRIBUTOR_PERIODS
        .iter()
        .map(|(key, _)| *key)
        .find(|key| Some(*key) == value)
        .unwrap_or("all")
}

/// The first second of the Pulse window that ends at `now`.
pub fn pulse_since(now: i64, period: &str) -> i64 {
    now - pulse_seconds(period)
}

fn pulse_seconds(period: &str) -> i64 {
    match period {
        "1d" => DAY,
        "3d" => 3 * DAY,
        "1w" => WEEK,
        _ => 30 * DAY,
    }
}

/// The week slot for a commit at `time`, or None when it is older than the weekly charts.
/// Slot `WEEKS - 1` is the week that ends at `now`; a commit dated after `now` counts there.
fn week_slot(now: i64, time: i64) -> Option<usize> {
    let weeks_ago = (now - time).max(0) / WEEK;
    (weeks_ago < WEEKS as i64).then(|| WEEKS - 1 - weeks_ago as usize)
}

/// The first day of each week slot, oldest first.
fn week_starts(now: i64) -> Vec<String> {
    (0..WEEKS)
        .map(|slot| date_label(now - (WEEKS - slot) as i64 * WEEK))
        .collect()
}

fn date_label(time: i64) -> String {
    DateTime::from_timestamp(time, 0)
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn listed(item: NumberedItem) -> ListedItem {
    ListedItem {
        number: item.number,
        title: item.title,
        date: item.at.format("%Y-%m-%d").to_string(),
    }
}

/// Whether the commits read cover everything `since` reaches back to. A capped read is complete
/// for a window only when its oldest commit is at or before the window start.
fn history_note(log: &CommitActivityLog, since: i64) -> HistoryNote {
    let reaches_back = !log.truncated
        || log
            .commits
            .last()
            .is_some_and(|commit| commit.authored_at <= since);
    if reaches_back {
        HistoryNote::Complete
    } else {
        capped()
    }
}

fn history_note_all(log: &CommitActivityLog) -> HistoryNote {
    if log.truncated {
        capped()
    } else {
        HistoryNote::Complete
    }
}

fn capped() -> HistoryNote {
    HistoryNote::Capped {
        commits: MAX_ACTIVITY_COMMITS as u32,
    }
}

/// The Pulse section. `history` is None when Git refused the history as too large.
pub fn pulse(
    now: i64,
    period: &'static str,
    counts: ActivityCounts,
    merged: Vec<NumberedItem>,
    issues: Vec<NumberedItem>,
    history: Option<&CommitActivityLog>,
) -> Pulse {
    let since = pulse_since(now, period);
    let mut pulse = Pulse {
        period: period.to_owned(),
        merged_pulls: counts.merged_pulls,
        opened_pulls: counts.opened_pulls,
        closed_issues: counts.closed_issues,
        opened_issues: counts.opened_issues,
        merged: merged.into_iter().map(listed).collect(),
        issues: issues.into_iter().map(listed).collect(),
        commits: 0,
        authors: 0,
        files: 0,
        additions: 0,
        deletions: 0,
        history: HistoryNote::TooLarge,
    };
    let Some(log) = history else {
        return pulse;
    };
    let window: Vec<&CommitActivity> = log
        .commits
        .iter()
        .filter(|commit| commit.authored_at >= since)
        .collect();
    let mut emails: Vec<&str> = window.iter().map(|c| c.author_email.as_str()).collect();
    emails.sort_unstable();
    emails.dedup();
    pulse.commits = window.len() as u64;
    pulse.authors = emails.len() as u64;
    for commit in &window {
        pulse.files += u64::from(commit.files);
        pulse.additions += commit.additions;
        pulse.deletions += commit.deletions;
    }
    pulse.history = history_note(log, since);
    pulse
}

/// The Contributors section: commit counts per author, with a weekly sparkline for each.
pub fn contributors(
    now: i64,
    period: &'static str,
    history: Option<&CommitActivityLog>,
) -> Contributors {
    let since = if period == "year" {
        now - YEAR
    } else {
        i64::MIN
    };
    let mut rows: HashMap<String, ContributorRow> = HashMap::new();
    let mut commits = 0u64;
    let mut note = HistoryNote::TooLarge;
    if let Some(log) = history {
        note = history_note_all(log);
        for commit in log
            .commits
            .iter()
            .filter(|commit| commit.authored_at >= since)
        {
            commits += 1;
            // The log is newest first, so the first commit seen gives the author's current name.
            let row = rows
                .entry(commit.author_email.to_lowercase())
                .or_insert_with(|| ContributorRow {
                    name: commit.author_name.clone(),
                    commits: 0,
                    weekly: vec![0; WEEKS],
                });
            row.commits += 1;
            if let Some(slot) = week_slot(now, commit.authored_at) {
                row.weekly[slot] += 1;
            }
        }
    }
    let mut rows: Vec<ContributorRow> = rows.into_values().collect();
    rows.sort_by(|a, b| b.commits.cmp(&a.commits).then_with(|| a.name.cmp(&b.name)));
    rows.truncate(CONTRIBUTOR_LIMIT);
    Contributors {
        period: period.to_owned(),
        rows,
        commits,
        history: note,
    }
}

/// The Commits section: commits per week and a weekday-by-hour count over the commits read.
pub fn commit_pattern(now: i64, history: Option<&CommitActivityLog>) -> CommitPattern {
    let starts = week_starts(now);
    let mut counts = vec![0u32; WEEKS];
    let mut punch = vec![vec![0u32; 24]; 7];
    let mut note = HistoryNote::TooLarge;
    let mut commits = 0u64;
    if let Some(log) = history {
        note = history_note_all(log);
        commits = log.commits.len() as u64;
        for commit in &log.commits {
            if let Some(slot) = week_slot(now, commit.authored_at) {
                counts[slot] += 1;
            }
            if let Some(time) = DateTime::from_timestamp(commit.authored_at, 0) {
                punch[time.weekday().num_days_from_monday() as usize][time.hour() as usize] += 1;
            }
        }
    }
    CommitPattern {
        weeks: starts
            .into_iter()
            .zip(counts)
            .map(|(start, count)| WeekCount { start, count })
            .collect(),
        punch,
        commits,
        history: note,
    }
}

/// The Code frequency section: lines added and removed per week.
pub fn code_frequency(now: i64, history: Option<&CommitActivityLog>) -> CodeFrequency {
    let starts = week_starts(now);
    let mut weeks = vec![(0u64, 0u64); WEEKS];
    let mut note = HistoryNote::TooLarge;
    if let Some(log) = history {
        note = history_note_all(log);
        for commit in &log.commits {
            if let Some(slot) = week_slot(now, commit.authored_at) {
                weeks[slot].0 += commit.additions;
                weeks[slot].1 += commit.deletions;
            }
        }
    }
    let additions = weeks.iter().map(|week| week.0).sum();
    let deletions = weeks.iter().map(|week| week.1).sum();
    CodeFrequency {
        weeks: starts
            .into_iter()
            .zip(weeks)
            .map(|(start, (additions, deletions))| WeekChange {
                start,
                additions,
                deletions,
            })
            .collect(),
        additions,
        deletions,
        history: note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn commit(
        email: &str,
        name: &str,
        authored_at: i64,
        additions: u64,
        deletions: u64,
    ) -> CommitActivity {
        CommitActivity {
            author_name: name.into(),
            author_email: email.into(),
            authored_at,
            files: 1,
            additions,
            deletions,
        }
    }

    fn log(commits: Vec<CommitActivity>, truncated: bool) -> CommitActivityLog {
        CommitActivityLog { commits, truncated }
    }

    #[test]
    fn periods_fall_back_to_their_defaults() {
        assert_eq!(pulse_period(Some("1w")), "1w");
        assert_eq!(pulse_period(Some("bogus")), "1d");
        assert_eq!(pulse_period(None), "1d");
        assert_eq!(contributor_period(Some("year")), "year");
        assert_eq!(contributor_period(Some("1w")), "all");
    }

    #[test]
    fn week_slots_count_back_from_now() {
        assert_eq!(week_slot(NOW, NOW), Some(WEEKS - 1));
        assert_eq!(week_slot(NOW, NOW - WEEK + 1), Some(WEEKS - 1));
        assert_eq!(week_slot(NOW, NOW - WEEK), Some(WEEKS - 2));
        assert_eq!(week_slot(NOW, NOW - (WEEKS as i64 - 1) * WEEK), Some(0));
        assert_eq!(week_slot(NOW, NOW - WEEKS as i64 * WEEK), None);
        // A commit dated in the future counts in the current week.
        assert_eq!(week_slot(NOW, NOW + DAY), Some(WEEKS - 1));
    }

    #[test]
    fn pulse_counts_only_the_window_and_flags_capped_history() {
        let history = log(
            vec![
                commit("a@x", "Ann", NOW - 60, 10, 2),
                commit("b@x", "Bo", NOW - 2 * DAY, 5, 0),
                commit("a@x", "Ann", NOW - 10 * DAY, 99, 99),
            ],
            true,
        );
        let counts = ActivityCounts {
            merged_pulls: 3,
            opened_pulls: 4,
            closed_issues: 1,
            opened_issues: 2,
        };
        let day = pulse(NOW, "1d", counts, Vec::new(), Vec::new(), Some(&history));
        assert_eq!((day.commits, day.authors, day.files), (1, 1, 1));
        assert_eq!((day.additions, day.deletions), (10, 2));
        assert_eq!(day.merged_pulls, 3);
        // The capped read reaches the start of a 1-week window, so that window is complete.
        let week = pulse(NOW, "1w", counts, Vec::new(), Vec::new(), Some(&history));
        assert_eq!(week.commits, 2);
        assert_eq!(week.authors, 2);
        assert_eq!(week.history, HistoryNote::Complete);
        // A 1-month window reaches past the oldest commit read, so the figures are capped.
        let month = pulse(NOW, "1m", counts, Vec::new(), Vec::new(), Some(&history));
        assert_eq!(month.commits, 3);
        assert_eq!(
            month.history,
            HistoryNote::Capped {
                commits: MAX_ACTIVITY_COMMITS as u32
            }
        );
    }

    #[test]
    fn missing_history_is_too_large_and_keeps_the_counts() {
        let counts = ActivityCounts {
            opened_issues: 7,
            ..ActivityCounts::default()
        };
        let pulse = pulse(NOW, "1d", counts, Vec::new(), Vec::new(), None);
        assert_eq!(pulse.history, HistoryNote::TooLarge);
        assert_eq!(pulse.opened_issues, 7);
        assert_eq!(pulse.commits, 0);
        assert_eq!(
            contributors(NOW, "all", None).history,
            HistoryNote::TooLarge
        );
        assert_eq!(commit_pattern(NOW, None).history, HistoryNote::TooLarge);
        assert_eq!(code_frequency(NOW, None).history, HistoryNote::TooLarge);
    }

    #[test]
    fn contributors_group_by_email_and_sort_by_commits() {
        let history = log(
            vec![
                commit("a@x", "Ann", NOW - 60, 1, 0),
                commit("B@x", "Bo", NOW - 120, 1, 0),
                commit("A@x", "Ann Old", NOW - 3 * YEAR, 1, 0),
                commit("b@x", "Bo", NOW - 3 * DAY, 1, 0),
                commit("b@x", "Bo", NOW - 4 * DAY, 1, 0),
            ],
            false,
        );
        let all = contributors(NOW, "all", Some(&history));
        assert_eq!(all.commits, 5);
        assert_eq!(all.rows.len(), 2);
        assert_eq!(all.rows[0].name, "Bo");
        assert_eq!(all.rows[0].commits, 3);
        assert_eq!(all.rows[1].name, "Ann");
        assert_eq!(all.rows[1].commits, 2);
        assert_eq!(all.rows[0].weekly.len(), WEEKS);
        assert_eq!(all.rows[0].weekly[WEEKS - 1], 3);
        // The year window leaves out the commit from three years ago.
        let year = contributors(NOW, "year", Some(&history));
        assert_eq!(year.commits, 4);
        assert_eq!(year.history, HistoryNote::Complete);
    }

    #[test]
    fn commit_pattern_buckets_weeks_and_weekday_hours() {
        // 1970-01-05 was a Monday; 1970-01-07 a Wednesday.
        let monday_nine = 4 * DAY + 9 * 3600;
        let wednesday_noon = 6 * DAY + 12 * 3600;
        let history = log(
            vec![
                commit("a@x", "Ann", NOW, 1, 0),
                commit("a@x", "Ann", monday_nine, 1, 0),
                commit("a@x", "Ann", wednesday_noon, 1, 0),
            ],
            false,
        );
        let pattern = commit_pattern(NOW, Some(&history));
        assert_eq!(pattern.weeks.len(), WEEKS);
        assert_eq!(pattern.weeks[WEEKS - 1].count, 1);
        assert_eq!(pattern.commits, 3);
        assert_eq!(pattern.punch[0][9], 1);
        assert_eq!(pattern.punch[2][12], 1);
        assert_eq!(pattern.punch.iter().flatten().sum::<u32>(), 3);
        assert_eq!(pattern.weeks[WEEKS - 1].start.len(), 10);
    }

    #[test]
    fn code_frequency_sums_lines_per_week() {
        let history = log(
            vec![
                commit("a@x", "Ann", NOW - 60, 7, 2),
                commit("a@x", "Ann", NOW - 3 * WEEK, 4, 1),
                commit("a@x", "Ann", NOW - 60 * WEEK, 100, 100),
            ],
            true,
        );
        let frequency = code_frequency(NOW, Some(&history));
        assert_eq!(frequency.weeks.len(), WEEKS);
        assert_eq!(frequency.weeks[WEEKS - 1].additions, 7);
        assert_eq!(frequency.weeks[WEEKS - 4].deletions, 1);
        assert_eq!((frequency.additions, frequency.deletions), (11, 3));
        assert_eq!(
            frequency.history,
            HistoryNote::Capped {
                commits: MAX_ACTIVITY_COMMITS as u32
            }
        );
    }
}
