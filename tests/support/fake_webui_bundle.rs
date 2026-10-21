//! A tiny stand-in for the generated browser bundle (`WEBUI_DIST`), so a test can
//! render full HTML pages without building the WASM application.
use std::{path::Path, sync::Once};

/// Points `WEBUI_DIST` at a small bundle under the target directory. Call it before the first
/// page is rendered: the server reads the variable once per process.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("webui-bundle");
        std::fs::create_dir_all(root.join("assets")).expect("bundle assets directory");
        write(&root.join("assets/app.js"), "// test bundle\n");
        write(
            &root.join("index.html"),
            "<!doctype html><script type=\"module\" src=\"/assets/app.js\"></script>",
        );
        std::env::set_var("WEBUI_DIST", &root);
    });
}

/// Test processes install the bundle at the same time, so each file is renamed into place whole.
fn write(path: &Path, content: &str) {
    let staging = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&staging, content).expect("bundle file");
    std::fs::rename(&staging, path).expect("bundle file");
}
