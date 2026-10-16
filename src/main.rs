#[cfg(target_arch = "wasm32")]
fn main() {
    codeyard::webui::hydration::launch();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    codeyard::backend::cli::run()
}
