//! One application: native backend authority and shared Dioxus SSR/hydration views.
// The git hook task future holds the application state, which reaches the network library's
// types; checking that it is `Send` needs more than the default 128 levels.
#![recursion_limit = "256"]
#![allow(
    clippy::result_large_err,
    reason = "A page handler's error is the response it returns, sent once per request, so its size is not worth boxing 100+ signatures for."
)]
#[cfg(not(target_arch = "wasm32"))]
pub mod backend;

#[cfg(all(target_arch = "wasm32", not(feature = "web")))]
compile_error!("Build the browser with --no-default-features --features web.");
