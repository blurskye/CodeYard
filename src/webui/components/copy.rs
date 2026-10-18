use dioxus::prelude::*;

/// A button that copies `text` to the clipboard. It shows `label` as its text, or with `icon_only`
/// the copy glyph, which names itself in a tooltip from `label`. `label` is also the accessible
/// name. A `menu_item` is a row of a "…" menu, with the copy glyph before its label.
#[component]
pub fn CopyButton(
    text: String,
    #[props(default = "Copy".to_string())] label: String,
    #[props(default = false)] icon_only: bool,
    #[props(default = false)] menu_item: bool,
) -> Element {
    let ready = use_signal(|| false);
    let notice = use_signal(String::new);
    #[cfg(target_arch = "wasm32")]
    {
        let mut ready = ready;
        use_effect(move || ready.set(true));
    }
    let class = if menu_item {
        "repo-menu-item"
    } else if icon_only {
        "repo-icon-button icon-tooltip"
    } else {
        "btn btn-secondary btn-sm"
    };
    rsx! {
        button { r#type: "button", disabled: !ready(), class: class, "aria-label": "{label}",
            onclick: move |_| {
                #[cfg(target_arch = "wasm32")]
                {
                    let text = text.clone();
                    let mut notice = notice;
                    spawn(async move {
                        use wasm_bindgen::JsCast;
                        let clipboard = web_sys::window().and_then(|window| {
                            js_sys::Reflect::get(window.navigator().as_ref(), &"clipboard".into()).ok()
                                .filter(|value| !value.is_null() && !value.is_undefined())
                                .and_then(|value| value.dyn_into::<web_sys::Clipboard>().ok())
                        });
                        let result = match clipboard {
                            Some(clipboard) => wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&text)).await,
                            None => Err(wasm_bindgen::JsValue::NULL),
                        };
                        notice.set(if result.is_ok() { "Copied.".into() } else { "Copy was denied. Select the text and copy it normally.".into() });
                    });
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = &text;
            },
            if icon_only || menu_item {
                svg { class: "repo-icon", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", width: "16", height: "16", "aria-hidden": "true", "focusable": "false",
                    rect { x: "9", y: "9", width: "13", height: "13", rx: "2" }
                    path { d: "M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" }
                }
            }
            if !icon_only {
                "{label}"
            }
        }
        span { role: "status", class: if icon_only { "copy-button-status repo-sr-only" } else { "copy-button-status" }, "{notice}" }
    }
}
