//! Native direct merges. Each option carries the commit identity displayed to the user.
use super::chrome::{chrome, csrf, RepoChrome};
use crate::components::{DraftSelect, DraftTextarea, SelectOption};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MergeForm {
    pub repository_id: String,
    pub base: String,
    pub head: String,
    pub choices: Vec<SelectOption>,
    pub message: String,
    pub error: Option<String>,
    pub can_merge: bool,
}

pub fn merge(repo: &RepoChrome, form: &MergeForm) -> Element {
    chrome(
        repo,
        rsx! { MergeComposer { repo: repo.clone(), form: form.clone() } },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::repos::page::{render, Page};
    use std::{cell::RefCell, rc::Rc};

    #[derive(Clone)]
    struct Harness {
        initial: Page,
        state: Rc<RefCell<Option<Signal<Page>>>>,
    }
    fn root(props: Harness) -> Element {
        let page = use_signal(|| props.initial.clone());
        *props.state.borrow_mut() = Some(page);
        render(page())
    }

    #[test]
    fn merge_refresh_keeps_the_message_repository_and_selected_revisions_together() {
        let repo = RepoChrome {
            id: 7,
            owner: "alice".into(),
            name: "repo".into(),
            can_write: true,
            ..Default::default()
        };
        let base = format!("{} main", "a".repeat(40));
        let head = format!("{} topic", "b".repeat(40));
        let form = MergeForm {
            repository_id: "7".into(),
            base: base.clone(),
            head: head.clone(),
            choices: vec![
                SelectOption::new(&base, "main (aaaaaaa)"),
                SelectOption::new(&head, "topic (bbbbbbb)"),
            ],
            message: "Original merge message".into(),
            error: None,
            can_merge: true,
        };
        let state = Rc::new(RefCell::new(None));
        let mut dom = VirtualDom::new_with_props(
            root,
            Harness {
                initial: Page::BranchMerge {
                    repo: repo.clone(),
                    form: Box::new(form.clone()),
                },
                state: state.clone(),
            },
        );
        dom.rebuild_in_place();
        let mut signal = state.borrow().unwrap();
        dom.in_runtime(|| {
            signal.set(Page::BranchMerge {
                repo: RepoChrome { id: 8, ..repo },
                form: Box::new(MergeForm {
                    repository_id: "8".into(),
                    base: format!("{} main", "c".repeat(40)),
                    head: format!("{} topic", "d".repeat(40)),
                    message: "A different message".into(),
                    error: Some("Repository changed".into()),
                    can_merge: false,
                    ..form
                }),
            })
        });
        dom.render_immediate_to_vec();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Original merge message"));
        assert!(!html.contains("A different message"));
        assert!(html.contains("value=\"7\""));
        assert!(!html.contains("value=\"8\""));
        assert!(html.contains(&base));
        assert!(html.contains(&head));
        assert!(!html.contains(&"c".repeat(40)));
        assert!(!html.contains(&"d".repeat(40)));
        assert!(html.contains("Repository changed"));
        assert!(html.contains("readonly"));
    }
}

#[component]
fn MergeComposer(repo: RepoChrome, form: MergeForm) -> Element {
    // A refreshed page may not silently give an unsent message new target commits or a new repo.
    let snapshot = use_signal(|| form.clone());
    let draft = snapshot.read().clone();
    rsx! {
        section { class: "collaboration-page",
            header { class: "collaboration-page-heading", h2 { "Merge branches" } }
            p { class: "collaboration-muted", "Merge the head into the base branch. The displayed revisions must still match when the merge is published. Branch protections apply." }
            if let Some(error) = form.error { p { class: "collaboration-error", role: "alert", "{error}" } }
            form { method: "post", action: format!("{}/merge", repo.base()), class: "collaboration-panel collaboration-form",
                {csrf(&repo.csrf_token)}
                input { r#type: "hidden", name: "repository_id", value: draft.repository_id }
                label { r#for: "merge-base", "Base branch" }
                DraftSelect { id: "merge-base", name: "base", initial_value: draft.base, options: draft.choices.clone(), required: true, disabled: !form.can_merge }
                label { r#for: "merge-head", "Head branch" }
                DraftSelect { id: "merge-head", name: "head", initial_value: draft.head, options: draft.choices, required: true, disabled: !form.can_merge }
                label { r#for: "merge-message", "Merge message (optional)" }
                DraftTextarea { id: "merge-message", name: "message", initial_value: draft.message, readonly: !form.can_merge, maxlength: Some(codeyard_types::git_limits::MAX_MERGE_MESSAGE_BYTES as u32), rows: 5 }
                if form.can_merge {
                    button { r#type: "submit", class: "btn btn-primary", "Merge branches" }
                } else { p { class: "collaboration-muted", "Repository write access and an unarchived repository are required. Your submitted message is preserved." } }
            }
        }
    }
}
