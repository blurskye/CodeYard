//! Pure helpers shared by the server and browser views.

#[cfg(not(target_arch = "wasm32"))]
pub mod locks;

#[cfg(not(target_arch = "wasm32"))]
pub mod crypto;

#[cfg(not(target_arch = "wasm32"))]
pub mod secrets;

#[cfg(not(target_arch = "wasm32"))]
pub mod fs;

#[cfg(not(target_arch = "wasm32"))]
pub mod node_identity;

#[cfg(not(target_arch = "wasm32"))]
pub mod token_hash;

#[cfg(not(target_arch = "wasm32"))]
pub mod secret_detection;

#[cfg(unix)]
pub mod outbound;

#[cfg(not(target_arch = "wasm32"))]
pub mod http_credentials;
