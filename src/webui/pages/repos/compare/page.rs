//! The branch comparison page: the Files changed list of a pull request, for any two branches of
//! one repository.
use crate::webui::{
    encode_component,
    navigation::NavLink,
    pages::repos::{
        self,
        issues::{branch_select, Kind, Repository},
        pulls::{
            diff::{diff_view, view_toggle},
            Diff, PullSummary,
        },
    },
};
use dioxus::prelude::*;

pub fn compare(
    repo: &Repository,
    base: &str,
    head: &str,
    branches: &[String],
    diff: Option<&Diff>,
    open_pull: Option<&PullSummary>,
    split: bool,
) -> Element {
    let url = format!("{}/compare", repo.url());
    let pair = format!(
        "{url}?base={}&head={}",
        encode_component(base),
        encode_component(head)
    );
    let new_pull = format!(
        "{}/new?base={}&head={}&intent=compare",
        repo.family_url(Kind::Pull),
        encode_component(base),
        encode_component(head)
    );
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-compare-page",
                header { class: "collaboration-page-heading", h2 { "Comparing changes" } }
                form { method: "get", action: url, class: "collaboration-panel collaboration-comparison-controls",
                    div { class: "collaboration-compare-row",
                        div { class: "collaboration-compare-side", {branch_select("base", "Base branch", base, branches)} }
                        span { class: "collaboration-compare-arrow", aria_hidden: "true", "←" }
                        div { class: "collaboration-compare-side", {branch_select("head", "Compare branch", head, branches)} }
                        button { r#type: "submit", class: "btn btn-secondary", "Compare" }
                    }
                }
                if let Some(diff) = diff {
                    if !diff.files.is_empty() {
                        div { class: "collaboration-actions",
                            {view_toggle(&pair, split)}
                            if let Some(pull) = open_pull {
                                NavLink { class: "btn btn-secondary", href: format!("{}/{}", repo.family_url(Kind::Pull), pull.number), "View pull request #{pull.number}" }
                                span { class: "collaboration-compare-title", "{pull.title}" }
                            } else {
                                NavLink { class: "btn btn-primary", href: new_pull, "Create pull request" }
                            }
                        }
                    }
                    {diff_view(diff, split)}
                } else {
                    p { class: "collaboration-panel collaboration-muted repo-compare-empty", "There isn't anything to compare." }
                }
            }
        },
    )
}
