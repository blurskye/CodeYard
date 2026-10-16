//! The branch comparison page: the Files changed list of a pull request, for any two branches of
//! one repository.
use super::{diff::diff_view, new::branch_select, Diff, Kind, Repository};
use crate::webui::{encode_component, navigation::NavLink, pages::repositories};
use dioxus::prelude::*;

pub fn compare(
    repo: &Repository,
    base: &str,
    head: &str,
    branches: &[String],
    diff: Option<&Diff>,
) -> Element {
    let url = format!("{}/compare", repo.url());
    let new_pull = format!(
        "{}/new?base={}&head={}&intent=compare",
        repo.family_url(Kind::Pull),
        encode_component(base),
        encode_component(head)
    );
    repositories::chrome(
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
                            NavLink { class: "btn btn-primary", href: new_pull, "Create pull request" }
                        }
                    }
                    {diff_view(diff)}
                } else {
                    p { class: "collaboration-panel collaboration-muted", "There isn't anything to compare." }
                }
            }
        },
    )
}
