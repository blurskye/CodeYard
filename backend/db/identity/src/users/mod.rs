pub mod accounts;
pub mod admission;
pub mod blocks;
pub mod emails;
pub mod gpg_keys;
pub mod profiles;
pub mod sessions;
pub mod signing_keys;
pub mod signup_requests;
pub mod social_accounts;
pub mod ssh_keys;
pub mod tokens;
pub mod transitions;
pub mod two_factor;

/// Names of the pages that sit directly under `/`. A user or organization called one of
/// these would have a profile and repositories that no link could reach.
const RESERVED_NAMES: &[&str] = &[
    "_forms",
    "admin",
    "api",
    "apps",
    "assets",
    "avatars",
    "ci",
    "demo",
    "emojis",
    "enter",
    "explore",
    "health",
    "gists",
    "help",
    "forgot-password",
    "issues",
    "login",
    "logout",
    "migration-archives",
    "new",
    "notifications",
    "orgs",
    "pulls",
    "ready",
    "register",
    "repos",
    "reset-password",
    "search",
    "settings",
    "setup",
    "stars",
    "twirp",
    "users",
    "v2",
    "verify-email",
    "wasm",
];

pub fn is_reserved_name(name: &str) -> bool {
    RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use crate::users::is_reserved_name;

    #[test]
    fn the_asset_and_avatar_addresses_are_reserved() {
        for name in [
            "assets",
            "avatars",
            "AVATARS",
            "wasm",
            "help",
            "HELP",
            "forgot-password",
            "reset-password",
            "verify-email",
            "apps",
            "users",
        ] {
            assert!(is_reserved_name(name), "{name}");
        }
        assert!(!is_reserved_name("alice"));
    }
}
