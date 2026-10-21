//! The file view: `/:owner/:repo/blob/*`.
mod page;

pub(in crate::webui::pages::repos) use page::{
    add_file_menu, can_commit_on, file_downloads, file_layout, file_stats, more_menu, outline_menu,
    title_row, view_switch, FileView,
};
pub use page::{blob, BlobPage, FilesPanel, FolderListing, GoToFile};
#[cfg(test)]
pub(in crate::webui::pages::repos) use page::{display_text, matching_entries};
