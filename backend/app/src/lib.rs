#![recursion_limit = "256"]
#![allow(
    clippy::result_large_err,
    reason = "Page handlers return the original response error by value."
)]

pub mod api_shared;

pub mod app;

pub mod artifact_metadata;

pub mod backups;

pub mod ci;

pub mod cluster;

pub mod code_scanning;

pub mod dashboard;

pub mod gists;

pub mod issues;

pub mod nuget;

pub mod organizations;

pub mod packages;

pub mod projects;

pub mod repositories;

pub mod vulnerabilities;

pub mod webhooks;

pub mod zip_archives;
