//! The branch and tag lists: `/:owner/:repo/branches` and `/:owner/:repo/tags`.
mod page;

pub use page::{refs, RefRow};
