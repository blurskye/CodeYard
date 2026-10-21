use super::{Organization, Page};
use crate::webui::components::Avatar;
use crate::webui::pages::fields::csrf;
use dioxus::prelude::*;

/// The organization's picture in a square frame. An uploaded picture is drawn from its file, so
/// a new one shows at once; without one, the generated picture of `/avatars/NAME` is drawn.
/// `frame` names the size class and `size` is the picture's pixel size.
pub(super) fn picture(org: &Organization, frame: &str, size: u32) -> Element {
    rsx! {
        span { class: "{frame}",
            if let Some(src) = &org.avatar_url {
                img { class: "avatar", src: src.clone(), width: "{size}", height: "{size}", alt: "", loading: "lazy" }
            } else {
                Avatar { name: org.name.clone(), size }
            }
        }
    }
}

/// Upload and removal stay native forms, including before hydration. The upload button sits
/// in the button row and names its form with `form`, so both buttons share one row.
pub(super) fn panel(page: &Page, org: &Organization) -> Element {
    let base = format!("/settings/organizations/{}/avatar", org.id);
    rsx! {
        section { class: "org-panel", aria_labelledby: "organization-picture-title",
            header { class: "org-panel-header", h3 { id: "organization-picture-title", "Organization picture" } }
            div { class: "org-panel-body org-picture-editor",
                {picture(org, "org-avatar", 72)}
                div { class: "org-picture-actions",
                    form { id: "organization-picture-upload", method: "post", action: base.clone(), enctype: "multipart/form-data", class: "org-form",
                        {csrf(&page.csrf_token)}
                        label { span { "Upload a new picture" }
                            input { r#type: "file", name: "picture", accept: "image/png,image/jpeg,image/gif,image/webp", required: true }
                        }
                        p { "PNG, JPEG, GIF, or WebP. Maximum 2 MB." }
                    }
                    div { class: "org-picture-buttons",
                        button { r#type: "submit", form: "organization-picture-upload", class: "btn btn-primary", "Upload picture" }
                        if org.avatar_url.is_some() {
                            form { method: "post", action: format!("{base}/remove"),
                                {csrf(&page.csrf_token)}
                                button { r#type: "submit", class: "btn btn-danger", "Remove picture" }
                            }
                        }
                    }
                }
            }
        }
    }
}
