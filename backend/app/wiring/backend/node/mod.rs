//! Node service selection and owned serving lifecycle. Desired roles do not
//! confer enrollment, repository authority or engine privileges.

pub mod config;

pub(crate) mod router;
pub(crate) mod runtime;
