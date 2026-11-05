#![recursion_limit = "256"]
#![allow(
    clippy::result_large_err,
    reason = "Page handlers return the original response error by value."
)]
//! Shared HTTP plumbing: config, request context, CSRF, errors, middleware, origin checks,
//! page rendering, sessions and static assets. Resource areas use these and own no routes here.

pub mod context;
pub mod credentials;
pub mod csrf;
pub mod error;
pub mod interaction_proofs;
pub mod middleware;
pub mod multipart;
pub mod origin;
pub mod range;
pub mod render;
pub mod session;
pub mod static_files;
pub mod traffic;

pub mod shared;

pub mod read_routing;
