//! A tiny stand-in for the generated browser bundle (`WEBUI_DIST`), so a test can
//! render full HTML pages without building the WASM application.
// Each integration crate uses a different subset of these shared test fixtures.
#![allow(dead_code)]
use std::sync::Once;

/// Points `WEBUI_DIST` at a throwaway bundle. Call it before the first page is
/// rendered: the server reads the variable once per process.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let root = tempfile::tempdir().expect("bundle directory").keep();
        std::fs::create_dir_all(root.join("assets")).expect("bundle assets directory");
        std::fs::write(root.join("assets/app.js"), "// test bundle\n").expect("bundle module");
        std::fs::write(
            root.join("index.html"),
            "<!doctype html><script type=\"module\" src=\"/assets/app.js\"></script>",
        )
        .expect("bundle index");
        std::env::set_var("WEBUI_DIST", &root);
    });
}
