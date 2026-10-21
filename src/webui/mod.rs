//! Shared safe Dioxus views: useful server HTML and identical browser hydration.
//! No backend/DB authority crosses into this module.
pub mod app;
pub mod bootstrap;
pub mod components;
pub mod document;
pub mod hydration;
pub mod navigation;
pub mod pages;

/// Percent-encode one path segment or query value, never an entire URL.
pub fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("writing to String");
        }
    }
    encoded
}

/// `%XX` escapes decoded once; bytes that are not valid escapes stay as they are.
pub fn decode_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
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
    #[test]
    fn encodes_segments_and_utf8_without_query_ambiguity() {
        assert_eq!(encode_component("a/b +é?"), "a%2Fb%20%2B%C3%A9%3F");
    }
}
