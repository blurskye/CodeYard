//! The file editor, delete and upload pages: `/:owner/:repo/new/*`, `edit/*`, `delete/*` and `upload/*`.
mod delete_file;
mod page;
mod upload;

pub use delete_file::{delete_file, DeleteFile, DELETE_REFUSED};
pub use page::{editor, Editor, SAVED_FOLLOWUP_FAILED};
pub use upload::{upload_page, UploadForm, UPLOAD_MESSAGE};
