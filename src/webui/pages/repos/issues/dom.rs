//! The browser calls the list widgets need: the caret and scroll of the search box, and focus,
//! scrolling and clicks for the suggestion and filter lists. The server never runs them, so each
//! has an empty version for the server build.

#[cfg(target_arch = "wasm32")]
fn element_by_id(id: &str) -> Option<web_sys::Element> {
    web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
}

#[cfg(target_arch = "wasm32")]
fn input_by_id(id: &str) -> Option<web_sys::HtmlInputElement> {
    use wasm_bindgen::JsCast;
    element_by_id(id).and_then(|element| element.dyn_into::<web_sys::HtmlInputElement>().ok())
}

/// The caret of a text field, in UTF-16 units. None when the browser gives no position.
#[cfg(target_arch = "wasm32")]
pub(super) fn input_caret(id: &str) -> Option<usize> {
    input_by_id(id)?
        .selection_start()
        .ok()
        .flatten()
        .map(|position| position as usize)
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn input_caret(_id: &str) -> Option<usize> {
    None
}

/// Places the caret of a text field, in UTF-16 units.
#[cfg(target_arch = "wasm32")]
pub(super) fn set_input_caret(id: &str, position: usize) {
    if let Some(input) = input_by_id(id) {
        let position = position as u32;
        let _ = input.set_selection_range(position, position);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn set_input_caret(_id: &str, _position: usize) {}

/// How far a text field is scrolled, in pixels.
#[cfg(target_arch = "wasm32")]
pub(super) fn input_scroll(id: &str) -> i32 {
    input_by_id(id).map_or(0, |input| input.scroll_left())
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn input_scroll(_id: &str) -> i32 {
    0
}

/// Moves focus to an element, when it can take focus. A hidden element keeps its focus.
#[cfg(target_arch = "wasm32")]
pub(super) fn focus(id: &str) {
    use wasm_bindgen::JsCast;
    let element =
        element_by_id(id).and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok());
    if let Some(element) = element {
        let _ = element.focus();
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn focus(_id: &str) {}

/// Scrolls an element into view inside its list.
#[cfg(target_arch = "wasm32")]
pub(super) fn scroll_into_view(id: &str) {
    if let Some(element) = element_by_id(id) {
        element.scroll_into_view_with_bool(false);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn scroll_into_view(_id: &str) {}

/// Clicks the first link inside an element, so the link's own navigation runs.
#[cfg(target_arch = "wasm32")]
pub(super) fn click_first_link(id: &str) {
    use wasm_bindgen::JsCast;
    let link = element_by_id(id)
        .and_then(|element| element.query_selector("a").ok().flatten())
        .and_then(|link| link.dyn_into::<web_sys::HtmlElement>().ok());
    if let Some(link) = link {
        link.click();
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn click_first_link(_id: &str) {}
