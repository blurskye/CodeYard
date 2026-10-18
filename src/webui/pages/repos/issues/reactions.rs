//! The reaction bar under a description, a comment or a review. It is one native form: each reaction
//! is a submit button, so it works without JavaScript.
use super::{detail::csrf, Reaction, Repository};
use crate::webui::pages::repos::chrome::icon;
use dioxus::prelude::*;

/// The reactions a user can pick, in display order: the stored name, the emoji and its label.
pub(in crate::webui::pages::repos) const REACTIONS: [(&str, &str, &str); 8] = [
    ("+1", "👍", "Thumbs up"),
    ("-1", "👎", "Thumbs down"),
    ("laugh", "😄", "Laugh"),
    ("hooray", "🎉", "Hooray"),
    ("confused", "😕", "Confused"),
    ("heart", "❤️", "Heart"),
    ("rocket", "🚀", "Rocket"),
    ("eyes", "👀", "Eyes"),
];

fn emoji(content: &str) -> (&'static str, &'static str) {
    REACTIONS
        .iter()
        .find(|(name, _, _)| *name == content)
        .map_or(("", ""), |(_, emoji, label)| (emoji, label))
}

/// The URL a reaction bar posts to. On a later discussion page the form names that page, so the
/// reaction comes back to it.
pub(in crate::webui::pages::repos) fn react_action(url: &str, page: usize) -> String {
    if page > 1 {
        format!("{url}/react?page={page}")
    } else {
        format!("{url}/react")
    }
}

/// `action` is the URL the form posts to. `field` is the hidden field that names the comment or
/// review the reactions belong to, with its id, or `None` for the description.
pub(in crate::webui::pages::repos) fn reaction_bar(
    repo: &Repository,
    action: &str,
    field: Option<(&str, i64)>,
    reactions: &[Reaction],
) -> Element {
    if reactions.is_empty() && !repo.signed_in {
        return rsx! {};
    }
    rsx! {
        form { method: "post", action: action.to_owned(), class: "collaboration-reactions", aria_label: "Reactions",
            {csrf(&repo.csrf)}
            if let Some((name, id)) = field { input { r#type: "hidden", name, value: id.to_string() } }
            for reaction in reactions {
                {
                    let (face, label) = emoji(&reaction.content);
                    let class = if reaction.mine { "collaboration-reaction is-mine" } else { "collaboration-reaction" };
                    rsx! {
                        if repo.signed_in {
                            button { key: "{reaction.content}", r#type: "submit", name: "content", value: reaction.content.clone(), class, title: label, "aria-pressed": reaction.mine.to_string(),
                                span { "{face}" } span { class: "collaboration-reaction-count", "{reaction.count}" }
                            }
                        } else {
                            span { key: "{reaction.content}", class, title: label,
                                span { "{face}" } span { class: "collaboration-reaction-count", "{reaction.count}" }
                            }
                        }
                    }
                }
            }
            if repo.signed_in {
                details { class: "native-menu collaboration-reaction-add",
                    summary { class: "collaboration-reaction", title: "Add reaction", "aria-label": "Add reaction", {icon("smile")} }
                    div { class: "native-menu-panel collaboration-reaction-panel",
                        for (name, face, label) in REACTIONS {
                            button { key: "{name}", r#type: "submit", name: "content", value: name, class: "collaboration-reaction-choice", title: label, "aria-label": label, "{face}" }
                        }
                    }
                }
            }
        }
    }
}
