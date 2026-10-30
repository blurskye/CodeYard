//! Git itself: libgit2 reads and hardened `git` commands against a repository
//! directory. Application code reaches a repository through
//! `repositories::store`, which is the only caller of the path-taking functions here.
pub mod ci;
mod comparison;
pub(crate) mod gists;
pub(crate) mod maintenance;
pub(crate) mod mirror;
pub(crate) mod oauth;
pub mod objects;
mod publications;
pub(crate) mod ref_names;
pub mod repository;
pub(crate) mod secret_scanning;
pub mod stats;

pub use ci::*;
pub(crate) use comparison::compare_commits;
pub use comparison::CommitComparison;
pub(crate) use publications::{
    prepare_at as prepare_branch_publications, read_at as read_branch_publication,
    PublicationUpdate,
};
pub use publications::{BranchPublication, PublicationActor, PublicationRecord};
pub(crate) use ref_names::{branch_ref, is_canonical_ref, is_full_commit_id, is_public_ref, tag_ref};
pub use repository::*;
pub use stats::*;
