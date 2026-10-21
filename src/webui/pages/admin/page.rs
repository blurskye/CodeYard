//! The administration overview: sign-up requests that wait for a decision, and daily activity
//! charts. Counts with a page of their own (users, storage) are not repeated here.
use super::nav::{admin_layout, Section};
use crate::webui::components::{count_label, format_bytes};
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

/// The timeframes the overview offers. A URL can still name any value `normalize_days` accepts.
const TIMEFRAMES: [i32; 4] = [7, 30, 90, 180];

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Admin {
    /// Sign-up requests waiting for a decision. `None` on a public node, which has none.
    pub pending_requests: Option<usize>,
    /// One point per day, oldest first.
    pub activity: Vec<Activity>,
    pub days: i32,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Activity {
    /// `YYYY-MM-DD`.
    pub date: String,
    pub commits: i64,
    pub pulls_opened: i64,
    pub pulls_merged: i64,
    pub issues_opened: i64,
    pub issues_closed: i64,
    pub runs: i64,
    pub storage_bytes: i64,
}

pub fn normalize_days(raw: Option<&str>) -> i32 {
    raw.and_then(|s| s.parse().ok())
        .filter(|days| [7, 30, 90, 180, 365].contains(days))
        .unwrap_or(30)
}

/// The chart drawing is `days` wide and this tall. It stretches to its box, so only the bars and
/// the baseline are drawn in it; the labels are HTML and keep their shape.
const PLOT_HEIGHT: f64 = 100.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Column {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One column per day, each one unit wide and scaled to the busiest day.
pub(super) fn columns(values: &[i64]) -> Vec<Column> {
    let peak = values.iter().copied().max().unwrap_or(0).max(1) as f64;
    values
        .iter()
        .enumerate()
        .map(|(day, value)| {
            let height = (*value).max(0) as f64 / peak * PLOT_HEIGHT;
            Column {
                x: day as f64 + 0.15,
                y: PLOT_HEIGHT - height,
                width: 0.7,
                height,
            }
        })
        .collect()
}

/// "Oct 3" for a `YYYY-MM-DD` date. Any other text is kept as it is.
fn short_date(value: &str) -> String {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|date| date.format("%b %-d").to_string())
        .unwrap_or_else(|_| value.to_owned())
}

/// What one chart plots: its title, how a day's value reads, and the value of each day.
struct Series {
    id: &'static str,
    title: &'static str,
    label: fn(i64) -> String,
    wide: bool,
    /// Whether the values are bytes, so the axis shows a size rather than a plain number.
    bytes: bool,
    values: Vec<i64>,
}

fn commits(n: i64) -> String {
    count_label(n as usize, "commit", "commits")
}
fn pulls(n: i64) -> String {
    count_label(n as usize, "pull request", "pull requests")
}
fn issues(n: i64) -> String {
    count_label(n as usize, "issue", "issues")
}
fn runs(n: i64) -> String {
    count_label(n as usize, "run", "runs")
}

fn series(activity: &[Activity]) -> [Series; 7] {
    let plain = |id: &'static str,
                 title: &'static str,
                 label: fn(i64) -> String,
                 pick: fn(&Activity) -> i64| Series {
        id,
        title,
        label,
        wide: false,
        bytes: false,
        values: activity.iter().map(pick).collect(),
    };
    [
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
        Series {
            bytes: true,
            ..plain("storage", "Storage added", format_bytes, |p| {
                p.storage_bytes
            })
        },
    ]
}

/// One small daily column chart. The caption carries the window's total, each column names its
/// day and value on hover, and a quiet window says so instead of drawing nothing.
fn chart(series: Series, dates: &[String]) -> Element {
    let label = series.label;
    let total = label(series.values.iter().sum());
    let peak = series.values.iter().copied().max().unwrap_or(0);
    let axis = if series.bytes {
        format_bytes(peak)
    } else {
        peak.to_string()
    };
    let first = dates
        .first()
        .map(|date| short_date(date))
        .unwrap_or_default();
    let last = dates
        .last()
        .map(|date| short_date(date))
        .unwrap_or_default();
    let summary = format!("{}, {first} to {last}: {total}", series.title);
    let title_id = format!("admin-chart-{}", series.id);
    let days = series.values.len();
    let bars: Vec<(Column, String)> = columns(&series.values)
        .into_iter()
        .zip(series.values.iter().zip(dates))
        .map(|(column, (value, date))| (column, format!("{}: {}", short_date(date), label(*value))))
        .collect();
    rsx! {
        figure {
            class: if series.wide { "identity-admin-chart identity-admin-chart-wide" } else { "identity-admin-chart" },
            aria_labelledby: "{title_id}",
            figcaption { class: "identity-admin-chart-caption",
                h3 { id: "{title_id}", "{series.title}" }
                if peak > 0 {
                    span { "{total}" }
                }
            }
            if peak == 0 {
                p { class: "identity-admin-chart-empty", "None in this period" }
            } else {
                div { class: "identity-admin-chart-plot",
                    span { class: "identity-admin-chart-peak", aria_hidden: "true", "{axis}" }
                    svg {
                        view_box: format!("0 0 {days} {PLOT_HEIGHT}"),
                        preserve_aspect_ratio: "none",
                        role: "img",
                        "aria-label": "{summary}",
                        for (index, (column, title)) in bars.into_iter().enumerate() {
                            g { key: "{index}",
                                title { "{title}" }
                                rect { x: index as f64, y: 0.0, width: 1.0, height: PLOT_HEIGHT, class: "identity-admin-chart-hit" }
                                rect { x: column.x, y: column.y, width: column.width, height: column.height, class: "identity-admin-chart-bar" }
                            }
                        }
                    }
                }
            }
            div { class: "identity-admin-chart-dates", aria_hidden: "true",
                span { "{first}" }
                span { "{last}" }
            }
        }
    }
}

/// The activity timeframe. Each choice is a plain link, so it works before scripts load.
pub(super) fn timeframe_control(days: i32) -> Element {
    rsx! {
        nav { class: "identity-admin-period", aria_label: "Activity timeframe",
            for choice in TIMEFRAMES {
                NavLink { href: format!("/admin?days={choice}"), class: "identity-admin-period-link",
                    aria_current: (choice == days).then(|| "page".to_owned()),
                    "{choice} days"
                }
            }
        }
    }
}

pub fn admin_page(model: Admin) -> Element {
    let dates: Vec<String> = model
        .activity
        .iter()
        .map(|point| point.date.clone())
        .collect();
    let pending = model.pending_requests.filter(|pending| *pending > 0);
    admin_layout(
        Section::Overview,
        "Overview",
        &format!(
            "Activity on this installation, which runs Codeyard {}.",
            model.version
        ),
        rsx! {
            if let Some(pending) = pending {
                div { class: "identity-admin-attention", role: "status",
                    p {
                        if pending == 1 { "1 sign-up request is" } else { "{pending} sign-up requests are" }
                        " waiting for a decision."
                    }
                    NavLink { href: "/admin/users", class: "btn btn-sm", "Review" }
                }
            }
            section { class: "identity-admin-activity", aria_labelledby: "admin-activity-title",
                header { class: "identity-admin-panel-header",
                    h2 { id: "admin-activity-title", "Activity" }
                    {timeframe_control(model.days)}
                }
                if model.activity.is_empty() {
                    div { class: "identity-empty", h3 { "No activity yet" } p { "Activity for this timeframe will appear here." } }
                } else {
                    div { class: "identity-admin-charts",
                        for series in series(&model.activity) {
                            {chart(series, &dates)}
                        }
                    }
                }
            }
        },
    )
}
