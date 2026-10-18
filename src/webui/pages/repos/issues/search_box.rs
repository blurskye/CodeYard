//! The search box of an issue or pull request list. Without JavaScript it is a plain text field in
//! the list's GET form. Once the page has hydrated, the word at the caret gets suggestions, and the
//! qualifiers in the text are highlighted. The form still submits the text as it is.
use super::dom::{input_caret, input_scroll, scroll_into_view, set_input_caret};
use super::search::{byte_index, highlight_pieces, insert_word, suggestions, utf16_index, word_at};
use super::{Kind, ListFilters};
use dioxus::prelude::*;

const INPUT_ID: &str = "list-search-input";
const SUGGESTIONS_ID: &str = "list-search-suggestions";

/// The search field of a list. `value` is the text the list's filters name, and `filters` holds the
/// labels, milestones and people that the suggestions offer.
#[component]
pub(super) fn SearchBox(kind: Kind, value: String, filters: ListFilters) -> Element {
    let mut text = use_signal(|| value.clone());
    let mut caret = use_signal(|| utf16_index(&value, value.len()));
    let mut open = use_signal(|| false);
    let mut active = use_signal(|| None::<usize>);
    let mut enhanced = use_signal(|| false);
    let mut scroll = use_signal(|| 0_i32);
    let mut pending = use_signal(|| None::<usize>);
    use_effect(move || enhanced.set(true));
    // A new caret is placed after the render, because setting the value moves the caret to the end.
    use_effect(move || {
        if let Some(position) = pending() {
            set_input_caret(INPUT_ID, position);
            pending.set(None);
        }
    });
    let current = text();
    let word = word_at(&current, byte_index(&current, caret()))
        .map(|span| current[span].to_owned())
        .unwrap_or_default();
    let rows = if word.is_empty() {
        Vec::new()
    } else {
        suggestions(&word, &filters)
    };
    let count = rows.len();
    let showing = enhanced() && open() && count > 0;
    let highlighted = active().filter(|index| *index < count);
    let placeholder = format!("Search {}", kind.title().to_lowercase());
    let pieces = highlight_pieces(&current);
    let offset = scroll();
    let key_rows = rows.clone();

    // Puts the chosen text in place of the word at the caret. The suggestions for the text after it
    // can show at once.
    let mut pick = move |insert: String| {
        let before = text();
        let Some(span) = word_at(&before, byte_index(&before, caret())) else {
            return;
        };
        let (next, byte) = insert_word(&before, span, &insert);
        let position = utf16_index(&next, byte);
        text.set(next);
        caret.set(position);
        pending.set(Some(position));
        active.set(None);
        open.set(true);
    };
    // Reads the caret and scroll position the browser has after a key, a click or a focus.
    let mut sync = move || {
        if let Some(position) = input_caret(INPUT_ID) {
            caret.set(position);
        }
        scroll.set(input_scroll(INPUT_ID));
    };

    rsx! {
        div { class: if enhanced() { "collaboration-search-field is-enhanced" } else { "collaboration-search-field" },
            if enhanced() {
                // The field's own text is transparent, and this layer draws the same text over it, so
                // the qualifiers are highlighted exactly where the caret and selection sit.
                div { class: "collaboration-search-mirror", aria_hidden: "true",
                    span { class: "collaboration-search-mirror-text", style: "transform: translateX(-{offset}px)",
                        for (index, (is_qualifier, piece)) in pieces.into_iter().enumerate() {
                            span { key: "{index}", class: if is_qualifier { "collaboration-token" } else { "" }, "{piece}" }
                        }
                    }
                }
            }
            input { id: INPUT_ID, r#type: "search", name: "q", class: "collaboration-input", value: "{current}", maxlength: "200", placeholder: "{placeholder}", aria_label: "{placeholder}", autocomplete: "off",
                role: if enhanced() { Some("combobox".to_owned()) } else { None },
                "aria-autocomplete": if enhanced() { Some("list".to_owned()) } else { None },
                "aria-expanded": if enhanced() { Some((if showing { "true" } else { "false" }).to_owned()) } else { None },
                "aria-controls": if showing { Some(SUGGESTIONS_ID.to_owned()) } else { None },
                "aria-activedescendant": highlighted.filter(|_| showing).map(|index| format!("{SUGGESTIONS_ID}-option-{index}")),
                oninput: move |event| {
                    let next = event.value();
                    let position = input_caret(INPUT_ID).unwrap_or_else(|| utf16_index(&next, next.len()));
                    text.set(next);
                    caret.set(position);
                    active.set(None);
                    open.set(true);
                },
                onkeydown: move |event| {
                    if !showing {
                        return;
                    }
                    match event.key() {
                        Key::ArrowDown => {
                            event.prevent_default();
                            let next = highlighted.map_or(0, |index| (index + 1).min(count - 1));
                            active.set(Some(next));
                            scroll_into_view(&format!("{SUGGESTIONS_ID}-option-{next}"));
                        }
                        Key::ArrowUp => {
                            event.prevent_default();
                            let previous = highlighted.map_or(count - 1, |index| index.saturating_sub(1));
                            active.set(Some(previous));
                            scroll_into_view(&format!("{SUGGESTIONS_ID}-option-{previous}"));
                        }
                        Key::Enter => {
                            // Enter without a highlighted row submits the form as usual.
                            if let Some(row) = highlighted.and_then(|index| key_rows.get(index)) {
                                event.prevent_default();
                                pick(row.insert.clone());
                            }
                        }
                        Key::Escape => {
                            event.prevent_default();
                            open.set(false);
                        }
                        _ => {}
                    }
                },
                onkeyup: move |_| sync(),
                onclick: move |_| sync(),
                onfocus: move |_| sync(),
                onblur: move |_| {
                    open.set(false);
                    active.set(None);
                },
            }
            if showing {
                div { id: SUGGESTIONS_ID, class: "native-menu-panel collaboration-suggest", role: "listbox", aria_label: "Search suggestions",
                    // The list keeps focus in the field, so a click on a row does not end the search.
                    onmousedown: move |event| event.prevent_default(),
                    for (index, row) in rows.iter().enumerate() {
                        {
                            let insert = row.insert.clone();
                            rsx! {
                                div { key: "{row.insert}", id: "{SUGGESTIONS_ID}-option-{index}", role: "option", class: if highlighted == Some(index) { "collaboration-suggest-option is-active" } else { "collaboration-suggest-option" },
                                    "aria-selected": if highlighted == Some(index) { "true" } else { "false" },
                                    onmouseenter: move |_| active.set(Some(index)),
                                    onclick: move |_| pick(insert.clone()),
                                    if row.name {
                                        svg { class: "collaboration-suggest-icon", view_box: "0 0 16 16", width: "16", height: "16", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linejoin: "round", "aria-hidden": "true", "focusable": "false",
                                            path { d: "M2.5 3.5h11l-4 5v4l-3-1.5v-2.5z" }
                                        }
                                    }
                                    span { class: "collaboration-suggest-text", "{row.text}" }
                                    if let Some(colour) = &row.colour {
                                        span { class: "collaboration-suggest-dot", style: "background: {colour}", aria_hidden: "true" }
                                    }
                                    if !row.hint.is_empty() {
                                        span { class: "collaboration-suggest-hint", "{row.hint}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
