//! Site administration: the overview, users, site settings, email, the audit log, and the cluster
//! pages. Every page shares the section list and title from `nav`.
mod audit;
pub mod email;
pub mod nav;
pub mod nodes;
mod page;
mod settings;
pub mod storage;
#[cfg(test)]
mod tests;
mod users;

pub use audit::{admin_audit_page, AdminAudit, AuditEntry, AUDIT_ENTRIES_PER_PAGE};
pub use nav::{admin_layout, Section};
pub use page::{admin_page, Admin};
pub use settings::{admin_settings_page, AdminSettings};
pub use users::{admin_new_user_page, admin_users_page, AccountMetadata, AdminUsers, Signup};

/// Signup requests shown per page on the users page.
pub(crate) const SIGNUP_REQUESTS_PER_PAGE: usize = 30;
