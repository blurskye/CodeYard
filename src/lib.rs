//! One application: native backend authority and shared Dioxus SSR/hydration views.
// The git hook task future holds the application state, which reaches the network library's
// types; checking that it is `Send` needs more than the default 128 levels.
#![recursion_limit = "256"]
// A page handler's error is the response it returns, which is sent once per request, so the size
// of the `Err` variant costs nothing worth boxing 100+ signatures for.
#![allow(clippy::result_large_err)]
#[cfg(not(target_arch = "wasm32"))]
pub mod backend;
pub mod webui;

#[cfg(all(target_arch = "wasm32", not(feature = "web")))]
compile_error!("Build the browser with --no-default-features --features web.");
