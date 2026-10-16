//! The same safe owned presentation tree is rendered on the server and hydrated in the browser.
use super::{
    components,
    document::Chrome,
    navigation::{page_history, PageRoute},
    pages,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "lane", content = "model")]
pub enum Page {
    Identity(pages::identity::Page),
    Organizations(pages::organizations::Page),
    OrganizationProfile(pages::organizations::OrganizationProfile),
    OrganizationTeam(pages::organizations::TeamPage),
    Discovery(pages::discovery::Page),
    Repositories(pages::repositories::Page),
    Collaboration(Box<pages::collaboration::Page>),
    Actions(pages::actions::Page),
    Nodes(pages::nodes::Page),
    Storage(pages::storage::Page),
    /// `repo` is set only when the viewer may read that repository.
    Error {
        status: u16,
        message: String,
        repo: Option<Box<pages::repositories::RepoChrome>>,
        signed_in: bool,
    },
}

/// The browser tab title: the page's title and the site name, unless the title has it already.
pub fn tab_title(title: &str) -> String {
    if title.ends_with("Codeyard") {
        title.to_owned()
    } else {
        format!("{title} · Codeyard")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageEnvelope {
    pub title: String,
    pub location: String,
    pub chrome: Chrome,
    pub page: Page,
}

#[derive(Clone, Copy)]
pub(crate) struct CurrentPage(pub Signal<PageEnvelope>);
#[derive(Clone, Copy)]
pub(crate) struct RefreshEpoch(pub Signal<u64>);
#[derive(Clone, Copy)]
pub(crate) struct AppearanceEpoch(pub Signal<u64>);

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn theme_for_read(
    read_epoch: u64,
    committed_epoch: u64,
    incoming: super::document::Theme,
    committed: super::document::Theme,
) -> super::document::Theme {
    if read_epoch != committed_epoch {
        committed
    } else {
        incoming
    }
}

#[component]
pub fn App(initial: PageEnvelope) -> Element {
    let initial_path = initial.location.clone();
    let current = use_signal(move || initial);
    let refresh = use_signal(|| 0_u64);
    let appearance = use_signal(|| 0_u64);
    use_context_provider(|| CurrentPage(current));
    use_context_provider(|| RefreshEpoch(refresh));
    use_context_provider(|| AppearanceEpoch(appearance));
    #[cfg(target_arch = "wasm32")]
    use_effect(move || {
        // Framework effects run after mounting the hydrated tree, not on a timer
        // or merely when the WASM starts. Behavior still needs independent tests.
        if let Some(root) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id(super::bootstrap::ROOT_ID))
        {
            let _ = root.set_attribute("data-hydrated", "true");
        }
    });
    rsx! {
        dioxus_router::components::HistoryProvider { history: move |_| page_history(&initial_path),
            dioxus_router::Router::<PageRoute> {}
        }
    }
}

pub(crate) fn render_body(page: Page) -> Element {
    match page {
        Page::Identity(page) => pages::identity::render(page),
        Page::Organizations(page) => pages::organizations::render(page),
        Page::OrganizationProfile(page) => pages::organizations::profile_page(page),
        Page::OrganizationTeam(page) => pages::organizations::team_page(page),
        Page::Discovery(page) => pages::discovery::render(page),
        Page::Repositories(page) => pages::repositories::render(page),
        Page::Collaboration(page) => pages::collaboration::render(*page),
        Page::Actions(page) => pages::actions::render(page),
        Page::Nodes(page) => pages::nodes::render(page),
        Page::Storage(page) => pages::storage::render(page),
        Page::Error {
            status,
            message,
            repo,
            signed_in,
        } => pages::error::render(status, &message, repo.as_deref(), signed_in),
    }
}

#[component]
fn PageBody(page: Page) -> Element {
    render_body(page)
}

pub(crate) fn page_tree(envelope: PageEnvelope) -> Element {
    // Chrome-only updates (notably theme) may not re-render a native editor or
    // advance its hidden revision snapshot. Equal Page props memoize this body.
    // A genuinely different URL resets scoped drafts/tasks; same-URL log polling
    // retains component state, whose controls still need proper local handlers.
    let location = &envelope.location;
    let body = rsx! { PageBody { key: "{location}", page: envelope.page } };
    components::shell(envelope.chrome, body)
}

#[cfg(test)]
mod tests {
    use super::super::document::Theme;
    use super::*;

    #[test]
    fn old_page_read_cannot_undo_a_committed_appearance() {
        assert_eq!(theme_for_read(2, 3, Theme::Light, Theme::Dark), Theme::Dark);
        assert_eq!(
            theme_for_read(3, 3, Theme::Light, Theme::Dark),
            Theme::Light
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn shared_page_boundary_preserves_same_location_but_resets_changed_location() {
        use pages::repositories::{Editor, Page as RepositoryPage, RepoChrome};
        use std::{cell::RefCell, rc::Rc};
        #[derive(Clone)]
        struct Harness {
            initial: PageEnvelope,
            control: Rc<RefCell<Option<Signal<PageEnvelope>>>>,
        }
        fn root(props: Harness) -> Element {
            let current = use_signal(|| props.initial.clone());
            *props.control.borrow_mut() = Some(current);
            page_tree(current())
        }
        let initial = PageEnvelope {
            title: "Edit".into(),
            location: "/alice/project/edit/main/file.md".into(),
            chrome: Chrome::default(),
            page: Page::Repositories(RepositoryPage::Editor {
                repo: RepoChrome {
                    owner: "alice".into(),
                    name: "project".into(),
                    can_write: true,
                    ..Default::default()
                },
                editor: Editor {
                    branch: "main".into(),
                    path: "file.md".into(),
                    content: "original draft".into(),
                    expected_revision: "original-revision".into(),
                    ..Default::default()
                },
            }),
        };
        let control = Rc::new(RefCell::new(None));
        let mut dom = VirtualDom::new_with_props(
            root,
            Harness {
                initial: initial.clone(),
                control: control.clone(),
            },
        );
        dom.rebuild_in_place();
        let mut current = control.borrow().unwrap();
        let mut next = initial;
        next.chrome.theme = Theme::Dark;
        if let Page::Repositories(RepositoryPage::Editor { editor, .. }) = &mut next.page {
            editor.content = "new authorized content".into();
            editor.expected_revision = "new-revision".into();
        }
        dom.in_runtime(|| current.set(next.clone()));
        dom.render_immediate_to_vec();
        let retained = dioxus_ssr::render(&dom);
        assert!(retained.contains("original draft"));
        assert!(retained.contains("original-revision"));
        assert!(!retained.contains("new-revision"));
        // Only URL identity changes: the repository editor's own key is identical.
        next.location.push_str("?preview=1");
        dom.in_runtime(|| current.set(next));
        dom.render_immediate_to_vec();
        let reset = dioxus_ssr::render(&dom);
        assert!(reset.contains("new authorized content"));
        assert!(reset.contains("new-revision"));
        assert!(!reset.contains("original-revision"));
    }
}
