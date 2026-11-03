//! The Actions lane: the `/ci` page, and the pages rendered through the same `Page` enum.
mod page;

pub use page::{render, Page};
