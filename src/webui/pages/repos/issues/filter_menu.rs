//! The list filter menus. Each is a native details element, so without JavaScript it opens and its
//! choices are links that apply the filter. Once the page has hydrated, its search field narrows
//! the choices as it is typed, and the arrow keys and Enter move through them.
use super::detail::{glyph, Glyph};
use super::dom::{click_first_link, focus, scroll_into_view};
use crate::webui::components::Avatar;
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

/// What a choice shows before its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::webui::pages::repos) enum Mark {
    None,
    /// The person's avatar.
    Person(String),
    /// The label's colour, as `#rrggbb`.
    Colour(String),
}

/// One choice in a menu: a link to the list with that choice made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::webui::pages::repos) struct Choice {
    pub href: String,
    pub text: String,
    /// The choice in use. It shows a check mark and is marked as the current one.
    pub chosen: bool,
    pub mark: Mark,
    /// Muted text after the name, such as a milestone's state. Empty when there is none.
    pub detail: String,
}

impl Choice {
    /// A choice with a name, and no mark or detail.
    pub(in crate::webui::pages::repos) fn plain(href: String, text: &str, chosen: bool) -> Self {
        Self {
            href,
            text: text.to_owned(),
            chosen,
            mark: Mark::None,
            detail: String::new(),
        }
    }
}

/// A filter menu: the button that shows the filter's title, and the popover. The popover has a
/// heading, a search field, the choices, and a clear link while a choice is in use.
#[component]
pub(in crate::webui::pages::repos) fn FilterMenu(
    id: String,
    title: String,
    heading: String,
    search_label: String,
    searchable: bool,
    set: bool,
    choices: Vec<Choice>,
    empty: String,
    clear: Option<String>,
) -> Element {
    let mut query = use_signal(String::new);
    let mut active = use_signal(|| None::<usize>);
    let mut enhanced = use_signal(|| false);
    use_effect(move || enhanced.set(true));
    let typed = query();
    let needle = typed.trim().to_lowercase();
    let shown: Vec<Choice> = choices
        .iter()
        .filter(|choice| needle.is_empty() || choice.text.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    let count = shown.len();
    let highlighted = active().filter(|index| *index < count);
    let search_id = format!("{id}-search");
    let details_class = if set {
        "native-menu collaboration-filter-menu collaboration-filter-set"
    } else {
        "native-menu collaboration-filter-menu"
    };
    let popover_class = if enhanced() {
        "collaboration-filter-popover is-enhanced"
    } else {
        "collaboration-filter-popover"
    };
    let toggle_search = search_id.clone();
    let key_prefix = id.clone();
    let list_id = format!("{id}-listbox");
    rsx! {
        details { id: "{id}-menu", class: details_class,
            ontoggle: move |_| {
                // Each opening starts with an empty search and no highlighted choice.
                query.set(String::new());
                active.set(None);
                focus(&toggle_search);
            },
            summary { class: "collaboration-filter-button", span { "{title}" } {glyph(Glyph::Caret)} }
            div { class: "native-menu-panel collaboration-filter-panel",
                div { class: popover_class,
                    h3 { class: "collaboration-filter-heading", "{heading}" }
                    if searchable {
                    input { id: "{search_id}", r#type: "search", class: "collaboration-filter-search", placeholder: "{search_label}", aria_label: "{search_label}", autocomplete: "off", value: "{typed}",
                        role: if enhanced() { Some("combobox".to_owned()) } else { None },
                        "aria-autocomplete": if enhanced() { Some("list".to_owned()) } else { None },
                        "aria-expanded": if enhanced() { Some("true".to_owned()) } else { None },
                        "aria-controls": if enhanced() { Some(list_id.clone()) } else { None },
                        "aria-activedescendant": highlighted.filter(|_| enhanced()).map(|index| format!("{key_prefix}-option-{index}")),
                        oninput: move |event| {
                            query.set(event.value());
                            active.set(None);
                        },
                        onkeydown: move |event| {
                            match event.key() {
                                Key::ArrowDown if count > 0 => {
                                    event.prevent_default();
                                    let next = highlighted.map_or(0, |index| (index + 1).min(count - 1));
                                    active.set(Some(next));
                                    scroll_into_view(&format!("{key_prefix}-option-{next}"));
                                }
                                Key::ArrowUp if count > 0 => {
                                    event.prevent_default();
                                    let previous = highlighted.map_or(count - 1, |index| index.saturating_sub(1));
                                    active.set(Some(previous));
                                    scroll_into_view(&format!("{key_prefix}-option-{previous}"));
                                }
                                Key::Enter => {
                                    if let Some(index) = highlighted {
                                        event.prevent_default();
                                        click_first_link(&format!("{key_prefix}-option-{index}"));
                                    }
                                }
                                _ => {}
                            }
                        },
                    }
                    }
                    ul { id: "{list_id}", class: "collaboration-filter-list",
                        role: if enhanced() { Some("listbox".to_owned()) } else { None },
                        "aria-label": if enhanced() { Some(heading.clone()) } else { None },
                        for (index, choice) in shown.iter().enumerate() {
                            li { key: "{choice.href}", id: "{id}-option-{index}", class: if highlighted == Some(index) { "collaboration-filter-item is-active" } else { "collaboration-filter-item" },
                                role: if enhanced() { Some("option".to_owned()) } else { None },
                                "aria-selected": if enhanced() { Some((if highlighted == Some(index) { "true" } else { "false" }).to_owned()) } else { None },
                                onmouseenter: move |_| active.set(Some(index)),
                                NavLink { href: choice.href.clone(), class: if choice.chosen { "collaboration-filter-option collaboration-filter-chosen" } else { "collaboration-filter-option" }, aria_current: choice.chosen.then(|| "true".to_owned()),
                                    span { class: "collaboration-filter-check", aria_hidden: "true", if choice.chosen { {glyph(Glyph::Check)} } }
                                    if let Mark::Person(name) = &choice.mark { Avatar { name: name.clone(), size: 16 } }
                                    if let Mark::Colour(colour) = &choice.mark { span { class: "collaboration-filter-dot", style: "background: {colour}", aria_hidden: "true" } }
                                    span { class: "collaboration-filter-name", "{choice.text}" }
                                    if !choice.detail.is_empty() { span { class: "collaboration-filter-detail", "{choice.detail}" } }
                                }
                            }
                        }
                    }
                    if count == 0 {
                        p { class: "collaboration-filter-empty", if choices.is_empty() { "{empty}" } else { "No matches" } }
                    }
                    if let Some(href) = clear {
                        div { class: "collaboration-filter-footer",
                            NavLink { href: href, class: "collaboration-filter-clear", "Clear" }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_are_links_and_the_field_is_no_combobox_before_the_page_hydrates() {
        crate::webui::tests::in_scope(|| {
            let html = dioxus_ssr::render_element(rsx! {
                FilterMenu {
                    id: "label-filter".to_owned(),
                    title: "Label".to_owned(),
                    heading: "Filter by label".to_owned(),
                    search_label: "Search labels".to_owned(),
                    searchable: true,
                    set: false,
                    choices: vec![Choice::plain("/alice/proj/issues?label=bug".into(), "bug", false)],
                    empty: "No labels".to_owned(),
                    clear: None,
                }
            });
            assert!(
                html.contains("href=\"/alice/proj/issues?label=bug\""),
                "{html}"
            );
            assert!(html.contains("type=\"search\""), "{html}");
            // Without the hydrated page the list is plain links, so no listbox or option roles.
            assert!(!html.contains("role=\"combobox\""), "{html}");
            assert!(!html.contains("role=\"option\""), "{html}");
            assert!(!html.contains("aria-activedescendant"), "{html}");
        });
    }
}
