//! Git itself: libgit2 reads and hardened `git` commands against a repository
//! directory. Application code reaches a repository through
//! `repositories::store`, which is the only caller of the path-taking functions here.
pub mod ci;
mod comparison;
pub mod gists;
pub mod maintenance;
pub mod mirror;
pub mod oauth;
pub mod objects;
mod publications;
pub mod ref_names;
pub mod repository;
pub mod secret_scanning;
pub mod stats;

pub use ci::*;
pub use comparison::compare_commits;
pub use comparison::CommitComparison;
pub use publications::{
    plan_at as plan_branch_publications, prepare_at as prepare_branch_publications,
    read_at as read_branch_publication, PublicationUpdate,
};
pub use publications::{BranchPublication, PublicationActor, PublicationRecord};
pub use ref_names::{branch_ref, is_canonical_ref, is_full_commit_id, is_public_ref, tag_ref};
pub use repository::*;
pub use stats::*;

pub mod error;
pub mod gist_types;

pub mod secret_detection;
