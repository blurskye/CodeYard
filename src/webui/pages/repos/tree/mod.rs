//! The repository root and file tree: `/:owner/:repo` and `/:owner/:repo/tree/*`.
mod page;

pub(in crate::webui::pages::repos) use page::{breadcrumbs, sort_entries};
pub use page::{tree, TreeEntry, TreePage, MAX_README_BYTES};
