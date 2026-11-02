use crate::webui::encode_component;
use dioxus::prelude::*;

/// The picture of an account, drawn at `size` pixels. It is decorative: the name always sits
/// next to it, so the image has no text of its own. `/avatars/NAME` answers for any name.
#[component]
pub fn Avatar(name: String, size: u32) -> Element {
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
