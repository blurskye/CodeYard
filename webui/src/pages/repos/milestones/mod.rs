//! Milestone pages: `/:owner/:repo/milestones`, `/milestones/new` and `/milestones/:number`.
mod page;

pub use page::{detail, list, new};
