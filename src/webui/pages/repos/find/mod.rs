//! The go-to-file list: `/:owner/:repo/find/*`.
mod page;

pub use page::{find, path_matches, FindPage, MAX_LISTED_FILES};
