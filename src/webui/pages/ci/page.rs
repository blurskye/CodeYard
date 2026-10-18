//! Safe Actions presentation shared by SSR and hydration. This is the Actions lane: the `/ci` page,
//! the notifications page, and the dispatch recovery page, all rendered through one `Page` enum.
use crate::webui::pages::{
    notifications,
    repos::{
        actions::{recovery, ActionsPage, ActionsView, DispatchDraft, Workflow},
        chrome::RepoChrome,
    },
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Page {
    Actions(Box<ActionsPage>),
    Notifications(notifications::NotificationsPage),
    DispatchRecovery {
        // Box the optional chrome to keep the recovery variant bounded.
        #[serde(default)]
        repo_chrome: Option<Box<RepoChrome>>,
        base_path: String,
        csrf_token: String,
        message: String,
        draft: Box<DispatchDraft>,
        /// The repository's workflows on the draft's branch, so the form can show their inputs.
        #[serde(default)]
        workflows: Vec<Workflow>,
    },
}

impl Page {
    /// The repository an Actions page is scoped to. Global CI pages have none.
    pub fn repo_chrome(&self) -> Option<&RepoChrome> {
        match self {
            Page::Actions(model) => model.repo_chrome.as_ref(),
            Page::DispatchRecovery { repo_chrome, .. } => repo_chrome.as_deref(),
            Page::Notifications(_) => None,
        }
    }
}

pub fn render(page: Page) -> Element {
    match page {
        Page::Actions(model) => {
            let model = *model;
            let key = model
                .selected
                .as_ref()
                .map(|selected| selected.run.id.clone())
                .unwrap_or_else(|| "list".into());
            rsx! { for key in [key] { ActionsView { key: "{key}", model: model.clone() } } }
        }
        Page::Notifications(model) => rsx! { notifications::NotificationsView { model } },
        Page::DispatchRecovery {
            repo_chrome,
            base_path,
            csrf_token,
            message,
            draft,
            workflows,
        } => recovery(
            repo_chrome.as_deref(),
            &base_path,
            &csrf_token,
            &message,
            &draft,
            &workflows,
        ),
    }
}
