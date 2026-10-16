//! Node service selection and owned serving lifecycle. Desired roles do not
//! confer enrollment, repository authority or engine privileges.
pub mod config;
mod roles;
mod runtime;

pub use config::ServeConfig;
pub use roles::{NodeRole, NodeRoles};
pub use runtime::{serve, serve_worker};
