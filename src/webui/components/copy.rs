use dioxus::prelude::*;

#[component]
pub fn CopyButton(text: String, #[props(default = "Copy".to_string())] label: String) -> Element {
    let ready = use_signal(|| false);
    let notice = use_signal(String::new);
    #[cfg(target_arch = "wasm32")]
    {
        let mut ready = ready;
        use_effect(move || ready.set(true));
    }
    rsx! {
        button { r#type: "button", disabled: !ready(), class: "btn btn-secondary btn-sm",
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
            "{label}"
        }
        span { role: "status", class: "copy-button-status", "{notice}" }
    }
}
