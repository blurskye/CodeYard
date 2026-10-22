//! Small column charts, one column per day or week, and the timeframe links above them. The
//! admin overview and a repository's insights draw their activity with these.
use crate::webui::components::{count_label, format_bytes};
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

/// The timeframes a page offers. A URL can still name any value `normalize_days` accepts.
const TIMEFRAMES: [i32; 4] = [7, 30, 90, 180];

pub fn normalize_days(raw: Option<&str>) -> i32 {
    raw.and_then(|s| s.parse().ok())
        .filter(|days| [7, 30, 90, 180, 365].contains(days))
        .unwrap_or(30)
}

/// What happened on one day, counted from the database.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DailyActivity {
    /// `YYYY-MM-DD`.
    pub date: String,
    pub commits: i64,
    pub pulls_opened: i64,
    pub pulls_merged: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
    pub runs: i64,
}

/// The chart drawing is one unit wide per column and this tall. It stretches to its box, so
/// only the bars are drawn in it; the labels are HTML and keep their shape.
const PLOT_HEIGHT: f64 = 100.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Column {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One column per value, each one unit wide and scaled to the largest.
pub(crate) fn columns(values: &[i64]) -> Vec<Column> {
    let peak = values.iter().copied().max().unwrap_or(0).max(1) as f64;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let height = (*value).max(0) as f64 / peak * PLOT_HEIGHT;
            Column {
                x: index as f64 + 0.15,
                y: PLOT_HEIGHT - height,
                width: 0.7,
                height,
            }
        })
        .collect()
}

/// "Oct 3" for a `YYYY-MM-DD` date, or "Oct 3, 2026" when the chart spans two years. Any other
/// text is kept as it is.
fn short_date(value: &str, with_year: bool) -> String {
    let format = if with_year { "%b %-d, %Y" } else { "%b %-d" };
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|date| date.format(format).to_string())
        .unwrap_or_else(|_| value.to_owned())
}

/// What one chart plots: its title, how a column's value reads, and the value of each column.
pub struct Series {
    pub id: &'static str,
    pub title: &'static str,
    pub label: fn(i64) -> String,
    /// Takes a whole row of the chart grid.
    pub wide: bool,
    /// The values are bytes, so the axis shows a size rather than a plain number.
    pub bytes: bool,
    /// Each column is the week that starts on its date.
    pub weekly: bool,
    pub values: Vec<i64>,
}

impl Series {
    pub fn new(
        id: &'static str,
        title: &'static str,
        label: fn(i64) -> String,
        values: Vec<i64>,
    ) -> Self {
        Self {
            id,
            title,
            label,
            wide: false,
            bytes: false,
            weekly: false,
            values,
        }
    }
}

pub fn commits(n: i64) -> String {
    count_label(n as usize, "commit", "commits")
}
pub fn pulls(n: i64) -> String {
    count_label(n as usize, "pull request", "pull requests")
}
pub fn issues(n: i64) -> String {
    count_label(n as usize, "issue", "issues")
}
pub fn runs(n: i64) -> String {
    count_label(n as usize, "run", "runs")
}
pub fn lines(n: i64) -> String {
    count_label(n as usize, "line", "lines")
}

/// The charts of daily activity: commits across the whole row, then pull requests, issues and
/// workflow runs in pairs.
pub fn activity_series(activity: &[DailyActivity]) -> Vec<Series> {
    let plain = |id, title, label, pick: fn(&DailyActivity) -> i64| {
        Series::new(id, title, label, activity.iter().map(pick).collect())
    };
    vec![
        Series {
            wide: true,
            ..plain("commits", "Commits pushed", commits, |p| p.commits)
        },
        plain("pulls-opened", "Pull requests opened", pulls, |p| {
            p.pulls_opened
        }),
        plain("pulls-merged", "Pull requests merged", pulls, |p| {
            p.pulls_merged
        }),
        plain("issues-opened", "Issues opened", issues, |p| {
            p.issues_opened
        }),
        plain("issues-closed", "Issues closed", issues, |p| {
            p.issues_closed
        }),
        plain("runs", "Workflow runs", runs, |p| p.runs),
    ]
}

/// One small column chart. The caption carries the window's total, each column names its day
/// and value on hover, and a quiet window says so instead of drawing nothing.
pub fn column_chart(series: Series, dates: &[String]) -> Element {
    let label = series.label;
    let total = label(series.values.iter().sum());
    let peak = series.values.iter().copied().max().unwrap_or(0);
    let axis = if series.bytes {
        format_bytes(peak)
    } else {
        peak.to_string()
    };
    let with_year =
        dates.first().and_then(|date| date.get(..4)) != dates.last().and_then(|date| date.get(..4));
    let first = dates
        .first()
        .map(|date| short_date(date, with_year))
        .unwrap_or_default();
    let last = dates
        .last()
        .map(|date| short_date(date, with_year))
        .unwrap_or_default();
    let summary = format!("{}, {first} to {last}: {total}", series.title);
    let title_id = format!("chart-{}", series.id);
    let count = series.values.len();
    let prefix = if series.weekly { "Week of " } else { "" };
    let bars: Vec<(Column, String)> = columns(&series.values)
        .into_iter()
        .zip(series.values.iter().zip(dates))
        .map(|(column, (value, date))| {
            let text = format!("{prefix}{}: {}", short_date(date, with_year), label(*value));
            (column, text)
        })
        .collect();
    rsx! {
        figure {
            class: if series.wide { "column-chart column-chart-wide" } else { "column-chart" },
            aria_labelledby: "{title_id}",
            figcaption { class: "column-chart-caption",
                h3 { id: "{title_id}", "{series.title}" }
                if peak > 0 {
                    span { "{total}" }
                }
            }
            if peak == 0 {
                p { class: "column-chart-empty", "None in this period" }
            } else {
                div { class: "column-chart-plot",
                    span { class: "column-chart-peak", aria_hidden: "true", "{axis}" }
                    svg {
                        view_box: format!("0 0 {count} {PLOT_HEIGHT}"),
                        preserve_aspect_ratio: "none",
                        role: "img",
                        "aria-label": "{summary}",
                        for (index, (column, title)) in bars.into_iter().enumerate() {
                            g { key: "{index}",
                                title { "{title}" }
                                rect { x: index as f64, y: 0.0, width: 1.0, height: PLOT_HEIGHT, class: "column-chart-hit" }
                                rect { x: column.x, y: column.y, width: column.width, height: column.height, class: "column-chart-bar" }
                            }
                        }
                    }
                }
            }
            div { class: "column-chart-dates", aria_hidden: "true",
                span { "{first}" }
                span { "{last}" }
            }
        }
    }
}

/// Charts in a grid: two to a row on a wide screen, one on a phone.
pub fn chart_grid(series: Vec<Series>, dates: &[String]) -> Element {
    rsx! {
        div { class: "column-charts",
            for series in series {
                {column_chart(series, dates)}
            }
        }
    }
}

/// The timeframe choices for the page at `path`. Each is a plain link, so it works before
/// scripts load.
pub fn timeframe_links(path: &str, days: i32) -> Element {
    rsx! {
        nav { class: "chart-period", aria_label: "Timeframe",
            for choice in TIMEFRAMES {
                NavLink { href: format!("{path}?days={choice}"), class: "chart-period-link",
                    aria_current: (choice == days).then(|| "page".to_owned()),
                    "{choice} days"
                }
            }
        }
    }
}
