//! Pure helpers shared by the server and browser views.
pub mod command_codec;
pub mod file_search;
pub mod git_ids;
pub mod git_limits;
pub mod git_paths;
pub mod media;
pub mod repository_names;
pub mod website;

#[cfg(not(target_arch = "wasm32"))]
pub mod crypto;
