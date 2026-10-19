//! The commit choice of a form that writes to a branch: commit to it directly, or put the commit
//! on a new branch whose name the user can change and start a pull request. A protected branch
//! cannot take the commit directly, so the new branch is the only choice.
use crate::webui::components::DraftInput;
use dioxus::prelude::*;

pub fn commit_target(
    branch: &str,
    new_branch: &str,
    protected: bool,
    selected_new_branch: bool,
) -> Element {
    let selected_new_branch = protected || selected_new_branch;
    rsx! {
        fieldset { class: "repo-commit-target",
            legend { "Where to commit" }
            label { class: "repo-commit-choice",
                input { r#type: "radio", name: "commit_target", value: "direct", disabled: protected, checked: !selected_new_branch }
                span {
                    strong { "Commit directly to the {branch} branch" }
                    if protected { small { "This branch is protected." } }
                }
            }
            label { class: "repo-commit-choice",
                input { r#type: "radio", name: "commit_target", value: "new_branch", checked: selected_new_branch }
                span { strong { "Create a new branch for this commit and start a pull request" } }
            }
            label { class: "repo-field repo-commit-branch",
                span { "New branch name" }
                DraftInput { name: "new_branch", required: protected, maxlength: Some(255), initial_value: new_branch.to_owned() }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_branch_choice_survives_preview_on_an_unprotected_branch() {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(commit_target("main", "topic", false, true))
        });
        assert!(html.contains("value=\"new_branch\" checked"), "{html}");
        assert!(!html.contains("value=\"direct\" checked"), "{html}");
    }

    #[test]
    fn refused_new_branch_choice_never_returns_to_direct() {
        crate::webui::tests::in_scope(|| {
            for (protected, selected, expected) in [
                (false, false, "direct"),
                (false, true, "new_branch"),
                (true, false, "new_branch"),
            ] {
                let html = dioxus_ssr::render_element(commit_target(
                    "main", "patch-2", protected, selected,
                ));
                let checked = html
                    .split("<input")
                    .find(|input| {
                        input.split('>').next().unwrap().contains("checked")
                            && input.contains("name=\"commit_target\"")
                    })
                    .unwrap();
                assert!(
                    checked
                        .split('>')
                        .next()
                        .unwrap()
                        .contains(&format!("value=\"{expected}\"")),
                    "{html}"
                );
            }
        });
    }
}
