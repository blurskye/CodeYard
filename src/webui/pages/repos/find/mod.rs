//! The go-to-file list: `/:owner/:repo/find/*`.
mod page;

pub use page::{find, FindPage, MAX_LISTED_FILES};
