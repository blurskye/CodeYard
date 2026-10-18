//! The signed-in user's own work: the issue and pull request lists under `/issues` and `/pulls`.
mod page;

pub use page::{choose_repository, render, RepositoryChoice, WorkItem, WorkList};
