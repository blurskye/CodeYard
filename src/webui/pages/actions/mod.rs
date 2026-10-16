//! Safe Actions presentation shared by SSR and hydration.
use crate::webui::pages::{notifications, repositories::RepoChrome};
use dioxus::prelude::*;
use runs::ActionsView;
use serde::{Deserialize, Serialize};

mod dispatch;
pub mod logs;
mod run;
mod runs;

pub use dispatch::{DispatchDraft, InputKind, WorkflowInput, TRIGGER_EVENTS};
pub use run::{Deployment, Run, SelectedRun};
pub use runs::{actions_page, matches_filters, query_url, workflow_name, ActionsPage, Workflow};

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
        } => dispatch::recovery(
            repo_chrome.as_deref(),
            &base_path,
            &csrf_token,
            &message,
            &draft,
            &workflows,
        ),
    }
}

#[cfg(test)]
mod tests;
