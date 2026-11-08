#[derive(Debug, thiserror::Error)]
#[error("A live unrestricted account credential is required")]
pub struct AccountRestricted;

/// Generated noreply identities contain case-sensitive account names.
pub fn email_matches(address: &str, candidate: &str) -> bool {
    if address.ends_with("@users.noreply") {
        address == candidate
    } else {
        address.eq_ignore_ascii_case(candidate)
    }
}
