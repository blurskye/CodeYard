//! Dropdown menus. Every `details.native-menu` gets this behaviour once the page hydrates:
//! a click outside closes it, a link inside it closes it, Escape closes it and returns focus
//! to its button, and a click on another menu's button closes the open one. Without scripts
//! the native `details` still opens and closes.
use wasm_bindgen::{closure::Closure, JsCast};

const MENU: &str = "details.native-menu";
const OPEN_MENU: &str = "details.native-menu[open]";
const MENU_LINK: &str = "details.native-menu a";

/// Adds the document listeners. The app root lives for the whole visit, so the listeners
/// stay for the life of the page.
pub fn install() {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let on_click = Closure::<dyn FnMut(web_sys::Event)>::new(|event: web_sys::Event| {
        close_for_click(&event);
    });
    let on_key = Closure::<dyn FnMut(web_sys::Event)>::new(|event: web_sys::Event| {
        close_for_key(&event);
    });
    // Capture, so that other menus close before the clicked control runs its own handler.
    let _ = document.add_event_listener_with_callback_and_bool(
        "click",
        on_click.as_ref().unchecked_ref(),
        true,
    );
    let _ = document.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());
    on_click.forget();
    on_key.forget();
}

/// A click closes every open menu it is not inside. A link inside a menu also closes that
/// menu, because the link has made the choice.
fn close_for_click(event: &web_sys::Event) {
    let Some(target) = event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
    else {
        return;
    };
    let target_node: &web_sys::Node = &target;
    let chose_link = target.closest(MENU_LINK).ok().flatten().is_some();
    for menu in open_menus() {
        // A menu nested in another (Watch inside the tabs menu) keeps both open.
        if !menu.contains(Some(target_node)) || chose_link {
            close(&menu);
        }
    }
}

/// Escape closes the open menus and returns focus to the button of the menu that held it.
fn close_for_key(event: &web_sys::Event) {
    let Some(key) = event.dyn_ref::<web_sys::KeyboardEvent>() else {
        return;
    };
    // A control inside the menu, such as an open select, has already used this key.
    if key.key() != "Escape" || event.default_prevented() {
        return;
    }
    let menus = open_menus();
    if menus.is_empty() {
        return;
    }
    let focused_menu = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.active_element())
        .and_then(|element| element.closest(MENU).ok().flatten());
    for menu in &menus {
        close(menu);
    }
    // Every open menu is closed, so focus goes to the outermost one that held it: a nested
    // menu's own button would sit inside a closed menu.
    let outermost = focused_menu.and_then(|inner| {
        let inner: &web_sys::Node = &inner;
        menus
            .iter()
            .find(|menu| menu.contains(Some(inner)))
            .cloned()
    });
    let summary = outermost.and_then(|menu| menu.query_selector("summary").ok().flatten());
    if let Some(button) =
        summary.and_then(|summary| summary.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = button.focus();
    }
}

fn open_menus() -> Vec<web_sys::Element> {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return Vec::new();
    };
    let Ok(open) = document.query_selector_all(OPEN_MENU) else {
        return Vec::new();
    };
    (0..open.length())
        .filter_map(|index| open.item(index)?.dyn_into::<web_sys::Element>().ok())
        .collect()
}

fn close(menu: &web_sys::Element) {
    let _ = menu.remove_attribute("open");
}
