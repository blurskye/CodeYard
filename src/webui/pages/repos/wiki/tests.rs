use super::*;
use crate::webui::pages::repos::chrome::RepoChrome;

fn repo(can_write: bool, can_admin: bool) -> RepoChrome {
    RepoChrome {
        id: 1,
        owner: "alice".into(),
        name: "project".into(),
        csrf_token: "csrf-fixture".into(),
        wiki_enabled: true,
        can_write,
        can_admin,
        ..Default::default()
    }
}

fn item(body: &str) -> PageItem {
    PageItem {
        slug: "home".into(),
        title: "Home".into(),
        body: body.into(),
        revision: 3,
        author: "alice".into(),
        saved_at: "2026-10-01T00:00:00Z".into(),
        message: String::new(),
    }
}

fn show(repo: &RepoChrome, can_write: bool, can_admin: bool) -> String {
    crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(render(
            repo,
            Page::Show {
                page: item("See [[Install Guide]]"),
                sidebar: Some(PageItem {
                    body: "Sidebar notes".into(),
                    ..item("")
                }),
                footer: None,
                known: vec!["home".into()],
                can_write,
                can_admin,
            },
        ))
    })
}

#[test]
fn a_reader_sees_the_page_without_editing_or_deleting_controls() {
    let html = show(&repo(false, false), false, false);
    assert!(html.contains("Sidebar notes"), "{html}");
    assert!(html.contains("/alice/project/wiki/_pages"), "{html}");
    assert!(!html.contains("/edit\""), "{html}");
    assert!(!html.contains("/delete\""), "{html}");
}

#[test]
fn a_writer_edits_and_an_admin_gets_the_delete_confirmation() {
    let html = show(&repo(true, true), true, true);
    assert!(html.contains("/alice/project/wiki/home/edit"), "{html}");
    assert!(html.contains("/alice/project/wiki/home/delete"), "{html}");
    assert!(html.contains("delete-wiki-page"), "{html}");
}

#[test]
fn a_refused_edit_keeps_the_text_and_the_revision_it_was_opened_on() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(render(
            &repo(true, false),
            Page::Form {
                editing: Some(("home".into(), 4)),
                draft: DraftFields {
                    title: "Home".into(),
                    body: "My words".into(),
                    message: "why".into(),
                    preview: false,
                },
                error: Some("The page changed while you were editing.".into()),
            },
        ))
    });
    assert!(
        html.contains("The page changed while you were editing."),
        "{html}"
    );
    assert!(html.contains("My words"), "{html}");
    assert!(html.contains("name=\"revision\""), "{html}");
    assert!(html.contains("value=\"4\""), "{html}");
}

#[test]
fn a_preview_renders_the_body_in_the_editor() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(render(
            &repo(true, false),
            Page::Form {
                editing: None,
                draft: DraftFields {
                    title: "Home".into(),
                    body: "**Bold** words".into(),
                    message: String::new(),
                    preview: true,
                },
                error: None,
            },
        ))
    });
    // The raw text sits in the textarea; only the preview renders the bold words, with classes.
    assert!(html.contains(">Bold</strong>"), "{html}");
}

#[test]
fn a_missing_page_offers_to_create_it_and_an_empty_wiki_says_so() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(render(
            &repo(true, false),
            Page::Missing {
                title: "Install Guide".into(),
                can_write: true,
                empty_wiki: true,
            },
        ))
    });
    assert!(html.contains("This wiki has no pages yet"), "{html}");
    assert!(html.contains("cannot be cloned or pushed"), "{html}");
    assert!(
        html.contains("/alice/project/wiki/_new?title=Install%20Guide"),
        "{html}"
    );
}

#[test]
fn the_compare_view_marks_added_and_removed_lines() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(render(
            &repo(false, false),
            Page::Compare {
                slug: "home".into(),
                old: RevisionItem {
                    number: 1,
                    title: "Home".into(),
                    body: "old".into(),
                    message: String::new(),
                    author: "alice".into(),
                    saved_at: "2026-10-01T00:00:00Z".into(),
                },
                new: RevisionItem {
                    number: 2,
                    title: "Home".into(),
                    body: "new".into(),
                    message: String::new(),
                    author: "alice".into(),
                    saved_at: "2026-10-02T00:00:00Z".into(),
                },
                diff: line_diff("old", "new"),
            },
        ))
    });
    assert!(
        html.contains("wiki-diff-del") && html.contains("wiki-diff-add"),
        "{html}"
    );
    assert!(!html.contains("<script"), "{html}");
}
