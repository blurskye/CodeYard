use super::{detail::csrf, DetailDraft, Item};
use crate::webui::components::{
    markdown_tab_class, references::References, DraftInput, DraftTextarea, Markdown,
    MarkdownToolbar,
};
use dioxus::prelude::*;

/// A normal native textarea and a Preview submit remain available before hydration; the server
/// answers the submit with the page and its preview. Once hydrated, the tabs switch in place and
/// the preview replaces the textarea without a request, and the form still sends the textarea's text.
/// Actual saves/comments/comparisons still use the authorized native form.
/// The preview links the references the page resolved: a reference typed after the page
/// loaded links only if the page already knew its target, such as a number in this repository.
#[component]
pub(in crate::webui::pages::repos) fn MarkdownEditor(
    id: String,
    initial: String,
    rows: usize,
    required: bool,
    initial_preview: bool,
    #[props(default)] placeholder: String,
    #[props(default)] references: References,
    // The page's revision, which the save checks. See `DraftTextarea`.
    #[props(default)] revision: Option<String>,
    #[props(default)] on_revision: Option<EventHandler<Option<String>>>,
) -> Element {
    let mut text = use_signal(|| initial.clone());
    let mut showing = use_signal(|| initial_preview);
    let mut ready = use_signal(|| false);
    let previewing = showing();
    rsx! {
        div { class: "collaboration-editor-tabs", onmounted: move |_| ready.set(true),
            // Write has nothing to do before hydration, so it is disabled there.
            button { r#type: "button", disabled: !ready(), class: markdown_tab_class(!previewing),
                aria_pressed: if previewing { "false" } else { "true" },
                onclick: move |_| showing.set(false),
                "Write"
            }
            button { r#type: if ready() { "button" } else { "submit" }, name: "intent", value: "preview", formnovalidate: true,
                class: markdown_tab_class(previewing), aria_pressed: if previewing { "true" } else { "false" },
                onclick: move |_| showing.set(true),
                "Preview"
            }
        }
        // Hidden rather than removed while previewing, so the form still sends the text.
        div { hidden: ready() && previewing,
            MarkdownToolbar { target: id.clone() }
            DraftTextarea {
                id: Some(id), name: "body", initial_value: initial, required,
                maxlength: Some(65536), rows: rows as u32, placeholder, revision, on_revision,
                class: "collaboration-textarea",
                on_value: move |value| text.set(value),
            }
            div { class: "collaboration-editor-toolbar",
                span { class: "collaboration-muted", "Markdown is supported" }
            }
        }
        if previewing {
            div { class: "collaboration-preview collaboration-markdown",
                if text().trim().is_empty() {
                    p { class: "collaboration-muted", "Nothing to preview." }
                } else {
                    Markdown { content: text(), references: references.clone() }
                }
            }
        }
    }
}

#[component]
pub(super) fn CommentEditForm(
    action: String,
    csrf_token: String,
    id: i64,
    body: String,
    original_body: String,
    preview: bool,
    references: References,
) -> Element {
    // Keep the original text while navigation refreshes the displayed comment.
    let original = use_signal(|| original_body);
    let original = original.read().clone();
    let token = serde_json::to_string(&original).unwrap_or_default();
    rsx! {
        form { method: "post", action, class: "collaboration-comment-edit-form",
            {csrf(&csrf_token)}
            input { r#type: "hidden", name: "expected_body", value: token }
            MarkdownEditor { id: format!("edit-comment-{id}"), initial: body, rows: 8usize, required: true, initial_preview: preview, references, revision: Some(original) }
            div { class: "collaboration-form-actions",
                button { class: "btn btn-primary", r#type: "submit", "Save changes" }
            }
        }
    }
}

#[derive(Clone)]
struct EditSnapshot {
    title: String,
    body: String,
    expected_state: String,
    expected_updated: String,
    preview: bool,
}

#[component]
pub(super) fn DetailEditForm(
    url: String,
    csrf_token: String,
    item: Item,
    draft: DetailDraft,
    references: References,
) -> Element {
    // A refreshed item may update the displayed article, but cannot advance the
    // optimistic-conflict base of an unsent draft. Logical item identity resets
    // this complete snapshot; CSRF remains current rather than cached with it.
    let snapshot = use_signal(|| EditSnapshot {
        title: draft.title.clone().unwrap_or_else(|| item.title.clone()),
        body: draft.body.clone().unwrap_or_else(|| item.body.clone()),
        expected_state: item.state.clone(),
        expected_updated: item.updated.clone(),
        preview: draft.preview,
    });
    let snapshot = snapshot.read().clone();
    // The revision the save checks, so a draft typed before the item changed is not saved silently.
    let revision = snapshot.expected_updated.clone();
    let mut expected_updated = use_signal(|| revision.clone());
    rsx! {
        form { method: "post", action: format!("{url}/edit"), class: "collaboration-form-fields",
            {csrf(&csrf_token)}
            input { r#type: "hidden", name: "expected_state", value: snapshot.expected_state }
            input { r#type: "hidden", name: "expected_updated", value: expected_updated() }
            label { r#for: "edit-title", "Title" }
            DraftInput { id: "edit-title", name: "title", initial_value: snapshot.title, required: true, maxlength: Some(255), class: "collaboration-input" }
            label { r#for: "edit-body", "Description (Markdown)" }
            MarkdownEditor { id: "edit-body", initial: snapshot.body, rows: 10, required: false, initial_preview: snapshot.preview, references, revision: Some(revision), on_revision: move |revision: Option<String>| expected_updated.set(revision.unwrap_or_default()) }
            div { class: "collaboration-actions", button { r#type: "submit", name: "intent", value: "save", class: "btn btn-primary", "Save changes" } }
        }
    }
}
