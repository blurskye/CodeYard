pub mod accounts;
pub mod admission;
pub mod sessions;
pub mod signup_requests;
pub mod ssh_keys;
pub mod tokens;
pub mod transitions;

pub use accounts::*;
pub use sessions::*;
pub use signup_requests::*;
pub use ssh_keys::*;

/// Names of the pages that sit directly under `/`. A user or organization called one of
/// these would have a profile and repositories that no link could reach.
const RESERVED_NAMES: &[&str] = &[
    "_forms",
    "admin",
    "api",
    "assets",
    "avatars",
    "ci",
    "demo",
    "enter",
    "explore",
    "health",
    "login",
    "logout",
    "new",
    "notifications",
    "orgs",
    "ready",
    "register",
    "search",
    "settings",
    "setup",
    "wasm",
];

pub fn is_reserved_name(name: &str) -> bool {
    RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::is_reserved_name;

    #[test]
    fn the_asset_and_avatar_addresses_are_reserved() {
        for name in ["assets", "avatars", "AVATARS", "wasm"] {
            assert!(is_reserved_name(name), "{name}");
        }
        assert!(!is_reserved_name("alice"));
    }
}
