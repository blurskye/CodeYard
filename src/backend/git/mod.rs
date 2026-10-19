//! Git itself: libgit2 reads and hardened `git` commands against a repository
//! directory. Application code reaches a repository through
//! `repositories::store`, which is the only caller of the path-taking functions here.
pub mod ci;
pub mod commit_activity;
pub(crate) mod maintenance;
pub(crate) mod ref_names;
pub mod repository;
pub mod stats;

pub use ci::*;
pub use commit_activity::*;
pub(crate) use ref_names::{branch_ref, is_canonical_ref, tag_ref};
pub use repository::*;
pub use stats::*;
