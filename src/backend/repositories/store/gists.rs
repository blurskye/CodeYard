use super::{ErrorKind, RepoError};
use crate::backend::db::users::accounts::AccountCredential;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_GIST_FILES: usize = 100;
pub const MAX_GIST_FILENAME_BYTES: usize = 255;
pub const MAX_GIST_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_GIST_REQUEST_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
pub enum GistCredential {
    Session(String),
    Token(i64, String),
}
impl std::fmt::Debug for GistCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GistCredential([redacted])")
    }
}
impl GistCredential {
    pub fn borrowed(&self) -> AccountCredential<'_> {
        match self {
            Self::Session(session) => AccountCredential::Session(session),
            Self::Token(id, hash) => AccountCredential::Token(*id, hash),
        }
    }
    pub fn from_credential(credential: &AccountCredential<'_>) -> Self {
        match credential {
            AccountCredential::Session(session) => Self::Session((*session).into()),
            AccountCredential::Token(id, hash) => Self::Token(*id, (*hash).into()),
        }
    }
}
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
    crate::backend::storage::valid_basename(name, MAX_GIST_FILENAME_BYTES)
        && !name.eq_ignore_ascii_case(".git")
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
