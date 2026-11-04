//! Shared HTTP plumbing: config, request context, CSRF, errors, middleware, origin checks,
//! page rendering, sessions and static assets. Resource areas use these and own no routes here.
pub(crate) mod context;
pub(crate) mod credentials;
pub(crate) mod csrf;
pub(crate) mod error;
pub(crate) mod interaction_proofs;
pub(crate) mod middleware;
pub(crate) mod multipart;
pub(crate) mod origin;
pub(crate) mod range;
pub(crate) mod render;
pub(crate) mod session;
pub(crate) mod static_files;
pub(crate) mod traffic;
