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

pub mod dashboard;
pub mod db;

pub mod email;
pub mod files;

pub mod gists;
pub mod identity;
pub mod issues;
pub mod node;
pub mod notifications;
pub(crate) mod npm;
pub(crate) mod nuget;
pub mod organizations;
pub mod packages;
pub mod projects;
pub mod repositories;
pub mod ssh;
pub mod storage;
pub mod vulnerabilities;
pub mod webhooks;
pub mod zip_archives;
