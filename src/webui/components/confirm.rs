use dioxus::prelude::*;

/// The confirmation for a destructive action: a centred dialog over a backdrop. It sits in the
/// `details` that opens it, or, with an `id`, opens when its fragment is the target (pages with no
/// disclosure widgets link to it). Cancel and the backdrop link to `cancel`, which reloads the page,
/// so the dialog closes without JavaScript. `fields` holds the form's hidden fields and inputs.
pub fn confirm_dialog(
    id: Option<&str>,
    label: &str,
    cancel: &str,
    action: &str,
    fields: Element,
    submit: &str,
) -> Element {
    rsx! {
        div { id: id.map(str::to_owned), class: "repo-confirm-modal", role: "dialog", aria_label: label.to_owned(),
            a { class: "repo-confirm-backdrop", href: cancel.to_owned(), aria_label: "Cancel", tabindex: "-1" }
            form { method: "post", action: action.to_owned(), class: "repo-confirm-card",
                {fields}
                div { class: "repo-confirm-actions",
                    a { class: "btn btn-secondary", href: cancel.to_owned(), "Cancel" }
                    button { class: "btn btn-danger", r#type: "submit", "{submit}" }
                }
            }
        }
    }
}
