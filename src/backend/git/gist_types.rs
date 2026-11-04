use super::error::{ErrorKind, RepoError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_GIST_FILES: usize = 100;
pub const MAX_GIST_FILENAME_BYTES: usize = 255;
pub const MAX_GIST_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_GIST_REQUEST_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GistFileChange {
    pub filename: Option<String>,
    pub content: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GistEdit {
    pub description: Option<String>,
    #[serde(default)]
    pub files: BTreeMap<String, Option<GistFileChange>>,
    pub expected: Option<String>,
}
impl GistEdit {
    pub fn validate(&self) -> Result<(), RepoError> {
        if (self.description.is_none() && self.files.is_empty())
            || self.description.as_ref().is_some_and(|description| {
                description.chars().count() > 65535 || description.contains('\0')
            })
            || self.files.len() > MAX_GIST_FILES * 2
        {
            return Err(invalid());
        }
        let mut bytes = 0usize;
        for (name, change) in &self.files {
            if !valid_gist_filename(name) {
                return Err(invalid());
            }
            if let Some(change) = change {
                if change
                    .filename
                    .as_ref()
                    .is_some_and(|name| !valid_gist_filename(name))
                {
                    return Err(invalid());
                }
                bytes = bytes.saturating_add(change.content.as_ref().map_or(0, String::len));
            }
        }
        if bytes > MAX_GIST_BYTES {
            return Err(RepoError::response_too_large());
        }
        Ok(())
    }
}
pub fn valid_gist_filename(name: &str) -> bool {
    codeyard_support::file_names::valid_basename(name, MAX_GIST_FILENAME_BYTES)
        && !codeyard_support::git_paths::is_git_dir_name(name)
}
fn invalid() -> RepoError {
    RepoError::new(ErrorKind::Invalid, "Invalid gist files or description")
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GistSnapshot {
    pub sha: String,
    pub files: Vec<crate::backend::git::objects::TreeObjectEntry>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GistRevision {
    pub sha: String,
    pub author_email: String,
    pub timestamp: i64,
    pub additions: i32,
    pub deletions: i32,
}
#[derive(Clone, Debug)]
pub(crate) struct PreparedGist {
    pub base: Option<String>,
    pub sha: String,
}
