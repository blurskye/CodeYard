use codeyard_webui::pages::repos::issues as view;
/// The page model, boxed: it is much larger than most pages.
pub fn page(model: view::Page) -> codeyard_webui::app::Page {
    codeyard_webui::app::Page::Collaboration(Box::new(model))
}
