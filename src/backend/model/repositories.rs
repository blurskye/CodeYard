use super::identity::SshKeyWriteRefusal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Package {
    pub manifest: String,
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    pub package_url: String,
    pub scope: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DeployKeyError {
    #[error("Repository admin access is required")]
    Denied,
    #[error("Enter a title of up to 255 characters")]
    InvalidTitle,
    #[error("Deploy key not found")]
    NotFound,
    #[error(transparent)]
    Key(#[from] SshKeyWriteRefusal),
    #[error("Deploy key storage is unavailable")]
    Unavailable(anyhow::Error),
}

pub const MAX_REVIEWERS: usize = 5000;
