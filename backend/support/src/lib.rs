//! Pure helpers shared by the server and browser views.
pub mod command_codec;
pub mod file_search;
pub mod git_ids;
pub mod git_limits;
pub mod git_paths;
pub mod media;
pub mod repository_names;
pub mod website;

pub mod credentials;
pub mod file_names;
#[cfg(not(target_arch = "wasm32"))]
pub mod locks;
pub mod tokens;

#[cfg(not(target_arch = "wasm32"))]
pub mod crypto;

#[cfg(not(target_arch = "wasm32"))]
pub mod secrets;

#[cfg(not(target_arch = "wasm32"))]
pub mod fs;

#[cfg(not(target_arch = "wasm32"))]
pub mod ssh_mode;

#[cfg(not(target_arch = "wasm32"))]
pub mod node_identity;

#[cfg(not(target_arch = "wasm32"))]
pub mod object_limits;

#[cfg(not(target_arch = "wasm32"))]
pub mod token_hash;

#[cfg(not(target_arch = "wasm32"))]
pub mod secret_detection;

#[cfg(unix)]
pub mod outbound;

pub mod zip_archives;
