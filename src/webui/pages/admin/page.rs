//! The administration overview: totals, system status, recent signups and the activity chart.
use super::nav::{admin_header, Section};
use crate::webui::components::{compact_date, count_label, format_bytes};
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

/// The daily table lists this many of the newest days. The chart always shows the whole window.
const TABLE_DAYS: usize = 30;
/// The timeframes the overview offers. A URL can still name any value `normalize_days` accepts.
const TIMEFRAMES: [i32; 4] = [7, 30, 90, 180];

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Admin {
    /// At most five recent signups, newest first.
    pub accounts: Vec<Account>,
    pub users: usize,
    pub banned: usize,
    pub repositories: usize,
    pub organizations: usize,
    /// Signup requests waiting for a decision. `None` on a public node, which has none.
    pub pending_requests: Option<usize>,
    pub storage_bytes: i64,
    pub activity: Vec<Activity>,
    pub days: i32,
    pub status: SystemStatus,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SystemStatus {
    pub version: String,
    pub database: String,
    /// Whether anyone can register, or an administrator approves each request.
    pub private: bool,
    /// The cluster's nodes, or `None` on a single node.
    pub nodes: Option<usize>,
    pub queued_runs: usize,
}

pub fn normalize_days(raw: Option<&str>) -> i32 {
    raw.and_then(|s| s.parse().ok())
        .filter(|days| [7, 30, 90, 180, 365].contains(days))
        .unwrap_or(180)
}
/// Plot area of the activity chart, in its 800 x 200 viewBox.
const PLOT_LEFT: f64 = 48.0;
const PLOT_RIGHT: f64 = 792.0;
const PLOT_TOP: f64 = 12.0;
const PLOT_BOTTOM: f64 = 170.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Column {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One column per day, scaled to the busiest day. An all-zero window draws flat columns.
pub(super) fn columns(values: &[i64]) -> Vec<Column> {
    let peak = values.iter().copied().max().unwrap_or(0).max(1) as f64;
    let slot = (PLOT_RIGHT - PLOT_LEFT) / values.len().max(1) as f64;
    let width = (slot * 0.7).max(1.0);
    values
        .iter()
        .enumerate()
        .map(|(day, value)| {
            let height = (*value).max(0) as f64 / peak * (PLOT_BOTTOM - PLOT_TOP);
            Column {
                x: PLOT_LEFT + day as f64 * slot + (slot - width) / 2.0,
                y: PLOT_BOTTOM - height,
                width,
                height,
            }
        })
        .collect()
}

/// Totals for the cards: contributions, uploaded bytes, and days with any contribution.
pub(super) fn activity_totals(activity: &[Activity]) -> (i64, i64, usize) {
    (
        activity.iter().map(|point| point.contributions).sum(),
        activity.iter().map(|point| point.bytes).sum(),
        activity
            .iter()
            .filter(|point| point.contributions > 0)
            .count(),
    )
}

/// The days that get a date label under the chart: the first and last day, and three evenly
/// spaced days between them.
pub(super) fn date_tick_indices(days: usize) -> Vec<usize> {
    let mut indices: Vec<usize> = if days == 0 {
        Vec::new()
    } else {
        (0..=4).map(|step| step * (days - 1) / 4).collect()
    };
    indices.dedup();
    indices
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
    let values: Vec<i64> = model
        .activity
        .iter()
        .map(|point| point.contributions)
        .collect();
    let day_columns = columns(&values);
    let top = values.iter().copied().max().unwrap_or(0);
    // Each tick is (value, line y, label y). The labels sit a little below their line.
    let ticks = [
        (top, PLOT_TOP, PLOT_TOP + 4.0),
        (
            top / 2,
            (PLOT_TOP + PLOT_BOTTOM) / 2.0,
            (PLOT_TOP + PLOT_BOTTOM) / 2.0 + 4.0,
        ),
        (0, PLOT_BOTTOM, PLOT_BOTTOM + 4.0),
    ];
    let (contributions, bytes, active_days) = activity_totals(&model.activity);
    let table_rows = &model.activity[model.activity.len().saturating_sub(TABLE_DAYS)..];
    let first_day = model
        .activity
        .first()
        .map_or("", |point| point.date.as_str());
    let last_day = model
        .activity
        .last()
        .map_or("", |point| point.date.as_str());
    let last_index = model.activity.len().saturating_sub(1);
    let date_labels: Vec<(f64, &str, &str)> = date_tick_indices(model.activity.len())
        .into_iter()
        .map(|index| {
            let column = day_columns[index];
            let anchor = if index == 0 {
                "start"
            } else if index == last_index {
                "end"
            } else {
                "middle"
            };
            (
                column.x + column.width / 2.0,
                anchor,
                model.activity[index].date.as_str(),
            )
        })
        .collect();
    let status = &model.status;
    rsx! {
        div { class: "identity-settings identity-admin page-shell",
            {admin_header(Section::Overview, "Overview", "Activity and status of this installation.")}
            div { class: "identity-admin-totals",
                div { class: "identity-admin-total", span { "Users" } strong { "{model.users}" } }
                div { class: "identity-admin-total", span { "Banned" } strong { "{model.banned}" } }
                div { class: "identity-admin-total", span { "Repositories" } strong { "{model.repositories}" } }
                div { class: "identity-admin-total", span { "Organizations" } strong { "{model.organizations}" } }
                div { class: "identity-admin-total", span { "Storage used" } strong { {format_bytes(model.storage_bytes)} } }
                if let Some(pending) = model.pending_requests {
                    div { class: "identity-admin-total",
                        span { "Pending requests" }
                        strong { "{pending}" }
                        if pending > 0 {
                            NavLink { href: "/admin/users".to_string(), class: "identity-account-name", "Review requests" }
                        }
                    }
                }
            }
            div { class: "identity-admin-columns",
                section { class: "identity-admin-panel", aria_labelledby: "admin-status-title",
                    header { class: "identity-admin-panel-header",
                        div { h2 { id: "admin-status-title", "System status" } p { "This node and its cluster." } }
                    }
                    ul { class: "identity-admin-list",
                        li { span { "Version" } span { class: "identity-muted", "{status.version}" } }
                        li { span { "Database" } span { class: "identity-muted", "{status.database}" } }
                        li { span { "Sign-up" } span { class: "identity-muted", if status.private { "By approval" } else { "Open" } } }
                        li { span { "Nodes" } span { class: "identity-muted",
                            if let Some(nodes) = status.nodes { {count_label(nodes, "cluster node", "cluster nodes")} } else { "Single node" }
                        } }
                        li { span { "CI queue" } span { class: "identity-muted", {count_label(status.queued_runs, "queued run", "queued runs")} } }
                    }
                }
                section { class: "identity-admin-panel", aria_labelledby: "admin-signups-title",
                    header { class: "identity-admin-panel-header",
                        div { h2 { id: "admin-signups-title", "Recent signups" } p { "The newest accounts." } }
                    }
                    ul { class: "identity-admin-list",
                        for account in &model.accounts {
                            li { key: "{account.id}",
                                a { class: "identity-account-name", href: format!("/{}", crate::webui::encode_component(&account.username)), "{account.username}" }
                                span { class: "identity-muted", {compact_date(&account.created)} }
                            }
                        }
                    }
                }
            }
            section { class: "identity-admin-panel", aria_labelledby: "admin-activity-title",
                header { class: "identity-admin-panel-header",
                    div { h2 { id: "admin-activity-title", "Activity" } p { "Contributions and uploaded storage over time." } }
                    {timeframe_control(model.days)}
                }
                if model.activity.is_empty() {
                    div { class: "identity-empty", h3 { "No activity yet" } p { "Activity for this timeframe will appear here." } }
                } else {
                    div { class: "identity-admin-totals",
                        div { class: "identity-admin-total", span { "Contributions" } strong { "{contributions}" } }
                        div { class: "identity-admin-total", span { "Storage uploaded" } strong { {format_bytes(bytes)} } }
                        div { class: "identity-admin-total", span { "Active days" } strong { "{active_days}" } }
                    }
                    div { class: "identity-admin-chart-scroll",
                        svg { view_box: "0 0 800 200", role: "img", "aria-label": "Daily contributions from {first_day} to {last_day}", class: "identity-admin-chart",
                            for (value, line_y, label_y) in ticks {
                                line { x1: PLOT_LEFT, x2: PLOT_RIGHT, y1: line_y, y2: line_y, class: "identity-admin-grid" }
                                text { x: PLOT_LEFT - 8.0, y: label_y, text_anchor: "end", class: "identity-admin-axis", "{value}" }
                            }
                            for column in day_columns {
                                rect { x: column.x, y: column.y, width: column.width, height: column.height, class: "identity-admin-bar" }
                            }
                            for (index, (x, anchor, date)) in date_labels.into_iter().enumerate() {
                                text { x: x, y: 190.0, text_anchor: anchor, class: if index % 2 == 1 { "identity-admin-axis identity-admin-axis-minor" } else { "identity-admin-axis" }, "{date}" }
                            }
                        }
                    }
                    details { class: "identity-admin-daily-details",
                        summary { "Daily figures" }
                        div { class: "identity-table-wrap", table { class: "identity-table",
                            caption { class: "identity-sr-only", "Daily and cumulative activity" }
                            thead { tr { th { "Date" } th { class: "identity-num", "Contributions" } th { class: "identity-num", "Total" } th { class: "identity-num", "Storage" } th { class: "identity-num", "Storage total" } } }
                            tbody { for point in table_rows { tr {
                                td { "{point.date}" } td { class: "identity-num", "{point.contributions}" } td { class: "identity-num", "{point.cumulative_contributions}" }
                                td { class: "identity-num", {format_bytes(point.bytes)} } td { class: "identity-num", {format_bytes(point.cumulative_bytes)} }
                            } } }
                        } }
                        if model.activity.len() > TABLE_DAYS {
                            p { class: "identity-muted", "Latest {TABLE_DAYS} of {model.activity.len()} days. The chart shows every day." }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Account {
    pub id: i64,
    pub username: String,
    pub created: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Activity {
    pub date: String,
    pub contributions: i64,
    pub cumulative_contributions: i64,
    pub bytes: i64,
    pub cumulative_bytes: i64,
}
