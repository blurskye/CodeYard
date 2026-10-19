//! Browser launch and hydration bootstrap wiring.
#[cfg(target_arch = "wasm32")]
use dioxus::prelude::*;

#[cfg(target_arch = "wasm32")]
pub fn launch() {
    if let Err(error) = launch_inner() {
        // Keep useful SSR intact if an incompatible/missing bundle arrives.
        web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(error));
    }
}

#[cfg(target_arch = "wasm32")]
fn launch_inner() -> Result<(), &'static str> {
    use super::bootstrap::{Bootstrap, BOOTSTRAP_ID, DIOXUS_VERSION, ROOT_ID};
    use wasm_bindgen::JsValue;
    // Before any field restores a draft: this document may answer a submit that left a draft behind.
    super::components::draft_text::settle_submitted();
    let window = web_sys::window().ok_or("The browser window is unavailable.")?;
    let document = window
        .document()
        .ok_or("The page document is unavailable.")?;
    let json = document
        .get_element_by_id(BOOTSTRAP_ID)
        .and_then(|node| node.text_content())
        .ok_or(
            "The server hydration bootstrap is missing; native links and forms remain available.",
        )?;
    let bootstrap: Bootstrap = serde_json::from_str(&json)
        .map_err(|_| "The server hydration state is invalid; reload this page.")?;
    if bootstrap.protocol != 1 || bootstrap.renderer != DIOXUS_VERSION {
        return Err("The browser/server renderer versions differ; reload this page.");
    }
    if document.get_element_by_id(ROOT_ID).is_none() {
        return Err("The SSR application root is missing.");
    }
    // Dioxus-web's generated hydration glue reads these exact framework globals.
    // Set them through Rust bindings, not handwritten JavaScript or eval.
    let globals: &JsValue = window.as_ref();
    js_sys::Reflect::set(
        globals,
        &"initial_dioxus_hydration_data".into(),
        &bootstrap.hydration.data.into(),
    )
    .map_err(|_| "Unable to initialize framework hydration data.")?;
    for (name, value) in [
        (
            "initial_dioxus_hydration_debug_types",
            bootstrap.hydration.debug_types,
        ),
        (
            "initial_dioxus_hydration_debug_locations",
            bootstrap.hydration.debug_locations,
        ),
    ] {
        let value =
            js_sys::JSON::parse(&value).map_err(|_| "Invalid framework hydration metadata.")?;
        js_sys::Reflect::set(globals, &name.into(), &value)
            .map_err(|_| "Unable to initialize framework hydration metadata.")?;
    }
    // Dioxus 0.7.10 registers a streaming callback even for complete SSR and
    // unconditionally reads hydrate_queue.length before hydrating the root.
    js_sys::Reflect::set(
        globals,
        &"hydrate_queue".into(),
        js_sys::Array::new().as_ref(),
    )
    .map_err(|_| "Unable to initialize framework hydration queue.")?;
    let dom = VirtualDom::new_with_props(
        super::app::App,
        super::app::AppProps {
            initial: bootstrap.initial,
        },
    );
    dioxus_web::launch::launch_virtual_dom(
        dom,
        dioxus_web::Config::new().hydrate(true).rootname(ROOT_ID),
    );
    Ok(())
}
