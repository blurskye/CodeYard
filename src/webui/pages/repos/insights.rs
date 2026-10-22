//! Repository insights: daily activity, contributors, weekly code frequency, workflow usage and
//! forks, each on its own page beside a section list, as GitHub lays them out.
use crate::webui::components::charts::{
    activity_series, chart_grid, lines, timeframe_links, DailyActivity, Series,
};
use crate::webui::components::{compact_date, count_label, Avatar};
use crate::webui::encode_component;
use crate::webui::navigation::NavLink;
use crate::webui::pages::repos::actions::{logs::duration, workflow_name};
use crate::webui::pages::repos::chrome::{chrome, section_menu, RepoChrome};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Pulse {
        days: i32,
        activity: Vec<DailyActivity>,
    },
    Contributors {
        rows: Vec<Contributor>,
    },
    CodeFrequency {
        weeks: Vec<Week>,
    },
    Actions {
        days: i32,
        workflows: Vec<WorkflowUsage>,
    },
    Forks {
        rows: Vec<Fork>,
        page: i64,
        has_more: bool,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Contributor {
    pub name: String,
    /// The account the commits' email belongs to, when there is one.
    pub username: Option<String>,
    pub commits: i64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Week {
    /// The Monday that starts the week, `YYYY-MM-DD`.
    pub week: String,
    pub additions: i64,
    pub deletions: i64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkflowUsage {
    pub path: String,
    pub runs: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub average_seconds: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Fork {
    pub owner: String,
    pub name: String,
    pub description: String,
    pub stars: i32,
    pub updated_at: String,
}

/// Contributors shown on the page; the rest are counted below the list.
pub const CONTRIBUTORS_SHOWN: usize = 100;
pub const FORKS_PER_PAGE: i64 = 30;

const SECTIONS: [(&str, &str); 5] = [
    ("", "Pulse"),
    ("/contributors", "Contributors"),
    ("/code-frequency", "Code frequency"),
    ("/actions", "Actions usage"),
    ("/forks", "Forks"),
];

impl Page {
    fn suffix(&self) -> &'static str {
        match self {
            Page::Pulse { .. } => "",
            Page::Contributors { .. } => "/contributors",
            Page::CodeFrequency { .. } => "/code-frequency",
            Page::Actions { .. } => "/actions",
            Page::Forks { .. } => "/forks",
        }
    }
}

pub fn render(repo: &RepoChrome, page: Page) -> Element {
    let base = format!("{}/insights", repo.base());
    let current = page.suffix();
    let links: Vec<(String, &str, bool)> = SECTIONS
        .iter()
        .map(|(suffix, label)| (format!("{base}{suffix}"), *label, *suffix == current))
        .collect();
    let path = format!("{base}{current}");
    let body = match page {
        Page::Pulse { days, activity } => pulse(&path, days, &activity),
        Page::Contributors { rows } => contributors(&rows),
        Page::CodeFrequency { weeks } => code_frequency(repo, &weeks),
        Page::Actions { days, workflows } => actions(repo, &path, days, &workflows),
        Page::Forks {
            rows,
            page,
            has_more,
        } => forks(&path, &rows, page, has_more),
    };
    chrome(
        repo,
        rsx! {
            section { class: "repo-insights-page",
                div { class: "repo-settings-layout",
                    nav { class: "repo-settings-nav", aria_label: "Insights sections",
                        for (href, label, current) in links.iter() {
                            NavLink { key: "{href}", href: href.clone(), aria_current: current.then(|| "page".to_owned()), "{label}" }
                        }
                    }
                    {section_menu("Insights sections", &links)}
                    div { class: "repo-settings-content repo-insights-content", {body} }
                }
            }
        },
    )
}

fn heading(title: &str, intro: &str, controls: Option<Element>) -> Element {
    rsx! {
        header { class: "repo-insights-heading",
            div { h2 { "{title}" } p { "{intro}" } }
            if let Some(controls) = controls { {controls} }
        }
    }
}

fn empty(text: &str) -> Element {
    rsx! { div { class: "repo-empty panel", p { "{text}" } } }
}

fn pulse(path: &str, days: i32, activity: &[DailyActivity]) -> Element {
    let dates: Vec<String> = activity.iter().map(|point| point.date.clone()).collect();
    rsx! {
        {heading("Pulse", "What happened in this repository each day.", Some(timeframe_links(path, days)))}
        {chart_grid(activity_series(activity), &dates)}
    }
}

fn contributors(rows: &[Contributor]) -> Element {
    let top = rows.first().map_or(1, |row| row.commits.max(1));
    let hidden = rows.len().saturating_sub(CONTRIBUTORS_SHOWN);
    rsx! {
        {heading("Contributors", "Authors of the commits on every branch, by number of commits.", None)}
        if rows.is_empty() {
            {empty("No commits yet.")}
        } else {
            ol { class: "repo-insights-list panel",
                for (index, row) in rows.iter().take(CONTRIBUTORS_SHOWN).enumerate() {
                    li { key: "{index}", class: "repo-insights-contributor",
                        span { class: "repo-insights-rank", "{index + 1}" }
                        if let Some(username) = &row.username {
                            NavLink { href: format!("/{}", encode_component(username)), class: "repo-insights-who",
                                Avatar { name: username.clone(), size: 20 }
                                span { "{row.name}" }
                            }
                        } else {
                            span { class: "repo-insights-who", span { "{row.name}" } }
                        }
                        span { class: "repo-insights-meter", aria_hidden: "true",
                            span { style: format!("width: {:.1}%", row.commits as f64 * 100.0 / top as f64) }
                        }
                        span { class: "repo-insights-count", {count_label(row.commits as usize, "commit", "commits")} }
                    }
                }
            }
            if hidden > 0 {
                p { class: "repo-insights-note", {count_label(hidden, "more contributor", "more contributors")} }
            }
        }
    }
}

fn code_frequency(repo: &RepoChrome, weeks: &[Week]) -> Element {
    let dates: Vec<String> = weeks.iter().map(|week| week.week.clone()).collect();
    let series = |id, title, values: Vec<i64>| Series {
        wide: true,
        weekly: true,
        ..Series::new(id, title, lines, values)
    };
    let charts = vec![
        series(
            "additions",
            "Lines added",
            weeks.iter().map(|week| week.additions).collect(),
        ),
        series(
            "deletions",
            "Lines removed",
            weeks.iter().map(|week| week.deletions).collect(),
        ),
    ];
    let intro = format!(
        "Lines added and removed on {} each week over the past year. Merge commits are not counted.",
        repo.default_branch
    );
    rsx! {
        {heading("Code frequency", &intro, None)}
        {chart_grid(charts, &dates)}
    }
}

fn actions(repo: &RepoChrome, path: &str, days: i32, workflows: &[WorkflowUsage]) -> Element {
    let base = repo.base();
    rsx! {
        {heading("Actions usage", "Workflow runs started in this timeframe, by workflow.", Some(timeframe_links(path, days)))}
        if workflows.is_empty() {
            {empty("No workflow runs in this timeframe.")}
        } else {
            ul { class: "repo-insights-list panel",
                for usage in workflows.iter() {
                    li { key: "{usage.path}", class: "repo-insights-workflow",
                        NavLink { href: format!("{base}/actions?workflow={}", encode_component(&usage.path)), class: "repo-insights-who",
                            span { "{workflow_name(&usage.path)}" }
                        }
                        span { class: "repo-insights-stats",
                            span { {count_label(usage.runs as usize, "run", "runs")} }
                            if usage.succeeded > 0 {
                                span { "{usage.succeeded} succeeded" }
                            }
                            if usage.failed > 0 {
                                span { "{usage.failed} failed" }
                            }
                            if let Some(seconds) = usage.average_seconds {
                                span { "{duration(seconds)} on average" }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn forks(path: &str, rows: &[Fork], page: i64, has_more: bool) -> Element {
    rsx! {
        {heading("Forks", "Copies of this repository that you can see, newest first.", None)}
        if rows.is_empty() {
            {empty(if page > 1 { "No more forks." } else { "No one has forked this repository yet." })}
        } else {
            ul { class: "repo-insights-list panel",
                for fork in rows.iter() {
                    li { key: "{fork.owner}/{fork.name}", class: "repo-insights-fork",
                        div { class: "repo-insights-who",
                            Avatar { name: fork.owner.clone(), size: 20 }
                            NavLink { href: format!("/{}/{}", encode_component(&fork.owner), encode_component(&fork.name)), "{fork.owner}/{fork.name}" }
                        }
                        if !fork.description.is_empty() {
                            p { class: "repo-insights-description", "{fork.description}" }
                        }
                        span { class: "repo-insights-stats",
                            span { {count_label(fork.stars.max(0) as usize, "star", "stars")} }
                            span { "Updated {compact_date(&fork.updated_at)}" }
                        }
                    }
                }
            }
        }
        if page > 1 || has_more {
            nav { class: "repo-pagination", aria_label: "Forks",
                if page > 1 { NavLink { class: "btn btn-secondary".to_owned(), href: format!("{path}?page={}", page - 1), "Newer" } }
                if has_more { NavLink { class: "btn btn-secondary repo-pager-older".to_owned(), href: format!("{path}?page={}", page + 1), "Older" } }
            }
        }
    }
}
