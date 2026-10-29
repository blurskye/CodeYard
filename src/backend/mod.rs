//! Codeyard library crate
//!
//! A minimal self-hosted Git server with SSH and HTTP support.

pub mod api;
pub mod app;
pub mod artifact_metadata;
pub mod backups;
pub mod ci;
pub mod cli;
pub mod cluster;
pub mod code_scanning;
pub mod crypto;
pub mod dashboard;
pub mod db;
pub mod dependency_graph;
pub mod email;
pub mod files;
pub mod gists;
pub mod git;
pub mod identity;
pub mod issues;
pub mod node;
pub mod notifications;
pub mod organizations;
pub mod packages;
pub mod projects;
pub mod repositories;
pub mod ssh;
pub mod storage;
pub(crate) mod fs;
pub mod vulnerabilities;
pub mod webhooks;
pub mod zip_archives;
