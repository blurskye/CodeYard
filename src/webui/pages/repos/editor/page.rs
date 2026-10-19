use crate::webui::pages::repos::{
    chrome::{chrome, csrf, icon, RepoChrome},
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

/// Unsaved file text, kept in this tab's session storage so that leaving the page and
/// coming back (Back, a link) does not lose it. The key holds the revision the text was
/// started from, so a file that changed meanwhile starts a new draft.
mod saved_draft {
    #[cfg(target_arch = "wasm32")]
    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.session_storage().ok().flatten()
    }

    pub fn load(key: &str) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        return storage()?.get_item(key).ok().flatten();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = key;
            None
        }
    }

    // A full or disabled session storage only loses the restored draft. The text stays in the
    // form, and the save still goes to the server.
    pub fn save(key: &str, text: &str) {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let _ = storage.set_item(key, text);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (key, text);
    }

    pub fn clear(key: &str) {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let _ = storage.remove_item(key);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = key;
    }

    /// Prefer this revision, otherwise recover a draft without discarding its original revision.
    pub fn recover(prefix: &str, current: &str) -> Option<(String, bool)> {
        if let Some(text) = load(current) {
            return Some((text, false));
        }
        let key = recovery_key(stored_keys(), prefix, current)?;
        load(&key).map(|text| (text, true))
    }

    pub(super) fn recovery_key(
        mut keys: Vec<String>,
        prefix: &str,
        current: &str,
    ) -> Option<String> {
        keys.sort();
        keys.into_iter()
            .rev()
            .find(|key| is_other_revision(key, prefix, current))
    }

    fn stored_keys() -> Vec<String> {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let count = storage.length().unwrap_or(0);
            let keys = (0..count).map(|index| storage.key(index));
            return keys.filter_map(|key| key.ok().flatten()).collect();
        }
        Vec::new()
    }

    /// Revisions are Git object names and never contain `:`, so a path that contains a colon
    /// cannot pass for another file's draft.
    pub fn is_other_revision(key: &str, prefix: &str, keep: &str) -> bool {
        match key.strip_prefix(prefix) {
            Some(revision) => key != keep && !revision.contains(':'),
            None => false,
        }
    }
}

pub fn editor(repo: &RepoChrome, editor: &Editor) -> Element {
    let key = format!(
        "{}:{}:{}:{}:{}",
        repo.viewer_id, repo.id, editor.branch, editor.path, editor.creating
    );
    // Dioxus reconciles keys in fragments, not a lone dynamic component slot.
    rsx! { for key in [key] { EditorView { key: "{key}", repo: repo.clone(), editor: editor.clone() } } }
}

/// Drafts are kept per viewer, repository, branch and file. The revision comes last, so every
/// revision of one file shares this prefix.
fn draft_prefix(repo: &RepoChrome, editor: &Editor) -> String {
    format!(
        "repo-editor:{}:{}:{}:{}:{}:",
        repo.viewer_id, repo.id, editor.branch, editor.path, editor.creating
    )
}

/// The references a preview links: the page's own, only while the file being typed is Markdown. A
/// file that is not Markdown is not previewed as a document, so it links nothing.
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
    // Preserve the loaded revision together with the draft across unrelated page refreshes.
    // A new repository/branch/path gets a different component key and a new snapshot.
    let (draft_key, original) = use_hook(|| {
        let prefix = draft_prefix(&repo, &editor);
        let key = format!("{prefix}{}", editor.expected_revision);
        (key, editor.content.clone())
    });
    // A page that came back from the server with an error or a preview already carries
    // the text; otherwise the draft left in this tab comes back.
    let mut stale_draft = use_signal(|| false);
    let (editor, recovered_stale) = use_hook(|| {
        let mut start = editor.clone();
        let mut recovered_stale = false;
        if start.error.is_none() && !start.preview {
            if let Some((text, stale)) =
                saved_draft::recover(&draft_prefix(&repo, &editor), &draft_key)
            {
                start.content = text;
                recovered_stale = stale;
            }
        }
        (start, recovered_stale)
    });
    use_effect(move || stale_draft.set(recovered_stale));
    let input_key = draft_key.clone();
    // After a rejected save or a preview the page's text is the submitted text, not the file
    // as Git has it, so it stays saved as a draft until the file is committed.
    let keep_draft = editor.error.is_some() || editor.preview;
    // DraftTextarea owns the live field; this observer only drives Markdown preview.
    // The field starts from the server's text, the same text the server rendered. The unsent
    // draft comes back through the field's own restore, which reports it through `on_value`, so
    // the gutter and the line count follow the field's text.
    let mut content = use_signal(|| original.clone());
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
    let preview_base = link_base(
        &repo.base(),
        &editor.branch,
        editor
            .path
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory),
    );
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
    let top_draft_key = draft_key.clone();
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
                        span { style: "display: contents", onclick: move |_| saved_draft::clear(&top_draft_key),
                            NavLink { class: "btn btn-secondary".to_owned(), href: cancel_url.clone(), "Cancel" }
                        }
                    }
                }
                if let Some(error) = &editor.error { div { class: "repo-alert repo-alert-error", role: "alert", strong { if editor.saved { "{SAVED_FOLLOWUP_FAILED}" } else { "Changes were not saved" } } p { "{error}" } } }
                if stale_draft() {
                    p { class: "collaboration-state-banner", role: "status", "Recovered unsaved text from an older revision. Review it against the current file before saving." }
                }
                form { id: "repo-editor-form", method: "post", action: write_action, class: "repo-editor-form",
                    {csrf(&repo.csrf_token)}
                    input { r#type: "hidden", name: "repository_id", value: "{repo.id}" }
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
                        label { class: "repo-sr-only", r#for: "repository-file-content", "File contents" }
                        div { class: "repo-code-editor-body",
                            pre { class: "repo-code-editor-gutter", aria_hidden: "true", {gutter()} }
                            div { class: "repo-editor-stack",
                                EditorHighlight { text: content, path: live_path, enabled: hydrated() }
                                DraftTextarea { id: "repository-file-content", class: "repo-editor-textarea", name: "content", rows: lines().max(10) as u32, initial_value: original.clone(), draft: Some(editor.content.clone()), on_value: move |next: String| {
                                    if next == original && !keep_draft { saved_draft::clear(&input_key) } else { saved_draft::save(&input_key, &next) }
                                    content.set(next);
                                }, on_scroll: move |event: ScrollEvent| follow_scroll(event.data().scroll_top(), event.data().scroll_left()) }
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
                                span { style: "display: contents", onclick: move |_| saved_draft::clear(&draft_key),
                                    NavLink { class: "btn btn-secondary".to_owned(), href: cancel_url, "Cancel" }
                                }
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
    use super::saved_draft::is_other_revision;
    use super::References;

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

    #[test]
    fn older_revision_drafts_are_available_for_recovery() {
        let prefix = "repo-editor:1:2:main:a.md:false:";
        let old = format!("{prefix}old");
        assert_eq!(
            super::saved_draft::recovery_key(
                vec![old.clone(), "unrelated".into()],
                prefix,
                &format!("{prefix}new")
            ),
            Some(old)
        );
    }

    #[test]
    fn only_other_revisions_of_the_same_file_match() {
        let prefix = "repo-editor:1:alice/p:main:a.md:false:";
        let keep = "repo-editor:1:alice/p:main:a.md:false:new";
        let stale = format!("{prefix}old");
        assert!(is_other_revision(&stale, prefix, keep));
        assert!(!is_other_revision(keep, prefix, keep));
        // A path with a colon is another file, even when its key starts the same way.
        let colon = format!("{prefix}x:false:old");
        assert!(!is_other_revision(&colon, prefix, keep));
        let other = "repo-editor:1:alice/p:main:b.md:false:old";
        assert!(!is_other_revision(other, prefix, keep));
    }
}
