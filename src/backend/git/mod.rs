//! Git itself: libgit2 reads and hardened `git` commands against a repository
//! directory. Application code reaches a repository through
//! `repositories::store`, which is the only caller of the path-taking functions here.
pub mod ci;
pub(crate) mod maintenance;
pub(crate) mod ref_names;
pub mod repository;
pub mod stats;

pub use ci::*;
pub(crate) use ref_names::{branch_ref, is_canonical_ref, is_full_commit_id, tag_ref};
pub use repository::*;
pub use stats::*;
