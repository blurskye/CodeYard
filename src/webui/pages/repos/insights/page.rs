//! The Insights page: a section nav (a picker on phones), period links and inline SVG charts.
//! It works without JavaScript; each chart has its figures in text beside it.
use super::model::*;
use crate::webui::navigation::NavLink;
use crate::webui::pages::repos::chrome::{chrome, RepoChrome};
use dioxus::prelude::*;

const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const CHART_WIDTH: f64 = 520.0;
const CHART_HEIGHT: f64 = 160.0;

/// One bar: its left edge, width and height, in the chart's viewBox.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bar {
    x: f64,
    width: f64,
    height: f64,
}

/// Bars for `values`, one slot each, scaled so that `peak` fills `max_height`. A zero peak draws
/// no bars.
fn bars(values: &[u64], peak: u64, max_height: f64) -> Vec<Bar> {
    let slot = CHART_WIDTH / values.len().max(1) as f64;
    let width = (slot * 0.7).max(1.0);
    values
        .iter()
        .enumerate()
        .map(|(index, value)| Bar {
            x: index as f64 * slot + (slot - width) / 2.0,
            width,
            height: if peak == 0 {
                0.0
            } else {
                *value as f64 / peak as f64 * max_height
            },
        })
        .collect()
}

/// The percentage of `max` that `value` fills, for the contributor bars.
fn percent(value: u64, max: u64) -> u64 {
    value.saturating_mul(100).checked_div(max).unwrap_or(0)
}

fn history_note(history: HistoryNote) -> Element {
    match history {
        HistoryNote::Complete => rsx! {},
        HistoryNote::Capped { commits } => rsx! {
            p { class: "repo-insights-note", "Based on the most recent {commits} commits." }
        },
        HistoryNote::TooLarge => rsx! {
            p { class: "repo-alert repo-alert-error", role: "status", "This history is too large to summarize. Counts from pull requests and issues are still exact." }
        },
    }
}

fn period_links(base: &str, section: &str, periods: &[(&str, &str)], current: &str) -> Element {
    rsx! {
        nav { class: "repo-insights-periods", aria_label: "Period",
            for (key, label) in periods.iter().copied() {
                NavLink {
                    href: format!("{base}/insights?section={section}&period={key}"),
                    class: "repo-insights-period".to_owned(),
                    aria_current: (key == current).then(|| "page".to_owned()),
                    "{label}"
                }
            }
        }
    }
}

fn stat(label: &str, value: String) -> Element {
    rsx! {
        div { class: "repo-insights-stat",
            span { "{label}" }
            strong { "{value}" }
        }
    }
}

fn listed_items(base: &str, kind: &str, items: &[ListedItem], empty: &str) -> Element {
    rsx! {
        if items.is_empty() {
            p { class: "repo-list-empty", "{empty}" }
        } else {
            ul { class: "repo-insights-list",
                for item in items.iter() {
                    li {
                        a { href: format!("{base}/{kind}/{}", item.number), {format!("#{} {}", item.number, item.title)} }
                        span { class: "repo-insights-date", "{item.date}" }
                    }
                }
            }
        }
    }
}

fn pulse_view(base: &str, pulse: &Pulse) -> Element {
    rsx! {
        {period_links(base, "pulse", &PULSE_PERIODS, &pulse.period)}
        {history_note(pulse.history)}
        div { class: "repo-insights-stats",
            {stat("Merged pull requests", pulse.merged_pulls.to_string())}
            {stat("Opened pull requests", pulse.opened_pulls.to_string())}
            {stat("Closed issues", pulse.closed_issues.to_string())}
            {stat("Opened issues", pulse.opened_issues.to_string())}
            {stat("Commits", pulse.commits.to_string())}
            {stat("Authors", pulse.authors.to_string())}
            {stat("Files changed", pulse.files.to_string())}
            {stat("Additions", format!("+{}", pulse.additions))}
            {stat("Deletions", format!("-{}", pulse.deletions))}
        }
        section { class: "repo-settings-section panel",
            header { class: "repo-section-heading", div { h3 { "Merged pull requests" } } }
            {listed_items(base, "pulls", &pulse.merged, "No pull requests were merged in this period.")}
        }
        section { class: "repo-settings-section panel",
            header { class: "repo-section-heading", div { h3 { "Opened issues" } } }
            {listed_items(base, "issues", &pulse.issues, "No issues were opened in this period.")}
        }
    }
}

fn contributors_view(base: &str, contributors: &Contributors) -> Element {
    let max = contributors.rows.first().map_or(0, |row| row.commits);
    rsx! {
        {period_links(base, "contributors", &CONTRIBUTOR_PERIODS, &contributors.period)}
        {history_note(contributors.history)}
        if contributors.rows.is_empty() {
            p { class: "repo-list-empty", "No commits in this period." }
        } else {
            div { class: "identity-table-wrap",
                table { class: "identity-table repo-insights-table",
                    caption { class: "identity-sr-only", "Commits per contributor on the default branch" }
                    thead { tr { th { "Contributor" } th { class: "identity-num", "Commits" } th { "Last 52 weeks" } } }
                    tbody {
                        for row in contributors.rows.iter() {
                            {
                                let width = percent(row.commits, max);
                                let peak = row.weekly.iter().copied().max().unwrap_or(0);
                                let spark = bars(&row.weekly.iter().map(|value| u64::from(*value)).collect::<Vec<_>>(), u64::from(peak), 16.0);
                                rsx! {
                                    tr {
                                        td { "{row.name}" }
                                        td { class: "identity-num",
                                            span { class: "repo-insights-bar-track", span { class: "repo-insights-bar", style: "width: {width}%" } }
                                            span { class: "repo-insights-count", "{row.commits}" }
                                        }
                                        td {
                                            svg { view_box: "0 0 {CHART_WIDTH} 16", preserve_aspect_ratio: "none", role: "img", "aria-label": "{row.commits} commits over the last 52 weeks", class: "repo-insights-spark",
                                                for bar in spark {
                                                    rect { x: bar.x, y: 16.0 - bar.height, width: bar.width, height: bar.height, class: "repo-insights-bar-fill" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn commits_view(pattern: &CommitPattern) -> Element {
    let counts: Vec<u64> = pattern
        .weeks
        .iter()
        .map(|week| u64::from(week.count))
        .collect();
    let peak = counts.iter().copied().max().unwrap_or(0);
    let columns = bars(&counts, peak, CHART_HEIGHT - 12.0);
    let first = pattern
        .weeks
        .first()
        .map(|week| week.start.clone())
        .unwrap_or_default();
    let last = pattern
        .weeks
        .last()
        .map(|week| week.start.clone())
        .unwrap_or_default();
    let cell_peak = pattern.punch.iter().flatten().copied().max().unwrap_or(0);
    let commits = pattern.commits;
    rsx! {
        {history_note(pattern.history)}
        section { class: "repo-settings-section panel",
            header { class: "repo-section-heading", div { h3 { "Commits per week" } p { "The last 52 weeks on the default branch." } } }
            div { class: "repo-insights-chart-scroll",
                svg { view_box: "0 0 {CHART_WIDTH} {CHART_HEIGHT}", role: "img", "aria-label": "Commits per week from {first} to {last}", class: "repo-insights-chart",
                    for bar in columns {
                        rect { x: bar.x, y: CHART_HEIGHT - 12.0 - bar.height, width: bar.width, height: bar.height, class: "repo-insights-bar-fill" }
                    }
                }
                div { class: "repo-insights-axis",
                    span { "{first}" }
                    span { "{last}" }
                }
            }
            details { class: "repo-insights-details",
                summary { "Weekly figures" }
                div { class: "identity-table-wrap",
                    table { class: "identity-table",
                        caption { class: "identity-sr-only", "Commits per week" }
                        thead { tr { th { "Week of" } th { class: "identity-num", "Commits" } } }
                        tbody { for week in pattern.weeks.iter() { tr { td { "{week.start}" } td { class: "identity-num", "{week.count}" } } } }
                    }
                }
            }
        }
        section { class: "repo-settings-section panel",
            header { class: "repo-section-heading", div { h3 { "Punch card" } p { "Commits by weekday and hour (UTC) across the {commits} commits read." } } }
            div { class: "repo-insights-punch", role: "img", "aria-label": "Commits by weekday and hour, UTC",
                div { class: "repo-insights-punch-hours", aria_hidden: "true",
                    span {}
                    for hour in 0..24 { span { if hour % 6 == 0 { "{hour}" } } }
                }
                for (day, hours) in pattern.punch.iter().enumerate() {
                    div { class: "repo-insights-punch-row",
                        span { class: "repo-insights-punch-day", {DAY_NAMES[day]} }
                        for (hour, count) in hours.iter().copied().enumerate() {
                            span {
                                class: "repo-insights-cell",
                                style: if count == 0 { "opacity: 0.08".to_owned() } else { format!("opacity: {}", 0.25 + 0.75 * count as f64 / cell_peak as f64) },
                                title: format!("{} {hour:02}:00 UTC: {count} commits", DAY_NAMES[day]),
                            }
                        }
                    }
                }
            }
        }
    }
}

fn code_frequency_view(frequency: &CodeFrequency) -> Element {
    let additions: Vec<u64> = frequency.weeks.iter().map(|week| week.additions).collect();
    let deletions: Vec<u64> = frequency.weeks.iter().map(|week| week.deletions).collect();
    let peak = additions
        .iter()
        .chain(&deletions)
        .copied()
        .max()
        .unwrap_or(0);
    let half = CHART_HEIGHT / 2.0 - 6.0;
    let up = bars(&additions, peak, half);
    let down = bars(&deletions, peak, half);
    let first = frequency
        .weeks
        .first()
        .map(|week| week.start.clone())
        .unwrap_or_default();
    let last = frequency
        .weeks
        .last()
        .map(|week| week.start.clone())
        .unwrap_or_default();
    let mid = CHART_HEIGHT / 2.0;
    rsx! {
        {history_note(frequency.history)}
        div { class: "repo-insights-stats",
            {stat("Additions, last 52 weeks", format!("+{}", frequency.additions))}
            {stat("Deletions, last 52 weeks", format!("-{}", frequency.deletions))}
        }
        section { class: "repo-settings-section panel",
            header { class: "repo-section-heading", div { h3 { "Code frequency" } p { "Lines added above the line and removed below it, per week." } } }
            div { class: "repo-insights-chart-scroll",
                svg { view_box: "0 0 {CHART_WIDTH} {CHART_HEIGHT}", role: "img", "aria-label": "Lines added and removed per week from {first} to {last}", class: "repo-insights-chart",
                    line { x1: 0.0, x2: CHART_WIDTH, y1: mid, y2: mid, class: "repo-insights-baseline" }
                    for bar in up {
                        rect { x: bar.x, y: mid - bar.height, width: bar.width, height: bar.height, class: "repo-insights-additions" }
                    }
                    for bar in down {
                        rect { x: bar.x, y: mid, width: bar.width, height: bar.height, class: "repo-insights-deletions" }
                    }
                }
                div { class: "repo-insights-axis",
                    span { "{first}" }
                    span { "{last}" }
                }
            }
        }
    }
}

/// The Insights page for one repository. `page` carries the section and what it shows.
pub fn insights(repo: &RepoChrome, page: &InsightsPage) -> Element {
    let base = repo.base();
    let section = page.section.as_str();
    chrome(
        repo,
        rsx! {
            section { class: "repo-settings-page repo-insights-page",
                header { class: "repo-page-heading", div { h2 { "Insights" } p { "Activity and contributions on this repository's default branch." } } }
                div { class: "repo-settings-layout",
                    nav { class: "repo-settings-nav", aria_label: "Insights sections",
                        for (key, label) in SECTIONS.iter().copied() {
                            NavLink {
                                href: format!("{base}/insights?section={key}"),
                                class: String::new(),
                                aria_current: (key == section).then(|| "page".to_owned()),
                                "{label}"
                            }
                        }
                    }
                    details { class: "native-menu repo-menu repo-settings-picker",
                        summary { class: "btn btn-secondary", aria_label: "Insights sections",
                            span { {SECTIONS.iter().find(|(key, _)| *key == section).map_or("Insights", |(_, label)| *label)} }
                        }
                        div { class: "repo-menu-panel",
                            for (key, label) in SECTIONS.iter().copied() {
                                NavLink {
                                    class: "repo-menu-item".to_owned(),
                                    href: format!("{base}/insights?section={key}"),
                                    aria_current: (key == section).then(|| "page".to_owned()),
                                    "{label}"
                                }
                            }
                        }
                    }
                    div { class: "repo-settings-content",
                        {match &page.view {
                            InsightsView::Empty => rsx! {
                                div { class: "identity-empty", h3 { "No commits yet" } p { "Insights appear after the first push to this repository." } }
                            },
                            InsightsView::Pulse(pulse) => pulse_view(&base, pulse),
                            InsightsView::Contributors(contributors) => contributors_view(&base, contributors),
                            InsightsView::Commits(pattern) => commits_view(pattern),
                            InsightsView::CodeFrequency(frequency) => code_frequency_view(frequency),
                        }}
                    }
                }
            }
        },
    )
}
