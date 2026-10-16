//! The formatting buttons above a Markdown textarea. A button puts its syntax around the selected
//! text, which only the browser can read, so the buttons stay hidden until the page hydrates. Without
//! JavaScript the textarea is a plain field and nothing is lost.
use super::shell::{shell_icon, ShellIcon};
use dioxus::prelude::*;

/// The Markdown a toolbar button inserts.
#[derive(Clone, Copy)]
enum Markup {
    Heading,
    Bold,
    Italic,
    Quote,
    Code,
    Link,
    BulletList,
    NumberedList,
    TaskList,
}

const MARKUPS: [Markup; 9] = [
    Markup::Heading,
    Markup::Bold,
    Markup::Italic,
    Markup::Quote,
    Markup::Code,
    Markup::Link,
    Markup::BulletList,
    Markup::NumberedList,
    Markup::TaskList,
];

impl Markup {
    fn label(self) -> &'static str {
        match self {
            Markup::Heading => "Heading",
            Markup::Bold => "Bold",
            Markup::Italic => "Italic",
            Markup::Quote => "Quote",
            Markup::Code => "Code",
            Markup::Link => "Link",
            Markup::BulletList => "Bulleted list",
            Markup::NumberedList => "Numbered list",
            Markup::TaskList => "Task list",
        }
    }

    fn icon(self) -> ShellIcon {
        match self {
            Markup::Heading => ShellIcon::Heading,
            Markup::Bold => ShellIcon::Bold,
            Markup::Italic => ShellIcon::Italic,
            Markup::Quote => ShellIcon::Quote,
            Markup::Code => ShellIcon::Code,
            Markup::Link => ShellIcon::Link,
            Markup::BulletList => ShellIcon::ListBullets,
            Markup::NumberedList => ShellIcon::ListNumbers,
            Markup::TaskList => ShellIcon::ListTodo,
        }
    }

    /// The text before and after the selection, and the text that stands in for an empty
    /// selection. None for the markup that starts each line instead.
    #[cfg(target_arch = "wasm32")]
    fn wrap(self) -> Option<(&'static str, &'static str, &'static str)> {
        match self {
            Markup::Bold => Some(("**", "**", "bold text")),
            Markup::Italic => Some(("*", "*", "italic text")),
            Markup::Code => Some(("`", "`", "code")),
            Markup::Link => Some(("[", "](https://)", "link text")),
            _ => None,
        }
    }

    /// What each selected line starts with, for the markup that starts lines.
    #[cfg(target_arch = "wasm32")]
    fn line_prefix(self) -> &'static str {
        match self {
            Markup::Heading => "## ",
            Markup::Quote => "> ",
            Markup::BulletList => "- ",
            Markup::NumberedList => "1. ",
            Markup::TaskList => "- [ ] ",
            _ => "",
        }
    }
}

/// The toolbar for the Markdown textarea with the id `target`. It is drawn with the page, so the
/// layout does not jump on hydration; its buttons stay disabled until the browser can edit the text.
#[component]
pub fn MarkdownToolbar(target: String) -> Element {
    let mut ready = use_signal(|| false);
    rsx! {
        div { class: "collaboration-markdown-toolbar", role: "toolbar", aria_label: "Formatting",
            onmounted: move |_| ready.set(true),
            for markup in MARKUPS {
                {tool(&target, markup, !ready())}
            }
        }
    }
}

fn tool(target: &str, markup: Markup, disabled: bool) -> Element {
    let target = target.to_owned();
    rsx! {
        button { r#type: "button", disabled: disabled, class: "collaboration-markdown-tool", title: markup.label(), aria_label: markup.label(),
            onclick: move |_| {
                #[cfg(target_arch = "wasm32")]
                insert_markup(&target, markup);
                #[cfg(not(target_arch = "wasm32"))]
                let _ = (&target, markup);
            },
            {shell_icon(markup.icon())}
        }
    }
}

/// Edits the textarea `id` in the browser. The selected text gets the markup, or each selected line
/// does. The change is sent as an input event, so the draft and the preview follow it.
#[cfg(target_arch = "wasm32")]
fn insert_markup(id: &str, markup: Markup) {
    use wasm_bindgen::JsCast;
    let Some(area) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
    else {
        return;
    };
    let text = area.value();
    let start = byte_at(&text, area.selection_start().ok().flatten().unwrap_or(0));
    let end = byte_at(&text, area.selection_end().ok().flatten().unwrap_or(0)).max(start);
    // A code selection over several lines becomes a fenced block; a single line gets backticks.
    let fenced = matches!(markup, Markup::Code) && text[start..end].contains('\n');
    let wrap = if fenced {
        Some(("```\n", "\n```", "code"))
    } else {
        markup.wrap()
    };
    let (edited, from, to) = match wrap {
        Some((before, after, placeholder)) => {
            let selected = if start == end {
                placeholder
            } else {
                &text[start..end]
            };
            let edited = format!(
                "{}{before}{selected}{after}{}",
                &text[..start],
                &text[end..]
            );
            let from = start + before.len();
            (edited, from, from + selected.len())
        }
        None => {
            let prefix = markup.line_prefix();
            let first = text[..start].rfind('\n').map_or(0, |at| at + 1);
            let last = text[end..].find('\n').map_or(text.len(), |at| end + at);
            let lines = text[first..last]
                .split('\n')
                .map(|line| format!("{prefix}{line}"))
                .collect::<Vec<_>>()
                .join("\n");
            let edited = format!("{}{lines}{}", &text[..first], &text[last..]);
            (edited, first, first + lines.len())
        }
    };
    area.set_value(&edited);
    // The selection is counted in UTF-16 units, as the browser counts text.
    let unit_at = |at: usize| edited[..at].encode_utf16().count() as u32;
    let _ = area.set_selection_range(unit_at(from), unit_at(to));
    let init = web_sys::EventInit::new();
    init.set_bubbles(true);
    if let Ok(event) = web_sys::Event::new_with_event_init_dict("input", &init) {
        let _ = area.dispatch_event(&event);
    }
}

/// The byte offset of a UTF-16 offset in `text`.
#[cfg(target_arch = "wasm32")]
fn byte_at(text: &str, units: u32) -> usize {
    let mut seen = 0;
    for (at, ch) in text.char_indices() {
        if seen >= units as usize {
            return at;
        }
        seen += ch.len_utf16();
    }
    text.len()
}
