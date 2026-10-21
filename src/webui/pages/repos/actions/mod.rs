//! Repository Actions pages: the run list, a run's detail and logs, and the dispatch form.
mod dispatch;
pub mod logs;
mod polling;
mod run;
mod runs;
#[cfg(test)]
mod tests;

pub(in crate::webui::pages) use dispatch::recovery;
pub use dispatch::{DispatchDraft, InputKind, WorkflowInput, TRIGGER_EVENTS};
pub use polling::RunLogUpdate;
pub use run::{Deployment, PlannedJob, Run, RunProgress, SelectedRun};
pub(in crate::webui::pages::repos) use runs::run_row;
pub(in crate::webui::pages) use runs::ActionsView;
pub use runs::{actions_page, matches_filters, query_url, workflow_name, ActionsPage, Workflow};
