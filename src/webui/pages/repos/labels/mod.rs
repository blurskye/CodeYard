//! The labels page, `/:owner/:repo/labels`. Everyone reads the labels here. An administrator adds,
//! changes and deletes them, and the forms post to the repository's form endpoint, which returns
//! to this page.
mod page;

pub use page::list;
