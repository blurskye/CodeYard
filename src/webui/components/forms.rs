//! Controlled visible draft fields survive unrelated parent re-renders/polls.
//! Use location/form keys to reset a draft intentionally. Hidden CSRF/revision
//! authority fields are NOT editable drafts and must stay tied to the form snapshot.
use dioxus::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[component]
pub fn DraftInput(
    name: String,
    initial_value: String,
    #[props(default)] id: Option<String>,
    #[props(default)] class: String,
    #[props(default = "text".to_string())] input_type: String,
    #[props(default)] placeholder: String,
    #[props(default)] required: bool,
    #[props(default)] readonly: bool,
    #[props(default)] maxlength: Option<u32>,
    #[props(default)] pattern: Option<String>,
    #[props(default)] inputmode: Option<String>,
    #[props(default)] on_value: EventHandler<String>,
) -> Element {
    let mut value = use_signal(move || initial_value);
    rsx! {
        input { r#type: input_type, name, id, class, placeholder, required, readonly, maxlength, pattern, inputmode,
            value: value(),
            oninput: move |event| {
                let next = event.value();
                value.set(next.clone());
                on_value.call(next);
            },
            onmounted: move |event| {
                // Preserve input typed into useful SSR before WASM finished loading.
                #[cfg(target_arch = "wasm32")]
                { use wasm_bindgen::JsCast;
                  if let Some(element) = event.data().downcast::<web_sys::Element>() {
                      if let Some(input) = element.dyn_ref::<web_sys::HtmlInputElement>() {
                          let next = input.value();
                          value.set(next.clone());
                          on_value.call(next);
                      }
                  }
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = event;
            }
        }
    }
}

/// A refused value, shown beside its control. Give the control `aria_invalid` and an
/// `aria_describedby` that names this id, so assistive technology reads the message too.
#[component]
pub fn FieldError(id: String, message: String) -> Element {
    rsx! { p { id, class: "mt-1 text-sm text-destructive", "{message}" } }
}

#[component]
pub fn DraftTextarea(
    name: String,
    initial_value: String,
    #[props(default)] id: Option<String>,
    #[props(default)] class: String,
    #[props(default = 10)] rows: u32,
    #[props(default)] required: bool,
    #[props(default)] readonly: bool,
    #[props(default)] maxlength: Option<u32>,
    // A draft restored in this browser. It replaces the server text once the page
    // hydrates, unless the user typed in the field before hydration. A caller that passes
    // its own draft keeps it itself, so this field then stores nothing.
    #[props(default)] draft: Option<String>,
    #[props(default)] on_value: EventHandler<String>,
    #[props(default)] on_scroll: EventHandler<ScrollEvent>,
) -> Element {
    // Cache escaped opaque text ONCE: changing innerHTML while typing would reset
    // the native control, caret and hydration ownership. Only value changes live.
    // HTML parsing removes one leading newline after <textarea>; prepend exactly
    // one so the native initial value (including its own leading newline) survives.
    let native_text = use_signal(|| format!("\n{}", super::escape_html(&initial_value)));
    let restored = use_signal(|| draft);
    // Browsers report textarea text with LF line breaks only.
    let initial_text = use_signal(|| initial_value.replace("\r\n", "\n").replace('\r', "\n"));
    // Where this field's unsent text is kept in session storage, if anywhere.
    let stored_key = use_signal(|| {
        if readonly || restored.peek().is_some() {
            None
        } else {
            draft_text::key(&name, id.as_deref())
        }
    });
    #[cfg(target_arch = "wasm32")]
    let mut submit_clear = use_signal(|| None::<draft_text::SubmitClear>);
    let mut value = use_signal(move || initial_value);
    rsx! {
        textarea { name, id, class, rows, required, readonly, maxlength,
            dangerous_inner_html: native_text(),
            value: value(),
            onscroll: move |event| on_scroll.call(event),
            oninput: move |event| {
                let text = event.value();
                if let Some(key) = stored_key.peek().as_deref() {
                    draft_text::save(key, &text);
                }
                value.set(text.clone());
                on_value.call(text);
            },
            onmounted: move |event| {
                let stored = stored_key.peek().as_deref().and_then(draft_text::load);
                #[cfg(target_arch = "wasm32")]
                { use wasm_bindgen::JsCast;
                  if let Some(element) = event.data().downcast::<web_sys::Element>() {
                      if let Some(input) = element.dyn_ref::<web_sys::HtmlTextAreaElement>() {
                          let mut text = input.value();
                          // Hydration leaves the server text in the field. Text the user
                          // typed before hydration is kept; untouched text gives way to the draft.
                          // defaultValue keeps the LF that the server render pads in front of
                          // the text, so compare with the text itself.
                          if *initial_text.peek() == text {
                              if let Some(draft) = restored.peek().clone().or(stored) {
                                  input.set_value(&draft);
                                  text = draft;
                              }
                          }
                          if let Some(key) = stored_key.peek().as_deref() {
                              if text != *initial_text.peek() {
                                  draft_text::save(key, &text);
                              }
                          }
                          if let Some(key) = stored_key.peek().clone() {
                              submit_clear.set(draft_text::clear_on_submit(element, key));
                          }
                          value.set(text.clone());
                          on_value.call(text);
                      }
                  }
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = (event, restored, initial_text, stored);
            }
        }
    }
}

/// Unsent text of a DraftTextarea, kept in this tab's session storage so that leaving the
/// page and coming back (Back, a link, a reload) does not lose it. Storage that is blocked
/// or full only loses the draft: the text stays in the field and the form still submits.
pub mod draft_text {
    /// Field names and ids that suggest a password, token or key. Their text is never stored.
    const SECRET_HINTS: [&str; 6] = ["pass", "token", "secret", "private", "credential", "key"];

    fn looks_secret(text: &str) -> bool {
        let lower = text.to_ascii_lowercase();
        SECRET_HINTS.iter().any(|hint| lower.contains(hint))
    }

    /// The storage key for a field on the current page, or None when the field must not be
    /// stored. A field with an id is keyed by the id, because one page can hold several fields
    /// with the same name, such as the comment box and a review box.
    pub fn key(name: &str, id: Option<&str>) -> Option<String> {
        let (path, search) = location();
        key_for(&path, &search, name, id)
    }

    fn key_for(path: &str, search: &str, name: &str, id: Option<&str>) -> Option<String> {
        if looks_secret(name) || id.is_some_and(looks_secret) {
            return None;
        }
        let field = match id {
            Some(id) => format!("id:{id}"),
            None => format!("name:{name}"),
        };
        Some(format!("codeyard-draft:{path}{search}:{field}"))
    }

    /// The page path and query. Off the browser there is no page, so the key is only a
    /// placeholder; nothing is stored there.
    fn location() -> (String, String) {
        #[cfg(target_arch = "wasm32")]
        if let Some(window) = web_sys::window() {
            let location = window.location();
            return (
                location.pathname().unwrap_or_default(),
                location.search().unwrap_or_default(),
            );
        }
        (String::new(), String::new())
    }

    #[cfg(target_arch = "wasm32")]
    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.session_storage().ok().flatten()
    }

    pub fn load(key: &str) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        {
            storage()?.get_item(key).ok().flatten()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = key;
            None
        }
    }

    /// Keeps the text as the draft, or drops the draft when the field is empty.
    pub fn save(key: &str, text: &str) {
        if text.is_empty() {
            clear(key);
        } else {
            #[cfg(target_arch = "wasm32")]
            if let Some(storage) = storage() {
                let _ = storage.set_item(key, text);
            }
        }
    }

    pub fn clear(key: &str) {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let _ = storage.remove_item(key);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = key;
    }

    /// Clears a draft when the form around its field is submitted. The listener stays
    /// until this value is dropped, which happens when the field unmounts.
    #[cfg(target_arch = "wasm32")]
    pub struct SubmitClear {
        form: web_sys::Element,
        listener: wasm_bindgen::closure::Closure<dyn FnMut()>,
    }

    #[cfg(target_arch = "wasm32")]
    impl Drop for SubmitClear {
        fn drop(&mut self) {
            use wasm_bindgen::JsCast;
            let _ = self.form.remove_event_listener_with_callback(
                "submit",
                self.listener.as_ref().unchecked_ref(),
            );
        }
    }

    /// None when the field is not inside a form.
    #[cfg(target_arch = "wasm32")]
    pub fn clear_on_submit(field: &web_sys::Element, key: String) -> Option<SubmitClear> {
        use wasm_bindgen::{closure::Closure, JsCast};
        let form = field.closest("form").ok()??;
        let listener = Closure::<dyn FnMut()>::new(move || clear(&key));
        form.add_event_listener_with_callback("submit", listener.as_ref().unchecked_ref())
            .ok()?;
        Some(SubmitClear { form, listener })
    }

    #[cfg(all(test, not(target_arch = "wasm32")))]
    mod tests {
        use super::*;

        #[test]
        fn a_draft_is_kept_per_page_query_and_field() {
            let body = key_for("/alice/demo/pulls/6", "", "body", Some("comment-body"));
            assert!(body.is_some());
            assert_eq!(
                body,
                key_for("/alice/demo/pulls/6", "", "body", Some("comment-body"))
            );
            assert_ne!(
                body,
                key_for("/alice/demo/pulls/7", "", "body", Some("comment-body"))
            );
            assert_ne!(
                body,
                key_for(
                    "/alice/demo/pulls/6",
                    "?tab=files",
                    "body",
                    Some("comment-body")
                )
            );
            assert_ne!(
                body,
                key_for("/alice/demo/pulls/6", "", "body", Some("review-body"))
            );
            assert_ne!(
                key_for("/alice/demo/pulls/6", "", "body", None),
                key_for("/alice/demo/pulls/6", "", "body", Some("body"))
            );
        }

        #[test]
        fn secret_looking_fields_keep_no_draft() {
            for (name, id) in [
                ("password", None),
                ("api_token", None),
                ("client_secret", None),
                ("private_key", None),
                ("body", Some("access-token")),
            ] {
                assert_eq!(key_for("/settings", "", name, id), None, "{name} {id:?}");
            }
            assert!(key_for("/settings", "", "bio", Some("bio")).is_some());
        }
    }
}

#[component]
pub fn DraftCheckbox(
    name: String,
    initial_checked: bool,
    #[props(default)] id: Option<String>,
    #[props(default)] class: String,
    #[props(default = "on".to_string())] submission_value: String,
) -> Element {
    let mut checked = use_signal(move || initial_checked);
    rsx! {
        input { r#type: "checkbox", name, id, class, value: submission_value,
            checked: checked(),
            oninput: move |event| checked.set(event.checked()),
            onmounted: move |event| {
                #[cfg(target_arch = "wasm32")]
                { use wasm_bindgen::JsCast;
                  if let Some(element) = event.data().downcast::<web_sys::Element>() {
                      if let Some(input) = element.dyn_ref::<web_sys::HtmlInputElement>() { checked.set(input.checked()); }
                  }
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = event;
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

impl SelectOption {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// The option an arrow key moves to. The list stops at its ends instead of wrapping.
pub fn next_index(current: usize, len: usize) -> usize {
    (current + 1).min(len.saturating_sub(1))
}

/// The option an up arrow moves to, stopping at the first option.
pub fn previous_index(current: usize, len: usize) -> usize {
    current.saturating_sub(1).min(len.saturating_sub(1))
}

/// The option a typed letter jumps to: the first option after `current` whose label starts with
/// that letter, wrapping around and ignoring case.
pub fn type_ahead_index(options: &[SelectOption], current: usize, typed: char) -> Option<usize> {
    let typed = typed.to_lowercase().to_string();
    let len = options.len();
    (1..=len)
        .map(|step| (current + step) % len)
        .find(|&index| options[index].label.to_lowercase().starts_with(&typed))
}

/// The character a key types, when it is one visible character (a space is not).
fn typed_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if !ch.is_whitespace() => Some(ch),
        _ => None,
    }
}

/// Numbers the dropdowns on a page, so that each list and option has its own DOM ids.
static SELECT_INSTANCES: AtomicUsize = AtomicUsize::new(0);

/// A select whose choice is a draft. Without JavaScript, and until the page hydrates, it is the
/// native select. After that, the native select is hidden and a button with a listbox takes its
/// place. The native select stays in the form, so submissions are the same either way.
#[component]
pub fn DraftSelect(
    name: String,
    initial_value: String,
    options: Vec<SelectOption>,
    #[props(default)] id: Option<String>,
    #[props(default)] class: String,
    #[props(default)] required: bool,
    #[props(default)] disabled: bool,
    #[props(default)] form: Option<String>,
    #[props(default)] aria_label: Option<String>,
    #[props(default)] on_value: EventHandler<String>,
) -> Element {
    let mut value = use_signal(move || initial_value);
    let mut enhanced = use_signal(|| false);
    use_effect(move || enhanced.set(true));
    let mut open = use_signal(|| false);
    let mut active = use_signal(|| 0usize);
    let instance = use_hook(|| SELECT_INSTANCES.fetch_add(1, Ordering::Relaxed));
    let native_id = format!("app-select-{instance}-native");
    let list_id = format!("app-select-{instance}-list");
    let button_id = id
        .clone()
        .unwrap_or_else(|| format!("app-select-{instance}-button"));
    let focus_id = button_id.clone();
    let label_id = button_id.clone();
    let current = value();
    let selected = options
        .iter()
        .position(|option| option.value == current)
        .unwrap_or(0);
    let shown = options
        .get(selected)
        .map(|option| option.label.clone())
        .unwrap_or_default();
    let open_now = open();
    let active_now = active();
    let active_option = (open_now && active_now < options.len())
        .then(|| format!("app-select-{instance}-option-{active_now}"));
    let keys = options.clone();
    let option_id = move |index: usize| format!("app-select-{instance}-option-{index}");
    // Records a chosen value, closes the list and tells the caller.
    let mut pick = move |chosen: String| {
        value.set(chosen.clone());
        open.set(false);
        sync_native_select(&format!("app-select-{instance}-native"), &chosen);
        on_value.call(chosen);
    };
    rsx! {
        if enhanced() {
            div { class: "app-select",
                button { id: button_id, r#type: "button", class: "app-select-button {class}", disabled,
                    aria_label,
                    "aria-haspopup": "listbox",
                    "aria-expanded": if open_now { "true" } else { "false" },
                    "aria-controls": if open_now { Some(list_id.clone()) } else { None },
                    "aria-activedescendant": active_option,
                    onblur: move |_| open.set(false),
                    onclick: move |_| {
                        if open() {
                            open.set(false);
                        } else {
                            active.set(selected);
                            open.set(true);
                            // Safari does not focus a button on click, and the keys need focus.
                            focus_element(&focus_id);
                        }
                    },
                    onkeydown: move |event| {
                        let key = event.key();
                        if !open() {
                            // Enter and Space reach the click handler, which opens the list.
                            match key {
                                Key::ArrowDown | Key::ArrowUp => {
                                    event.prevent_default();
                                    active.set(selected);
                                    open.set(true);
                                }
                                Key::Character(typed) => {
                                    if let Some(ch) = typed_char(&typed) {
                                        if let Some(index) = type_ahead_index(&keys, selected, ch) {
                                            event.prevent_default();
                                            pick(keys[index].value.clone());
                                        }
                                    }
                                }
                                _ => {}
                            }
                            return;
                        }
                        let len = keys.len();
                        match key {
                            Key::ArrowDown => active.set(next_index(active(), len)),
                            Key::ArrowUp => active.set(previous_index(active(), len)),
                            Key::Home => active.set(0),
                            Key::End => active.set(len.saturating_sub(1)),
                            Key::Enter => match keys.get(active()) {
                                Some(option) => pick(option.value.clone()),
                                None => open.set(false),
                            },
                            Key::Escape => open.set(false),
                            Key::Tab => {
                                open.set(false);
                                return;
                            }
                            Key::Character(typed) if typed == " " => match keys.get(active()) {
                                Some(option) => pick(option.value.clone()),
                                None => open.set(false),
                            },
                            Key::Character(typed) => match typed_char(&typed) {
                                Some(ch) => {
                                    if let Some(index) = type_ahead_index(&keys, active(), ch) {
                                        active.set(index);
                                    }
                                }
                                None => return,
                            },
                            _ => return,
                        }
                        event.prevent_default();
                        scroll_into_view(&option_id(active()));
                    },
                    span { class: "app-select-value", "{shown}" }
                }
                if open_now {
                    ul { id: list_id.clone(), class: "app-select-list", role: "listbox", "aria-labelledby": label_id,
                        // The list keeps focus on the button, so a click on an option does not close it first.
                        onmousedown: move |event| event.prevent_default(),
                        for (index, option) in options.iter().enumerate() {
                            {
                                let chosen = option.value.clone();
                                rsx! {
                                    li { key: "{option.value}", id: option_id(index), role: "option",
                                        class: if index == active_now { "app-select-option is-active" } else { "app-select-option" },
                                        "aria-selected": if index == selected { "true" } else { "false" },
                                        onmouseenter: move |_| active.set(index),
                                        // A label around the dropdown would otherwise click the button again and reopen the list.
                                        onclick: move |event| {
                                            event.prevent_default();
                                            pick(chosen.clone());
                                        },
                                        "{option.label}"
                                    }
                                }
                            }
                        }
                    }
                }
                // Last in the wrapper, so that a label around the dropdown names the button.
                select { id: native_id.clone(), class: "app-select-native", name, form, required, disabled,
                    tabindex: "-1", "aria-hidden": "true",
                    for option in options.iter() {
                        option { key: "{option.value}", value: option.value.clone(), selected: current == option.value, "{option.label}" }
                    }
                }
            }
        } else {
            select { name, id, class, required, disabled, form, aria_label,
                oninput: move |event| {
                    let next = event.value();
                    value.set(next.clone());
                    on_value.call(next);
                },
                onmounted: move |event| {
                    #[cfg(target_arch = "wasm32")]
                    { use wasm_bindgen::JsCast;
                      if let Some(element) = event.data().downcast::<web_sys::Element>() {
                          if let Some(input) = element.dyn_ref::<web_sys::HtmlSelectElement>() { value.set(input.value()); }
                      }
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    let _ = event;
                },
                for option in options.iter() {
                    option { key: "{option.value}", value: option.value.clone(), selected: current == option.value, "{option.label}" }
                }
            }
        }
    }
}

/// Sets the native select `id` to `value` and sends the events a user's choice would.
fn sync_native_select(id: &str, value: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let select = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id(id))
            .and_then(|element| element.dyn_into::<web_sys::HtmlSelectElement>().ok());
        if let Some(select) = select {
            select.set_value(value);
            for name in ["input", "change"] {
                let init = web_sys::EventInit::new();
                init.set_bubbles(true);
                if let Ok(event) = web_sys::Event::new_with_event_init_dict(name, &init) {
                    let _ = select.dispatch_event(&event);
                }
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (id, value);
}

/// Moves keyboard focus to the element `id`, if it is in the page.
fn focus_element(id: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let element = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id(id))
            .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok());
        if let Some(element) = element {
            let _ = element.focus();
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = id;
}

/// Scrolls the list so that the option `id` is visible after a key moved to it.
fn scroll_into_view(id: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        let element = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id(id));
        if let Some(element) = element {
            element.scroll_into_view_with_bool(false);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = id;
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    #[test]
    fn arrow_keys_stop_at_the_ends_of_the_list() {
        assert_eq!(next_index(0, 3), 1);
        assert_eq!(next_index(2, 3), 2);
        assert_eq!(next_index(0, 0), 0);
        assert_eq!(previous_index(2, 3), 1);
        assert_eq!(previous_index(0, 3), 0);
        assert_eq!(previous_index(0, 0), 0);
    }

    #[test]
    fn type_ahead_finds_the_next_option_with_that_letter() {
        let options = [
            SelectOption::new("a", "Apple"),
            SelectOption::new("b", "Banana"),
            SelectOption::new("c", "Blueberry"),
        ];
        assert_eq!(type_ahead_index(&options, 0, 'b'), Some(1));
        assert_eq!(type_ahead_index(&options, 1, 'b'), Some(2));
        assert_eq!(type_ahead_index(&options, 2, 'b'), Some(1));
        assert_eq!(type_ahead_index(&options, 0, 'B'), Some(1));
        assert_eq!(type_ahead_index(&options, 0, 'z'), None);
        assert_eq!(type_ahead_index(&[], 0, 'a'), None);
    }

    #[test]
    fn only_a_single_visible_character_types_ahead() {
        assert_eq!(typed_char("x"), Some('x'));
        assert_eq!(typed_char(" "), None);
        assert_eq!(typed_char("Enter"), None);
    }

    #[test]
    fn native_drafts_have_real_values_and_escaped_text_without_browser_runtime() {
        crate::webui::tests::in_scope(|| {
            let html = dioxus_ssr::render_element(rsx! {
                DraftInput { name: "q", initial_value: "a&b\"c" }
                DraftCheckbox { name: "enabled", initial_checked: true }
                DraftSelect { name: "ref", initial_value: "feature/branch", options: vec![
                    SelectOption { value: "main".into(), label: "Main".into() },
                    SelectOption { value: "feature/branch".into(), label: "Feature".into() }
                ] }
            });
            assert!(html.contains("value=\"a&#38;b&#34;c\""));
            assert!(html.contains("value=\"feature/branch\""));
            assert!(html.contains("selected"));
            assert!(html.contains("checked"));
            // Before hydration the native select is the control, with its options.
            assert!(html.contains("<select"));
            assert!(!html.contains("app-select"));
        });
    }
    #[test]
    fn prerendered_textarea_has_native_source_not_dynamic_text_comment_markers() {
        crate::webui::tests::in_scope(|| {
            for initial in [
                "",
                "plain",
                "\nleading",
                "\n\ntwo",
                "\n</textarea><script>bad</script>&",
                "<!--node-id0-->",
            ] {
                let mut renderer = dioxus_ssr::Renderer::new();
                renderer.pre_render = true;
                let html = renderer.render_element(rsx! {
                    DraftTextarea { name: "content", initial_value: initial.to_owned() }
                });
                let textarea = html.split_once("<textarea").unwrap().1;
                let content = textarea
                    .split_once('>')
                    .unwrap()
                    .1
                    .split_once("</textarea>")
                    .unwrap()
                    .0;
                // HTML removes only the first prefixed newline; authored ones remain.
                assert_eq!(content, format!("\n{}", super::super::escape_html(initial)));
                assert!(!content.contains("<!--"));
                assert!(!content.contains("<script>"));
            }
        });
    }

    #[test]
    fn opaque_native_text_is_fixed_for_the_mounted_field_snapshot() {
        use std::{cell::RefCell, rc::Rc};
        #[derive(Clone)]
        struct Harness(Rc<RefCell<Option<Signal<String>>>>);
        fn root(props: Harness) -> Element {
            let initial = use_signal(|| "\noriginal </textarea>".to_owned());
            *props.0.borrow_mut() = Some(initial);
            rsx! { DraftTextarea { name: "content", initial_value: initial() } }
        }
        let control = Rc::new(RefCell::new(None));
        let mut dom = VirtualDom::new_with_props(root, Harness(control.clone()));
        dom.rebuild_in_place();
        let mut initial = control.borrow().unwrap();
        dom.in_runtime(|| initial.set("new server props".to_owned()));
        dom.render_immediate_to_vec();
        let mut renderer = dioxus_ssr::Renderer::new();
        renderer.pre_render = true;
        let html = renderer.render(&dom);
        let textarea = html.split_once("<textarea").unwrap().1;
        let content = textarea
            .split_once('>')
            .unwrap()
            .1
            .split_once("</textarea>")
            .unwrap()
            .0;
        assert_eq!(content, "\n\noriginal &lt;/textarea&gt;");
        assert!(!content.contains("new server props"));
        assert!(!content.contains("<!--"));
    }
}
