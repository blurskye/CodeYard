use super::*;

#[test]
fn oversized_patches_keep_threads_and_refused_line_drafts_in_both_views() {
    let file = DiffFile {
        filename: "large.rs".into(),
        status: "modified".into(),
        additions: 5000,
        deletions: 0,
        patch: "+line\n".repeat(5000),
        owners: vec![],
    };
    let thread = Thread {
        root: Comment {
            id: 42,
            revision: String::new(),
            author: "bob".into(),
            body: "Existing discussion".into(),
            created: "today".into(),
            location: None,
            conversation: None,
            reactions: vec![],
            can_edit: false,
            can_delete: false,
            edited: None,
            edit: None,
        },
        path: "large.rs".into(),
        line: Some(1),
        side: "RIGHT".into(),
        replies: vec![],
        pending: false,
    };
    let composer = LineComment {
        path: "large.rs".into(),
        line: "1".into(),
        side: "RIGHT".into(),
        body: "Refused draft on large patch".into(),
        commit: "old-head".into(),
        ..Default::default()
    };
    for split in [false, true] {
        let html = crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(diff_page(
                &repository(),
                &item(),
                &Diff {
                    files: vec![file.clone()],
                    ..Default::default()
                },
                std::slice::from_ref(&thread),
                false,
                Some(&composer),
                None,
                0,
                Some("Head changed"),
                &References::default(),
                split,
            ))
        });
        assert!(html.contains("This patch is too large to display"));
        assert!(html.contains("Existing discussion"));
        assert!(html.contains("Refused draft on large patch"));
        assert!(html.contains("name=\"commit_sha\" value=\"old-head\""));
        assert!(!html.contains("class=\"patch-lines\""));
    }
}

#[test]
fn review_summary_keeps_its_commit_snapshot_when_the_head_refreshes() {
    use std::{cell::RefCell, rc::Rc};
    #[derive(Clone)]
    struct Harness(Rc<RefCell<Option<Signal<String>>>>);
    fn root(props: Harness) -> Element {
        let head = use_signal(|| "head-A".to_owned());
        *props.0.borrow_mut() = Some(head);
        crate::webui::pages::repos::pulls::review::review_changes(
            &repository(),
            "/alice/repo/pulls/7",
            &head(),
            true,
            false,
            true,
            None,
            0,
            &References::default(),
        )
    }
    let control = Rc::new(RefCell::new(None));
    let mut dom = VirtualDom::new_with_props(root, Harness(control.clone()));
    dom.rebuild_in_place();
    let mut head = control.borrow().unwrap();
    dom.in_runtime(|| head.set("head-B".into()));
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(
        html.contains("name=\"commit_sha\" value=\"head-A\""),
        "{html}"
    );
    assert!(!html.contains("value=\"head-B\""));
}

#[test]
fn collapsed_files_panel_is_hidden_from_focus_and_accessibility() {
    let css = include_str!("../../style.css");
    let collapsed = css
        .split_once("#repo-files-toggle:checked ~ .repo-files-panel {")
        .unwrap()
        .1
        .split('}')
        .next()
        .unwrap();
    assert!(collapsed.contains("visibility: hidden"));
}
