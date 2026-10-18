//! The commit choice of a form that writes to a branch: commit to it directly, or put the commit
//! on a new branch whose name the user can change and start a pull request. A protected branch
//! cannot take the commit directly, so the new branch is the only choice.
use crate::webui::components::DraftInput;
use dioxus::prelude::*;

pub fn commit_target(branch: &str, new_branch: &str, protected: bool) -> Element {
    rsx! {
        fieldset { class: "repo-commit-target",
            legend { "Where to commit" }
            label { class: "repo-commit-choice",
                input { r#type: "radio", name: "commit_target", value: "direct", disabled: protected, checked: !protected }
                span {
                    strong { "Commit directly to the {branch} branch" }
                    if protected { small { "This branch is protected." } }
                }
            }
            label { class: "repo-commit-choice",
                input { r#type: "radio", name: "commit_target", value: "new_branch", checked: protected }
                span { strong { "Create a new branch for this commit and start a pull request" } }
            }
            label { class: "repo-field repo-commit-branch",
                span { "New branch name" }
                DraftInput { name: "new_branch", required: protected, maxlength: Some(255), initial_value: new_branch.to_owned() }
            }
        }
    }
}
