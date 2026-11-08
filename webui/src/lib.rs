//! Shared safe Dioxus views: useful server HTML and identical browser hydration.
//! No backend/DB authority crosses into this module.
#![recursion_limit = "256"]
pub mod app;
pub mod bootstrap;
pub mod components;
pub mod document;
pub mod hydration;
pub mod navigation;
pub mod pages;

use codeyard_types::paths::{
    decode_component, encode_component, profile_path, repository_path, workflow_filename,
};

/// Non-executable raw media types shared by response headers and inline previews.
pub fn raw_content_type(path: &str, is_binary: bool) -> &'static str {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ if is_binary => "application/octet-stream",
        _ => "text/plain; charset=utf-8",
    }
}

/// The first `limit` characters of `text`, with an ellipsis when more follow.
pub fn excerpt(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut output: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

#[cfg(test)]
pub(crate) mod tests {
    use dioxus::html::*;
    use std::{any::Any, rc::Rc};

    struct TestEvents;
    macro_rules! convert_events {
        ($($method:ident: $data:ty),* $(,)?) => {
            $(fn $method(&self, event: &PlatformEventData) -> $data {
                event.downcast::<Rc<dyn Fn() -> $data>>().expect("matching event factory")()
            })*
        };
    }
    impl HtmlEventConverter for TestEvents {
        convert_events! {
            convert_animation_data: AnimationData,
            convert_cancel_data: CancelData,
            convert_clipboard_data: ClipboardData,
            convert_composition_data: CompositionData,
            convert_drag_data: DragData,
            convert_focus_data: FocusData,
            convert_form_data: FormData,
            convert_image_data: ImageData,
            convert_keyboard_data: KeyboardData,
            convert_media_data: MediaData,
            convert_mounted_data: MountedData,
            convert_mouse_data: MouseData,
            convert_pointer_data: PointerData,
            convert_resize_data: ResizeData,
            convert_scroll_data: ScrollData,
            convert_selection_data: SelectionData,
            convert_toggle_data: ToggleData,
            convert_touch_data: TouchData,
            convert_transition_data: TransitionData,
            convert_visible_data: VisibleData,
            convert_wheel_data: WheelData,
        }
    }

    /// Headless listeners need the same platform-event conversion as a renderer.
    pub(crate) fn event<T: 'static>(
        factory: impl Fn() -> T + 'static,
    ) -> dioxus::core::Event<dyn Any> {
        set_event_converter(Box::new(TestEvents));
        let factory: Rc<dyn Fn() -> T> = Rc::new(factory);
        let data: Rc<dyn Any> = Rc::new(PlatformEventData::new(Box::new(factory)));
        dioxus::core::Event::new(data, true)
    }

    /// RSX event-handler/default props must be constructed in a framework scope.
    pub(crate) fn in_scope<T>(render: impl FnOnce() -> T) -> T {
        use dioxus::prelude::*;
        let dom = VirtualDom::new(|| rsx! {});
        dom.in_scope(ScopeId::ROOT, render)
    }
}
