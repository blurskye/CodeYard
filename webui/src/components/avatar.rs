use crate::encode_component;
use dioxus::prelude::*;

/// The picture of an account, drawn at `size` pixels. It is decorative: the name always sits
/// next to it, so the image has no text of its own. `/avatars/NAME` answers for any name.
#[component]
pub fn Avatar(name: String, size: u32, #[props(default = true)] account: bool) -> Element {
    if !account {
        let letter = name
            .chars()
            .next()
            .map(|letter| letter.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into());
        return rsx! { span { class: "avatar", aria_hidden: "true", style: format!("display:inline-flex;align-items:center;justify-content:center;width:{size}px;height:{size}px;background:var(--muted);color:var(--muted-foreground)"), "{letter}" } };
    }
    rsx! {
        img {
            class: "avatar",
            src: format!("/avatars/{}", encode_component(&name)),
            width: "{size}",
            height: "{size}",
            alt: "",
            loading: "lazy",
        }
    }
}
