//! The repository settings page and the forms on it.
pub mod environments;
mod page;
pub mod properties;
pub mod secrets;

pub use page::{
    rejected_form, settings, settings_section, settings_tab, split_status_checks, BranchRule,
    Collaborator, PendingInvitation, RepoActions, RuleDraft, Settings,
};
