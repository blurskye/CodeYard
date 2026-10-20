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
    #[props(default)] aria_label: Option<String>,
    #[props(default)] on_value: EventHandler<String>,
) -> Element {
    let mut value = use_signal(move || initial_value);
    rsx! {
        input { r#type: input_type, name, id, class, placeholder, required, readonly, maxlength, pattern, inputmode, "aria-label": aria_label,
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
    #[props(default)] placeholder: String,
    // A draft restored in this browser. It replaces the server text once the page
    // hydrates, unless the user typed in the field before hydration. A caller that passes
    // its own draft keeps it itself, so this field then stores nothing.
    #[props(default)] draft: Option<String>,
    // Caller-owned drafts still participate in successful native-submit cleanup.
    #[props(default)] submit_keys: Vec<String>,
    // The revision of the page this field belongs to, such as the one its save checks. A draft saved
    // against another revision is shown with a notice instead of being restored silently.
    #[props(default)] revision: Option<String>,
    #[props(default)] on_revision: Option<EventHandler<Option<String>>>,
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
    #[allow(unused_mut)]
    let mut stored_key = use_signal(|| {
        if readonly || restored.peek().is_some() {
            None
        } else {
            draft_text::key(&name, id.as_deref())
        }
    });
    // Set when the field shows a draft that was saved against another revision of the page.
    let mut stale = use_signal(|| false);
    #[cfg(target_arch = "wasm32")]
    let mut submit_marks = use_signal(Vec::<draft_text::SubmitMark>::new);
    let submitted_form = try_consume_context::<crate::webui::app::CurrentPage>()
        .and_then(|page| page.0.peek().chrome.submitted_form.clone());
    let mut value = use_signal(move || initial_value);
    let mut typed_revision = use_signal(|| revision.clone());
    let reset_revision = revision.clone();
    let accept_revision = revision.clone();
    rsx! {
        if stale() {
            p { class: "collaboration-state-banner", role: "status",
                "This text was saved before the page changed; review it before saving. "
                button { r#type: "button", class: "btn btn-secondary btn-sm",
                    onclick: move |_| {
                        // The current text is the server's, so the draft goes and the field shows it.
                        let current = initial_text.peek().clone();
                        if let Some(key) = stored_key.peek().as_deref() {
                            draft_text::clear(key);
                        }
                        typed_revision.set(reset_revision.clone());
                        if let Some(handler) = on_revision { handler.call(reset_revision.clone()); }
                        value.set(current.clone());
                        on_value.call(current);
                        stale.set(false);
                    },
                    "Use the current text"
                }
                button { r#type: "button", class: "btn btn-secondary btn-sm",
                    onclick: move |_| {
                        typed_revision.set(accept_revision.clone());
                        if let Some(handler) = on_revision { handler.call(accept_revision.clone()); }
                        if let Some(key) = stored_key.peek().as_deref() {
                            draft_text::save(key, &value.peek(), accept_revision.as_deref());
                        }
                        stale.set(false);
                    },
                    "Use this draft on the current revision"
                }
            }
        }
        textarea { name, id, class, rows, required, readonly, maxlength, placeholder,
            dangerous_inner_html: native_text(),
            value: value(),
            onscroll: move |event| on_scroll.call(event),
            oninput: move |event| {
                let text = event.value();
                if let Some(key) = stored_key.peek().as_deref() {
                    draft_text::save(key, &text, typed_revision.peek().as_deref());
                }
                value.set(text.clone());
                on_value.call(text);
            },
            onmounted: move |event| {
                #[cfg(target_arch = "wasm32")]
                { use wasm_bindgen::JsCast;
                  if let Some(element) = event.data().downcast::<web_sys::Element>() {
                      if let Some(input) = element.dyn_ref::<web_sys::HtmlTextAreaElement>() {
                          if stored_key.peek().is_some() {
                              stored_key.set(draft_text::form_key(element));
                          }
                          let action = element.closest("form").ok().flatten().and_then(|form| form.get_attribute("action"));
                          // Other composers on the page post to the same path but carry no submitted text.
                          let submitted = draft_text::submitted_form_matches(submitted_form.as_deref(), action.as_deref())
                              && !initial_text.peek().is_empty();
                          let stored = stored_key.peek().as_deref().and_then(draft_text::load);
                          let mut text = input.value();
                          let mut stale_draft = false;
                          // Hydration leaves the server text in the field. Text the user
                          // typed before hydration is kept; untouched text gives way to the draft.
                          // defaultValue keeps the LF that the server render pads in front of
                          // the text, so compare with the text itself.
                          if draft_text::should_restore(submitted, &initial_text.peek(), &text) {
                              if let Some(draft) = restored.peek().clone() {
                                  input.set_value(&draft);
                                  text = draft;
                              } else if let Some(stored) = stored {
                                  typed_revision.set(stored.revision.clone());
                                  if let Some(handler) = on_revision { handler.call(stored.revision.clone()); }
                                  let draft = match draft_text::restore(stored, revision.as_deref()) {
                                      draft_text::Restore::Silently(draft) => draft,
                                      draft_text::Restore::Stale(draft) => {
                                          stale_draft = true;
                                          draft
                                      }
                                  };
                                  input.set_value(&draft);
                                  text = draft;
                              }
                          }
                          stale.set(stale_draft);
                          // A stale draft keeps the revision it was typed against, so it stays stale.
                          if let Some(key) = stored_key.peek().as_deref() {
                              if (submitted || text != *initial_text.peek()) && !stale_draft {
                                  draft_text::save(key, &text, revision.as_deref());
                              }
                          }
                          let keys = stored_key.peek().clone().into_iter().chain(submit_keys.iter().cloned());
                          submit_marks.set(keys.filter_map(|key| draft_text::mark_on_submit(element, key)).collect());
                          value.set(text.clone());
                          on_value.call(text);
                      }
                  }
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = (event, restored, initial_text, &revision, &submit_keys, &submitted_form);
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

    /// Every draft key starts with this, so the drafts of a tab can be found and dropped.
    const DRAFT_PREFIX: &str = "codeyard-draft:";

    /// A submitted draft is marked with this prefix until the page that answers the submit loads.
    #[cfg(target_arch = "wasm32")]
    const SENT_PREFIX: &str = "codeyard-draft-sent:";

    /// The account the stored drafts of this tab belong to.
    #[cfg(target_arch = "wasm32")]
    const OWNER_KEY: &str = "codeyard-draft-owner";

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

    // The action identifies the same logical form on its GET and refused POST pages.
    #[cfg(target_arch = "wasm32")]
    pub fn form_key(field: &web_sys::Element) -> Option<String> {
        let name = field.get_attribute("name")?;
        let id = field.get_attribute("id");
        let Some(form) = field.closest("form").ok().flatten() else {
            return key(&name, id.as_deref());
        };
        let Some(action) = form.get_attribute("action") else {
            return key(&name, id.as_deref());
        };
        key_for_form(&action, &name, id.as_deref(), |target| {
            form.query_selector(&format!("input[name='{target}']"))
                .ok()
                .flatten()
                .and_then(|field| field.get_attribute("value"))
        })
    }

    #[cfg(any(target_arch = "wasm32", test))]
    fn key_for_form(
        action: &str,
        name: &str,
        id: Option<&str>,
        mut field_value: impl FnMut(&str) -> Option<String>,
    ) -> Option<String> {
        let mut identity = String::new();
        for target in [
            "repository_id",
            "template",
            "template_revision",
            "path",
            "side",
            "line",
            "in_reply_to",
        ] {
            if let Some(value) = field_value(target) {
                identity.push_str(&format!(
                    ":{target}={}",
                    crate::webui::encode_component(&value)
                ));
            }
        }
        key_for_action(action, &identity, name, id)
    }

    #[cfg(any(target_arch = "wasm32", test))]
    fn key_for_action(
        action: &str,
        identity: &str,
        name: &str,
        id: Option<&str>,
    ) -> Option<String> {
        key_for(
            action.split('#').next().unwrap_or(action),
            identity,
            name,
            id,
        )
    }

    fn key_for(path: &str, search: &str, name: &str, id: Option<&str>) -> Option<String> {
        if looks_secret(name) || id.is_some_and(looks_secret) {
            return None;
        }
        let field = match id {
            Some(id) => format!("id:{id}"),
            None => format!("name:{name}"),
        };
        Some(format!("{DRAFT_PREFIX}{path}{search}:{field}"))
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

    /// A stored draft and the revision of the page it was typed against.
    #[derive(Debug, PartialEq)]
    pub struct Stored {
        pub text: String,
        pub revision: Option<String>,
    }

    /// Where a draft's revision is kept, next to its text.
    #[cfg(target_arch = "wasm32")]
    fn revision_key(key: &str) -> String {
        format!("{key}@revision")
    }

    pub fn load(key: &str) -> Option<Stored> {
        #[cfg(target_arch = "wasm32")]
        {
            let storage = storage()?;
            let text = storage.get_item(key).ok().flatten()?;
            let revision = storage.get_item(&revision_key(key)).ok().flatten();
            Some(Stored { text, revision })
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = key;
            None
        }
    }

    /// Keeps the text as the draft, or drops the draft when the field is empty. `revision` is the
    /// revision of the page the text was typed on.
    pub fn save(key: &str, text: &str, revision: Option<&str>) {
        if text.is_empty() {
            clear(key);
        } else {
            #[cfg(target_arch = "wasm32")]
            if let Some(storage) = storage() {
                let _ = storage.set_item(key, text);
                match revision {
                    Some(revision) => {
                        let _ = storage.set_item(&revision_key(key), revision);
                    }
                    None => {
                        let _ = storage.remove_item(&revision_key(key));
                    }
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            let _ = revision;
        }
    }

    pub fn clear(key: &str) {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let _ = storage.remove_item(key);
            let _ = storage.remove_item(&revision_key(key));
            let _ = storage.remove_item(&format!("{key}@modified"));
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = key;
    }

    pub fn submitted_form_matches(submitted: Option<&str>, action: Option<&str>) -> bool {
        submitted.is_some_and(|submitted| {
            action.is_some_and(|action| action.split(['?', '#']).next() == Some(submitted))
        })
    }

    pub fn should_restore(submitted: bool, initial: &str, text: &str) -> bool {
        !submitted && initial == text
    }

    /// What a field does with a stored draft whose text is untouched.
    #[derive(Debug, PartialEq)]
    pub enum Restore {
        /// Restored as it is.
        Silently(String),
        /// Restored, but the page changed after the draft was typed, so the user reviews it first.
        Stale(String),
    }

    /// A draft typed against the page's current revision is restored. One typed against another
    /// revision is restored too, but marked stale. A field without a revision restores its drafts.
    pub fn restore(stored: Stored, revision: Option<&str>) -> Restore {
        match revision {
            Some(current) if stored.revision.as_deref() != Some(current) => {
                Restore::Stale(stored.text)
            }
            _ => Restore::Silently(stored.text),
        }
    }

    /// Unsent text belongs to the account that typed it. The first page that another account, or
    /// nobody, shows in this tab drops the drafts that the earlier account left there. The page
    /// calls this before its fields restore their drafts.
    pub fn keep_for_account(account: Option<&str>) {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let account = account.unwrap_or_default();
            if storage.get_item(OWNER_KEY).ok().flatten().as_deref() != Some(account) {
                forget_drafts(&storage);
                let _ = storage.set_item(OWNER_KEY, account);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = account;
    }

    #[cfg(any(target_arch = "wasm32", test))]
    fn is_draft_key(key: &str) -> bool {
        key.starts_with(DRAFT_PREFIX)
            || key.starts_with("repo-editor:")
            || key.starts_with("codeyard-draft-sent:")
    }

    #[cfg(target_arch = "wasm32")]
    fn forget_drafts(storage: &web_sys::Storage) {
        let keys: Vec<String> = (0..storage.length().unwrap_or(0))
            .filter_map(|index| storage.key(index).ok().flatten())
            .filter(|key| is_draft_key(key))
            .collect();
        for key in keys {
            let _ = storage.remove_item(&key);
        }
    }

    /// Marks a draft when the form around its field is submitted. The listener stays until this
    /// value is dropped, which happens when the field unmounts.
    #[cfg(target_arch = "wasm32")]
    pub struct SubmitMark {
        form: web_sys::Element,
        listener: wasm_bindgen::closure::Closure<dyn FnMut()>,
    }

    #[cfg(target_arch = "wasm32")]
    impl Drop for SubmitMark {
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
    pub fn mark_on_submit(field: &web_sys::Element, key: String) -> Option<SubmitMark> {
        use wasm_bindgen::{closure::Closure, JsCast};
        let form = field.closest("form").ok()??;
        let submitted_form = form.clone();
        let listener = Closure::<dyn FnMut()>::new(move || {
            let field = submitted_form
                .query_selector("input[name='draft_submission']")
                .ok()
                .flatten()
                .or_else(|| {
                    let field = web_sys::window()?
                        .document()?
                        .create_element("input")
                        .ok()?;
                    field.set_attribute("type", "hidden").ok()?;
                    field.set_attribute("name", "draft_submission").ok()?;
                    field
                        .set_attribute(
                            "value",
                            &format!("{:.0}-{}", js_sys::Date::now(), js_sys::Math::random()),
                        )
                        .ok()?;
                    submitted_form.append_child(&field).ok()?;
                    Some(field)
                });
            if let Some(token) = field.and_then(|field| field.get_attribute("value")) {
                mark_sent(&key, &token);
            }
        });
        form.add_event_listener_with_callback("submit", listener.as_ref().unchecked_ref())
            .ok()?;
        Some(SubmitMark { form, listener })
    }

    /// The form was submitted, so its draft waits for the page that answers the submit.
    #[cfg(target_arch = "wasm32")]
    fn mark_sent(key: &str, token: &str) {
        if let Some(storage) = storage() {
            let _ = storage.set_item(&format!("{SENT_PREFIX}{key}"), token);
        }
    }

    /// Only an explicit server acknowledgment of this submission can clear its drafts.
    pub fn settle_submitted() {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = storage() {
            let acknowledged = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| {
                    use wasm_bindgen::JsCast;
                    document.dyn_into::<web_sys::HtmlDocument>().ok()
                })
                .and_then(|document| document.cookie().ok())
                .and_then(|cookies| {
                    cookies.split(';').find_map(|cookie| {
                        cookie
                            .trim()
                            .strip_prefix("codeyard-draft-accepted=")
                            .map(str::to_owned)
                    })
                });
            let marks: Vec<String> = (0..storage.length().unwrap_or(0))
                .filter_map(|index| storage.key(index).ok().flatten())
                .filter(|key| key.starts_with(SENT_PREFIX))
                .collect();
            for mark in marks {
                let sent = storage.get_item(&mark).ok().flatten();
                if acknowledged_submission(sent.as_deref(), acknowledged.as_deref()) {
                    if let Some(key) = mark.strip_prefix(SENT_PREFIX) {
                        clear(key);
                    }
                }
                let _ = storage.remove_item(&mark);
            }
        }
    }

    #[cfg(any(target_arch = "wasm32", test))]
    fn acknowledged_submission(sent: Option<&str>, accepted: Option<&str>) -> bool {
        sent.is_some_and(|sent| !sent.is_empty() && Some(sent) == accepted)
    }

    #[cfg(all(test, not(target_arch = "wasm32")))]
    mod tests {
        use super::*;

        #[test]
        fn only_matching_success_acknowledgments_clear_drafts() {
            assert!(!acknowledged_submission(Some("failed"), None));
            assert!(!acknowledged_submission(Some("failed"), Some("another")));
            assert!(acknowledged_submission(Some("saved"), Some("saved")));
            assert!(!acknowledged_submission(None, None));
        }

        #[test]
        fn submitted_values_and_pre_hydration_typing_win_over_storage() {
            assert!(submitted_form_matches(
                Some("/a/r/comments"),
                Some("/a/r/comments?view=split")
            ));
            assert!(!submitted_form_matches(
                Some("/a/r/comments"),
                Some("/a/r/edit")
            ));
            assert!(!submitted_form_matches(None, Some("/a/r/comments")));
            assert!(!should_restore(true, "body B", "body B"));
            assert!(!should_restore(false, "default", "typed"));
            assert!(should_restore(false, "default", "default"));
        }

        #[test]
        fn templates_and_repositories_never_share_drafts() {
            let key = |identity| {
                key_for_action("/a/r/issues/new", identity, "form_0", Some("form-field-0"))
            };
            assert_ne!(
                key(":repository_id=1:template=a"),
                key(":repository_id=1:template=b")
            );
            assert_ne!(
                key(":repository_id=1:template=a"),
                key(":repository_id=2:template=a")
            );
        }

        #[test]
        fn get_and_refused_post_use_the_same_logical_form_key() {
            let get = key_for_action("/alice/repo/issues/7/edit", "", "body", Some("edit-body"));
            let refused = key_for_action(
                "/alice/repo/issues/7/edit#preview",
                "",
                "body",
                Some("edit-body"),
            );
            assert_eq!(get, refused);
            assert_ne!(
                key_for_action(
                    "/alice/repo/pulls/7/comments",
                    ":path=a:line=1",
                    "body",
                    Some("line-comment-body")
                ),
                key_for_action(
                    "/alice/repo/pulls/7/comments",
                    ":path=b:line=1",
                    "body",
                    Some("line-comment-body")
                )
            );
        }

        #[test]
        fn new_issue_drafts_belong_to_the_original_repository_and_template() {
            let key =
                |repository: &str, template: &str, revision: &str| {
                    key_for_form("/alice/repo/issues/new", "body", Some("new-body"), |name| {
                        match name {
                            "repository_id" => Some(repository.to_owned()),
                            "template" => Some(template.to_owned()),
                            "template_revision" => Some(revision.to_owned()),
                            _ => None,
                        }
                    })
                };
            let original = key("1", "bug.yml", "A");
            assert_eq!(original, key("1", "bug.yml", "A"));
            assert_ne!(original, key("2", "bug.yml", "A"));
            assert_ne!(original, key("1", "feature.yml", "A"));
            assert_ne!(original, key("1", "bug.yml", "B"));
            assert_ne!(original, key("1", "", ""));
        }

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
        fn account_cleanup_includes_private_editor_text_and_submit_marks() {
            assert!(is_draft_key("repo-editor:1:2:main:private.rs:false:abc"));
            assert!(is_draft_key("codeyard-draft:/settings:name:bio"));
            assert!(is_draft_key(
                "codeyard-draft-sent:codeyard-draft:/settings:name:bio"
            ));
            assert!(!is_draft_key("codeyard-theme"));
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

        #[test]
        fn a_draft_typed_against_another_revision_is_marked_stale() {
            let stored = |revision: Option<&str>| Stored {
                text: "draft".into(),
                revision: revision.map(str::to_owned),
            };
            let draft = || "draft".to_owned();
            assert_eq!(
                restore(stored(Some("r1")), Some("r1")),
                Restore::Silently(draft())
            );
            assert_eq!(
                restore(stored(Some("r1")), Some("r2")),
                Restore::Stale(draft())
            );
            assert_eq!(restore(stored(None), Some("r2")), Restore::Stale(draft()));
            assert_eq!(
                restore(stored(Some("r1")), None),
                Restore::Silently(draft())
            );
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

fn type_ahead_allowed(modifiers: Modifiers) -> bool {
    !modifiers.intersects(Modifiers::CONTROL | Modifiers::META | Modifiers::ALT)
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
    let enhance = options.len() <= 100;
    use_effect(move || enhanced.set(enhance));
    let mut open = use_signal(|| false);
    let mut active = use_signal(|| 0usize);
    let instance = use_hook(|| SELECT_INSTANCES.fetch_add(1, Ordering::Relaxed));
    if !enhance {
        // The browser owns large lists. Arrow keys must not rebuild thousands of VDOM options.
        let current = value.peek().clone();
        return rsx! {
            select { name, id, class, required, disabled, form, aria_label,
                oninput: move |event| on_value.call(event.value()),
                for option in &options {
                    option { key: "{option.value}", value: option.value.clone(), selected: current == option.value, "{option.label}" }
                }
            }
        };
    }
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
                button { id: button_id, r#type: "button", role: "combobox", class: "app-select-button {class}", disabled,
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
                        use dioxus::html::ModifiersInteraction;
                        if !type_ahead_allowed(event.modifiers()) {
                            return;
                        }
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

/// Submits the form `id` as if its submit button were pressed. Filters use it to apply a
/// choice at once once the page hydrates; without scripts the submit button does the job.
pub fn submit_form(id: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let form = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id(id))
            .and_then(|element| element.dyn_into::<web_sys::HtmlFormElement>().ok());
        if let Some(form) = form {
            let _ = form.request_submit();
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = id;
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
    fn enhanced_select_owns_its_active_option_as_a_combobox() {
        fn root() -> Element {
            rsx! { DraftSelect { name: "choice", initial_value: "a", aria_label: "Choice", options: vec![SelectOption::new("a", "A")] } }
        }
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        for _ in 0..3 {
            dom.render_immediate_to_vec();
        }
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("role=\"combobox\""), "{html}");
        assert!(html.contains("aria-haspopup=\"listbox\""));
        assert!(html.contains("aria-label=\"Choice\""));
    }

    #[test]
    fn large_selects_stay_native_after_hydration() {
        fn root() -> Element {
            rsx! { DraftSelect { name: "branch", initial_value: "0", options: (0..10_000).map(|n| SelectOption::new(n.to_string(), n.to_string())).collect::<Vec<_>>() } }
        }
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        for _ in 0..3 {
            dom.render_immediate_to_vec();
        }
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("<select"));
        assert!(!html.contains("combobox"));
        assert_eq!(html.matches("<option").count(), 10_000);
    }

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
    fn browser_shortcuts_never_type_ahead() {
        for modifier in [Modifiers::CONTROL, Modifiers::META, Modifiers::ALT] {
            assert!(!type_ahead_allowed(modifier));
            assert!(!type_ahead_allowed(modifier | Modifiers::SHIFT));
        }
        assert!(type_ahead_allowed(Modifiers::empty()));
        assert!(type_ahead_allowed(Modifiers::SHIFT));
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
