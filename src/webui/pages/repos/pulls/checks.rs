//! The Checks tab of a pull request: the workflow runs on its head commit.
use crate::webui::pages::repos::{
    self as repos,
    actions::{self, Run},
    issues::{
        detail::{pull_summary, pull_tabs, PullTab},
        Item, Kind, Repository,
    },
};
use crate::webui::{navigation::NavLink, pages::repos::ref_path::branch_url};
use dioxus::prelude::*;

/// A commit status an outside service reported on the head commit.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CheckStatus {
    pub context: String,
    pub state: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

/// The Checks tab: the workflow runs on the head commit of a pull request. Each run is the Actions
/// page's run row, linked to the run's logs there.
pub fn checks_page(
    repo: &Repository,
    item: &Item,
    runs: &[Run],
    statuses: &[CheckStatus],
    code_scanning: &[repos::security::Check],
    (check_page, more_checks): (u32, bool),
) -> Element {
    let url = format!("{}/{}", repo.family_url(Kind::Pull), item.number);
    let actions_url = format!("{}/actions", repo.url());
    // A workflow file goes on the head branch, because the checks run on that branch's commit. Only
    // a writer gets the link, and only when the head branch is in this repository.
    let add_workflow = item.comparison.as_ref().and_then(|compare| {
        let own_branch = branch_url(&repo.url(), "tree", &compare.head, "");
        (repo.can_write && compare.head_url.as_ref() == Some(&own_branch))
            .then(|| branch_url(&repo.url(), "new", &compare.head, ".github/workflows"))
    });
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-pull-checks",
                header { class: "collaboration-detail-heading",
                    h2 { "{item.title} ", span { class: "collaboration-number", "#{item.number}" } }
                    {pull_summary(repo, item, Kind::Pull)}
                    {pull_tabs(&url, item, PullTab::Checks)}
                }
                if runs.is_empty() && statuses.is_empty() && code_scanning.is_empty() {
                    p { class: "collaboration-panel collaboration-muted collaboration-checks-empty",
                        "No workflow runs have started on this commit."
                        if let Some(href) = &add_workflow {
                            " "
                            NavLink { href: href.clone(), "Add a workflow file" }
                        }
                    }
                }
                {repos::security::check_list(code_scanning)}
                if !runs.is_empty() {
                    // The Actions page scopes its run row styles to this class.
                    div { class: "actions-page",
                        section { class: "actions-runs", aria_label: "Workflow runs on this commit",
                            ul {
                                for run in runs {
                                    li { key: "{run.id}",
                                        {actions::run_row(run, actions::workflow_name(&run.workflow_path), format!("{actions_url}?job={}&view=logs", run.id), false)}
                                    }
                                }
                            }
                        }
                    }
                }
                if !statuses.is_empty() {
                    section { class: "collaboration-panel", aria_label: "External checks on this commit",
                        h3 { "External checks" }
                        ul {
                            for status in statuses {
                                li {
                                    strong { "{status.context}" }
                                    " · {status.state}"
                                    if let Some(description) = &status.description { p { "{description}" } }
                                    if let Some(href) = status.target_url.as_ref().filter(|href| url::Url::parse(href).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())) {
                                        a { href: href.clone(), rel: "noopener noreferrer", "Details" }
                                    }
                                }
                            }
                        }
                    }
                }
                if check_page > 1 || more_checks {
                    nav { class: "repo-pagination", aria_label: "Check pages",
                        if check_page > 1 { NavLink { class: "btn btn-secondary", href: format!("{url}/checks?checks_page={}", check_page - 1), "Previous" } }
                        if more_checks { NavLink { class: "btn btn-secondary", href: format!("{url}/checks?checks_page={}", check_page + 1), "Next" } }
                    }
                }
            }
        },
    )
}
