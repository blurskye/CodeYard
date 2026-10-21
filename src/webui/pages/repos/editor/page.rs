use crate::webui::pages::repos::{
    chrome::{chrome, csrf, icon, repository_id, RepoChrome},
    commits::commit_target,
    ref_path::{file_url, is_markdown_path, link_base},
};
use crate::webui::{
    components::{
        code::{language_of_path, themed_highlight},
        markdown_tab_class,
        references::References,
        DraftInput, DraftTextarea, RepoMarkdown,
    },
    navigation::NavLink,
};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Editor {
    pub branch: String,
    pub path: String,
    pub content: String,
    pub message: String,
    pub expected_revision: String,
    pub creating: bool,
    pub error: Option<String>,
    pub saved: bool,
    pub preview: bool,
    /// What the content links in its preview. The server resolves it for the text it shows.
    #[serde(default)]
    pub references: References,
    /// The branch refuses direct commits to this user, so the commit goes to `new_branch`.
    #[serde(default)]
    pub protected: bool,
    pub commit_to_new_branch: bool,
    /// The name for the new branch, prefilled by the server and kept as the user typed it.
    #[serde(default)]
    pub new_branch: String,
    /// The text after the commit message's first line; empty for none.
    #[serde(default)]
    pub description: String,
}

/// The commit landed, but a step after it failed. This is not a failed save.
pub const SAVED_FOLLOWUP_FAILED: &str = "File saved; follow-up failed";

pub fn editor(repo: &RepoChrome, editor: &Editor) -> Element {
    let key = format!(
        "{}:{}:{}:{}:{}",
        repo.viewer_id, repo.id, editor.branch, editor.path, editor.creating
    );
    // Dioxus reconciles keys in fragments, not a lone dynamic component slot.
    rsx! { for key in [key] { EditorView { key: "{key}", repo: repo.clone(), editor: editor.clone() } } }
}

/// The references a preview links: the page's own, only while the file being typed is Markdown. A
/// file that is not Markdown is not previewed as a document, so it links nothing.
fn preview_base(base: &str, branch: &str, path: &str) -> crate::webui::components::LinkBase {
    link_base(
        base,
        branch,
        path.rsplit_once('/').map_or("", |(directory, _)| directory),
    )
}

fn preview_references(live_path: &str, references: &References) -> References {
    if is_markdown_path(live_path) {
        references.clone()
    } else {
        References::default()
    }
}

/// Whether the Markdown preview control is offered. Only a new file types its name here. Before
/// hydration the typed name is unknown to the browser, so the control stays. After it, a new file
/// keeps the control while its name is empty or Markdown.
fn preview_offered(creating: bool, hydrated: bool, typed_path: &str, path: &str) -> bool {
    if !creating {
        return is_markdown_path(path);
    }
    !hydrated || typed_path.is_empty() || is_markdown_path(typed_path)
}

const HIGHLIGHT_ID: &str = "repository-file-content-highlight";

/// Longer text stays plain. Highlighting is slow in the browser: about 17 ms per KiB.
const HIGHLIGHT_LIMIT: usize = 20 * 1024;

/// Text up to this size is highlighted as it is typed. Longer text waits for a pause in typing, and
/// the field shows its own plain text until then.
const LIVE_HIGHLIGHT: usize = 4 * 1024;

fn can_highlight(text: &str) -> bool {
    text.len() <= HIGHLIGHT_LIMIT
}

/// Waits for a pause in typing: 300 ms.
async fn typing_pause() {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(300).await;
}

/// The text, highlighted, behind the transparent field. The field keeps the caret, selection and
/// typing. Only the hydrated page draws it, so the server-rendered field does not change. While the
/// highlight is behind the text (a pause is pending), the layer is hidden and the field shows its
/// own text.
#[component]
fn EditorHighlight(text: Signal<String>, path: Signal<String>, enabled: bool) -> Element {
    let mut done = use_signal(String::new);
    let mut html = use_signal(String::new);
    let mut ticket = use_signal(|| 0u32);
    use_effect(move || {
        let next = text();
        let language = language_of_path(&path()).to_owned();
        if !can_highlight(&next) {
            return;
        }
        let mine = ticket.peek().wrapping_add(1);
        ticket.set(mine);
        spawn(async move {
            if next.len() > LIVE_HIGHLIGHT {
                typing_pause().await;
            }
            if *ticket.peek() == mine {
                // The final newline gives the empty row after a last line break, as the field shows it.
                html.set(format!("{}\n", themed_highlight(&next, &language)));
                done.set(next);
            }
        });
    });
    let current = text.read();
    let stale = *current != *done.read();
    rsx! {
        if enabled && can_highlight(&current) {
            pre { id: HIGHLIGHT_ID, class: if stale { "repo-editor-highlight is-stale" } else { "repo-editor-highlight" },
                aria_hidden: "true", dangerous_inner_html: html() }
        }
    }
}

/// Moves the highlight to the field's scroll position. Long lines scroll sideways inside the field.
fn follow_scroll(top: f64, left: f64) {
    #[cfg(target_arch = "wasm32")]
    {
        let layer = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id(HIGHLIGHT_ID));
        if let Some(layer) = layer {
            layer.set_scroll_top(top as i32);
            layer.set_scroll_left(left as i32);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (top, left);
}

fn line_numbers(lines: usize) -> String {
    (1..=lines)
        .map(|number| number.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Local preview state belongs to this component, never the route dispatcher's hook sequence.
/// The native preview POST and textarea remain functional when hydration is unavailable.
#[component]
fn EditorView(repo: RepoChrome, editor: Editor) -> Element {
    // The loaded text and its revision stay together. A refresh of the same page must not move the
    // hidden revision under text that was typed against the old one. A new repository, branch or
    // path gets a different component key and a new snapshot.
    let editor = use_hook(|| editor);
    // DraftTextarea owns the live field; this observer only drives Markdown preview. After a
    // refused save or a preview the server text is the submitted text.
    let mut content = use_signal(|| editor.content.clone());
    // The gutter and the field grow with the text, so one scroll box moves both together. The
    // memo changes only when a line is added or removed, not on every key.
    let lines = use_memo(move || content.read().matches('\n').count() + 1);
    let gutter = use_memo(move || line_numbers(lines()));
    // The path field's live value decides whether the preview links the page's references. A new
    // file's first load resolves only this repository's numbers, so links to other repositories and
    // to users appear once the native Preview posts the text.
    let mut live_path = use_signal(|| editor.path.clone());
    let mut hydrated = use_signal(|| false);
    use_effect(move || hydrated.set(true));
    let offer_preview = preview_offered(editor.creating, hydrated(), &live_path(), &editor.path);
    // The preview renders as the saved file will: the same document mode and relative links.
    let preview_base = preview_base(&repo.base(), &editor.branch, &live_path());
    let mut preview = use_signal(|| editor.preview);
    let previewing = preview() && offer_preview;
    let cancel_url = if editor.creating {
        file_url(&repo.base(), "tree", &editor.branch, "")
    } else {
        file_url(&repo.base(), "blob", &editor.branch, &editor.path)
    };
    // A rejected save is shown at this POST-only URL; `from` lets a GET there find the editor.
    let write_action = format!(
        "{}?from={}",
        repo.action("write-file"),
        crate::webui::encode_component(&file_url(
            &repo.base(),
            if editor.creating { "new" } else { "edit" },
            &editor.branch,
            &editor.path,
        ))
    );
    // Without JavaScript, the Preview submit lands on the preview card above the commit box.
    let preview_action = format!("{write_action}#preview");
    let commit_placeholder = if editor.creating {
        "Create file".to_owned()
    } else {
        format!("Update {}", editor.path)
    };
    chrome(
        &repo,
        rsx! {
            section { class: "repo-editor-page",
                header { class: "repo-page-heading repo-editor-heading",
                    div {
                        h2 { if editor.creating { "Create a new file" } else { "Edit file" } }
                        p { {icon("branches")} code { "{editor.branch}" } if !editor.path.is_empty() { " / " strong { "{editor.path}" } } }
                    }
                    div { class: "repo-editor-actions",
                        NavLink { class: "btn btn-secondary".to_owned(), href: cancel_url.clone(), "Cancel" }
                    }
                }
                if let Some(error) = &editor.error { div { class: "repo-alert repo-alert-error", role: "alert", strong { if editor.saved { "{SAVED_FOLLOWUP_FAILED}" } else { "Changes were not saved" } } p { "{error}" } } }
                form { id: "repo-editor-form", method: "post", action: write_action, class: "repo-editor-form",
                    {csrf(&repo.csrf_token)}
                    {repository_id(repo.id)}
                    input { r#type: "hidden", name: "expected_revision", value: editor.expected_revision.clone() }
                    input { r#type: "hidden", name: "creating", value: if editor.creating { "yes" } else { "no" } }
                    // The heading names the branch, and the commit target below picks where the commit goes.
                    // A branch field here would be a second destination.
                    input { r#type: "hidden", name: "branch", value: editor.branch.clone() }
                    div { class: "repo-editor-context panel",
                        label { class: "repo-field repo-editor-path", span { "File path" } DraftInput { name: "path", required: true, maxlength: Some(4096), initial_value: editor.path.clone(), readonly: !editor.creating, placeholder: "Name your file… use / to add folders", on_value: move |next: String| live_path.set(next) } }
                    }
                    if offer_preview {
                        div { class: "collaboration-editor-tabs",
                            // Write has nothing to do before hydration, so it is disabled there.
                            button { r#type: "button", disabled: !hydrated(), class: markdown_tab_class(!previewing),
                                aria_pressed: if previewing { "false" } else { "true" },
                                onclick: move |_| preview.set(false),
                                "Write"
                            }
                            button { r#type: if hydrated() { "button" } else { "submit" }, name: "intent", value: "preview", formnovalidate: true,
                                formaction: preview_action, class: markdown_tab_class(previewing),
                                aria_pressed: if previewing { "true" } else { "false" },
                                onclick: move |_| preview.set(true),
                                "Preview"
                            }
                        }
                    }
                    section { class: "repo-code-editor panel",
                        header { class: "repo-code-editor-header",
                            strong { if editor.creating { "New file" } else { "Edit" } }
                            span { if lines() == 1 { "1 line" } else { "{lines()} lines" } }
                        }
                        if !can_highlight(&content()) {
                            p { class: "repo-editor-note", "Syntax highlighting is off for large files." }
                        }
                        label { class: "sr-only", r#for: "repository-file-content", "File contents" }
                        div { class: "repo-code-editor-body",
                            pre { class: "repo-code-editor-gutter", aria_hidden: "true", {gutter()} }
                            div { class: "repo-editor-stack",
                                EditorHighlight { text: content, path: live_path, enabled: hydrated() }
                                DraftTextarea { id: "repository-file-content", class: "repo-editor-textarea", name: "content", rows: lines().max(10) as u32, initial_value: editor.content.clone(), on_value: move |next: String| content.set(next), on_scroll: move |event: ScrollEvent| follow_scroll(event.data().scroll_top(), event.data().scroll_left()) }
                            }
                        }
                    }
                    if previewing {
                        section { id: "preview", class: "repo-editor-preview panel", aria_label: "Markdown preview",
                            header { class: "repo-section-heading", h3 { "Preview" } }
                            div { class: "repo-readme-body", RepoMarkdown { content: content(), base: preview_base.clone(), references: preview_references(&live_path(), &editor.references) } }
                        }
                    }
                    section { class: "repo-commit-box panel",
                        header { class: "repo-section-heading", div { h3 { "Commit changes" } p { "Describe what changed so it is easy to understand in history." } } }
                        div { class: "repo-form-body",
                            {commit_target(&editor.branch, &editor.new_branch, editor.protected, editor.commit_to_new_branch)}
                            label { class: "repo-field", span { "Commit message" } DraftInput { name: "message", initial_value: editor.message.clone(), maxlength: Some(4096), placeholder: commit_placeholder } }
                            label { class: "repo-field", span { "Extended description" } DraftTextarea { name: "description", initial_value: editor.description.clone(), rows: 3, maxlength: Some(4096) } }
                            p { class: "repo-field-help", "If the branch changed while you were editing, your text will be kept so you can review and try again." }
                            div { class: "repo-form-actions",
                                button { class: "btn btn-primary", r#type: "submit", name: "intent", value: "save",
                                    "Commit changes"
                                }
                                NavLink { class: "btn btn-secondary".to_owned(), href: cancel_url, "Cancel" }
                            }
                        }
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::References;

    #[test]
    fn new_file_preview_links_follow_the_typed_directory() {
        let nested = super::preview_base("/u/r", "main", "docs/new.md");
        assert_eq!(nested.directory, "docs");
        assert_eq!(nested.blob, "/u/r/blob/refs/heads/main");
        assert_eq!(super::preview_base("/u/r", "main", "new.md").directory, "");
    }

    #[test]
    fn gutter_numbers_cover_every_source_line() {
        assert_eq!(super::line_numbers(1), "1");
        let gutter = super::line_numbers(50_000);
        assert_eq!(gutter.lines().count(), 50_000);
        assert!(gutter.ends_with("49999\n50000"));
    }

    #[test]
    fn a_preview_links_the_page_only_for_a_markdown_path() {
        let references = References {
            owner: "alice".into(),
            repo: "demo".into(),
            last_number: 1,
            ..References::default()
        };
        assert_eq!(
            super::preview_references("notes.md", &references).repo,
            "demo"
        );
        assert_eq!(
            super::preview_references("docs/Guide.MD", &references).repo,
            "demo"
        );
        assert!(super::preview_references("main.rs", &references)
            .repo
            .is_empty());
        assert!(super::preview_references("README", &references)
            .repo
            .is_empty());
        assert!(super::preview_references("", &references).repo.is_empty());
    }

    #[test]
    fn the_preview_control_follows_the_typed_name_after_hydration() {
        // Before hydration the server cannot see the typed name, so a new file keeps the control.
        assert!(super::preview_offered(true, false, "src/x.rs", ""));
        assert!(super::preview_offered(true, true, "", ""));
        assert!(super::preview_offered(true, true, "notes.MD", ""));
        assert!(!super::preview_offered(true, true, "src/x.rs", ""));
        assert!(super::preview_offered(false, true, "", "README.md"));
        assert!(!super::preview_offered(false, false, "", "src/main.rs"));
    }

    #[test]
    fn highlighting_stops_above_the_size_limit() {
        assert!(super::can_highlight(&"a".repeat(super::HIGHLIGHT_LIMIT)));
        assert!(!super::can_highlight(
            &"a".repeat(super::HIGHLIGHT_LIMIT + 1)
        ));
    }
}
