//! Codeyard library crate
//!
//! A minimal self-hosted Git server with SSH and HTTP support.

pub mod api;
pub mod app;
pub mod ci;
pub mod cli;
pub mod cluster;
pub mod crypto;
pub mod dashboard;
pub mod db;
pub mod files;
pub mod git;
pub mod identity;
pub mod issues;
pub mod node;
pub mod notifications;
pub mod repositories;
pub mod ssh;
pub mod storage;
