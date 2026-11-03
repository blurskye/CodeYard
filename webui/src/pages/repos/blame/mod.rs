//! The blame view: `/:owner/:repo/blame/*`.
mod page;

pub use page::{blame, BlameHunk, BlamePage};
