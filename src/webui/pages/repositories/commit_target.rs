//! The commit choice of a form that writes to a protected branch. The branch cannot take the
//! commit directly, so the form offers a new branch, selected, with a name the user can change.
use crate::webui::components::DraftInput;
use dioxus::prelude::*;

pub fn commit_target(branch: &str, new_branch: &str) -> Element {
    rsx! {
        fieldset { class: "repo-commit-target",
            legend { "Where to commit" }
            label { class: "repo-commit-choice",
                input { r#type: "radio", name: "commit_target", value: "direct", disabled: true }
                span { strong { "Commit directly to the {branch} branch" } small { "This branch is protected." } }
            }
            label { class: "repo-commit-choice",
                input { r#type: "radio", name: "commit_target", value: "new_branch", checked: true }
                span { strong { "Create a new branch for this commit and start a pull request" } }
            }
            label { class: "repo-field repo-commit-branch",
                span { "New branch name" }
                DraftInput { name: "new_branch", required: true, maxlength: Some(255), initial_value: new_branch.to_owned() }
            }
        }
    }
}
