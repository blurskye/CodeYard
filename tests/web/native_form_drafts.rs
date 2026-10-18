//! Native VDOM policy tests, not browser/domain acceptance or mocked product responses.
//! The fetched WASM browser suite must separately prove real user draft preservation.
use codeyard::webui::pages::{
    ci,
    repos::{
        actions::{ActionsPage, Workflow},
        chrome::{repo_engagement, repo_tabs, RepoChrome},
        issues::{self, DetailDraft, Item, Kind, Repository},
    },
};
use dioxus::{
    core::{AttributeValue, Mutation},
    prelude::*,
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[test]
fn quoted_native_defaults_are_nonvolatile_and_keep_initial_ssr_values() {
    let initial = "initial <draft>".to_string();
    let selected = true;
    let nodes = [
        rsx! { input { "value": initial.clone(), "checked": selected } }.unwrap(),
        rsx! { option { "selected": selected, "Initial option" } }.unwrap(),
    ];
    for node in &nodes {
        for attributes in node.dynamic_attrs.iter() {
            for attribute in attributes.iter() {
                assert!(
                    !attribute.volatile,
                    "native default {} became controlled",
                    attribute.name
                );
            }
        }
    }
    let html = dioxus_ssr::render_element(rsx! {
        input { "value": initial, "checked": selected }
        select { option { "selected": selected, "Initial option" } }
    });
    assert!(html.contains("value=\"initial &#60;draft&#62;\""));
    assert!(html.contains("checked"));
    assert!(html.contains("selected"));
    let disabled = false;
    let unchecked = dioxus_ssr::render_element(rsx! {
        input { "checked": disabled }
        select { option { "selected": disabled, "Unselected option" } }
    });
    assert!(!unchecked.contains("checked="));
    assert!(!unchecked.contains("selected="));
}

#[derive(Clone)]
struct Harness {
    initial: ActionsPage,
    control: Rc<RefCell<Option<Signal<ActionsPage>>>>,
}
fn actions_root(props: Harness) -> Element {
    let model = use_signal(|| props.initial.clone());
    *props.control.borrow_mut() = Some(model);
    ci::render(ci::Page::Actions(Box::new(model())))
}

#[test]
fn unrelated_actions_model_refresh_emits_no_branch_draft_value_setters() {
    let branch = "native-draft-branch-fixture";
    let mut model = ActionsPage {
        repository: Some(("fixture-owner".into(), "fixture-repository".into())),
        repo_chrome: Some(RepoChrome {
            owner: "fixture-owner".into(),
            name: "fixture-repository".into(),
            current_path: "/fixture-owner/fixture-repository/actions".into(),
            signed_in: true,
            can_write: true,
            csrf_token: "fixture-csrf".into(),
            ..Default::default()
        }),
        title: "Fixture Actions".into(),
        base_path: "/fixture-owner/fixture-repository/actions".into(),
        csrf_token: "fixture-csrf".into(),
        query: BTreeMap::new(),
        runs: vec![],
        workflows: vec![Workflow {
            path: ".github/workflows/fixture.yml".into(),
            name: "Fixture workflow".into(),
            dispatchable: true,
            inputs: Vec::new(),
        }],
        selected: None,
        dispatch_branch: branch.into(),
        can_dispatch: true,
        run_limit: 50,
        log_limit: 2000,
    };
    let control = Rc::new(RefCell::new(None));
    let mut dom = VirtualDom::new_with_props(
        actions_root,
        Harness {
            initial: model.clone(),
            control: control.clone(),
        },
    );
    let initial = dom.rebuild_to_vec();
    let draft_elements: Vec<_> = initial
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::SetAttribute {
                name: "value",
                value: AttributeValue::Text(value),
                id,
                ..
            } if value == branch => Some(*id),
            _ => None,
        })
        .collect();
    // The page's only branch control is the dispatch menu's branch input.
    assert_eq!(
        draft_elements.len(),
        1,
        "fixture must exercise the dispatch branch control"
    );
    assert!(dioxus_ssr::render(&dom).contains(&format!("value=\"{branch}\"")));
    model.title = "Updated fixture metadata, same draft context".into();
    let mut signal = control.borrow().expect("mounted Actions fixture signal");
    dom.in_runtime(|| signal.set(model));
    let refreshed = dom.render_immediate_to_vec();
    assert!(
        !refreshed.edits.iter().any(|edit| matches!(edit,
            Mutation::SetAttribute { name: "value", id, .. } if draft_elements.contains(id)
        )),
        "a parent refresh must not overwrite native user-edited branch properties"
    );
}

#[derive(Clone)]
struct CollaborationHarness {
    initial: issues::Page,
    control: Rc<RefCell<Option<Signal<issues::Page>>>>,
}
fn collaboration_root(props: CollaborationHarness) -> Element {
    let model = use_signal(|| props.initial.clone());
    *props.control.borrow_mut() = Some(model);
    issues::render(model())
}
fn collaboration_edit_form(html: &str, number: i32) -> &str {
    html.split("<form")
        .find(|form| {
            form.contains(&format!(
                "action=\"/fixture-owner/fixture-repository/issues/{number}/edit\""
            ))
        })
        .unwrap()
        .split_once("</form>")
        .unwrap()
        .0
}

#[test]
fn collaboration_chrome_refresh_preserves_controlled_values_and_complete_edit_snapshot() {
    let repository = Repository {
        owner: "fixture-owner".into(),
        name: "fixture-repository".into(),
        signed_in: true,
        can_write: true,
        can_admin: false,
        csrf: "initial-csrf".into(),
        viewer: "fixture-owner".into(),
        chrome: RepoChrome {
            owner: "fixture-owner".into(),
            name: "fixture-repository".into(),
            signed_in: true,
            can_write: true,
            is_owner: true,
            csrf_token: "initial-csrf".into(),
            current_path: "/fixture-owner/fixture-repository/issues/7".into(),
            ..Default::default()
        },
    };
    let mut model = issues::Page::Detail {
        repository,
        kind: Kind::Issue,
        item: Item {
            number: 7,
            stacked: false,
            stack: None,
            stack_candidate: None,
            assignees: Vec::new(),
            milestone: None,
            title: "snapshot-title-fixture".into(),
            body: "snapshot-body-fixture".into(),
            state: "open".into(),
            author: "fixture-owner".into(),
            created: "2026-10-02T12:00:00Z".into(),
            updated: "snapshot-revision".into(),
            labels: vec![],
            comparison: None,
            comment_count: 0,
            commit_count: None,
            file_count: None,
            check_count: 0,
            review_state: None,
            check_state: None,
            reactions: Vec::new(),
        },
        comments: codeyard::webui::pages::repos::issues::CommentPage::default(),
        available_labels: vec![],
        draft: DetailDraft::default(),
        error: None,
        references: Default::default(),
        reviewers: Vec::new(),
        assignable: Vec::new(),
        milestones: Vec::new(),
    };
    let control = Rc::new(RefCell::new(None));
    let mut dom = VirtualDom::new_with_props(
        collaboration_root,
        CollaborationHarness {
            initial: model.clone(),
            control: control.clone(),
        },
    );
    let initial = dom.rebuild_to_vec();
    let draft_elements: Vec<_> = initial
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::SetAttribute {
                name: "value",
                value: AttributeValue::Text(value),
                id,
                ..
            } if value == "snapshot-title-fixture" || value == "snapshot-body-fixture" => {
                Some((*id, value.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        draft_elements.len(),
        2,
        "fixture must exercise title AND Markdown body controls"
    );
    if let issues::Page::Detail {
        repository,
        item,
        draft,
        ..
    } = &mut model
    {
        repository.csrf = "fresh-csrf".into();
        repository.chrome.csrf_token = "fresh-csrf".into();
        repository.chrome.stars = 9;
        repository.chrome.watch_level = "watching".to_owned();
        item.title = "refreshed-item-title".into();
        item.body = "refreshed-item-body".into();
        item.state = "closed".into();
        item.updated = "refreshed-item-revision".into();
        draft.title = Some("refreshed-server-title".into());
        draft.body = Some("refreshed-server-body".into());
        draft.preview = true;
    }
    let mut signal = control
        .borrow()
        .expect("mounted collaboration fixture signal");
    dom.in_runtime(|| signal.set(model.clone()));
    let refreshed = dom.render_immediate_to_vec();
    // These are signal-controlled DraftInput/DraftTextarea, unlike the quoted
    // native Actions defaults above. Reasserting their current value is valid;
    // adopting refreshed server values is not. Actual typed browser drafts need
    // the separate live browser check, not a zero-setter assertion here.
    for edit in &refreshed.edits {
        if let Mutation::SetAttribute {
            name: "value",
            value,
            id,
            ..
        } = edit
        {
            if let Some((_, expected)) = draft_elements.iter().find(|(input, _)| input == id) {
                assert!(
                    matches!(value, AttributeValue::Text(actual) if actual == expected),
                    "metadata refresh replaced a controlled draft value: {edit:?}"
                );
            }
        }
    }
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("refreshed-item-title"));
    // The tab row and the engagement buttons are drawn by the site header, which this body-only
    // render does not include.
    let tabs = dioxus_ssr::render_element(repo_tabs(model.repo_chrome()));
    assert!(tabs.contains("repo-tab repo-tab-active"));
    let engagement =
        dioxus_ssr::render_element(repo_engagement(model.repo_chrome(), "repo-engagement"));
    assert!(engagement.contains("/_forms/watch-level"));
    let form = collaboration_edit_form(&html, 7);
    for preserved in [
        "snapshot-title-fixture",
        "snapshot-body-fixture",
        "snapshot-revision",
        "value=\"open\"",
    ] {
        assert!(form.contains(preserved));
    }
    assert!(form.contains("fresh-csrf"));
    assert!(!form.contains("initial-csrf"));
    assert!(!form.contains("refreshed-item-revision"));
    assert!(!form.contains("refreshed-server-body"));
    assert!(
        !form.contains("collaboration-preview"),
        "preview belongs to the same immutable edit snapshot"
    );
    if let issues::Page::Detail {
        repository,
        item,
        draft,
        ..
    } = &mut model
    {
        item.number = 8;
        repository.chrome.current_path = "/fixture-owner/fixture-repository/issues/8".into();
        *draft = DetailDraft::default();
    }
    dom.in_runtime(|| signal.set(model));
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    let form = collaboration_edit_form(&html, 8);
    for reset in [
        "refreshed-item-title",
        "refreshed-item-body",
        "refreshed-item-revision",
        "value=\"closed\"",
    ] {
        assert!(form.contains(reset));
    }
    assert!(!form.contains("snapshot-revision"));
    assert!(form.contains("fresh-csrf"));
}
