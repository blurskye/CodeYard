//! Releases: the list, a release's page and the form that publishes or edits one.
mod page;

pub use page::{
    releases_tags_nav, render, sidebar, AssetItem, Page, ReleaseDraft, ReleaseItem, ReleasesSummary,
};
