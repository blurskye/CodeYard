//! Event names a webhook can subscribe to. The names are GitHub's, so receivers that
//! already branch on them keep working.

pub const PUSH: &str = "push";
pub const CREATE: &str = "create";
pub const DELETE: &str = "delete";
pub const FORK: &str = "fork";
pub const STAR: &str = "star";
pub const RELEASE: &str = "release";
pub const ISSUES: &str = "issues";
pub const ISSUE_COMMENT: &str = "issue_comment";
pub const PULL_REQUEST: &str = "pull_request";
pub const PULL_REQUEST_REVIEW: &str = "pull_request_review";
pub const PULL_REQUEST_REVIEW_COMMENT: &str = "pull_request_review_comment";
/// Sent when a hook is created or tested. Not a subscription: every hook gets it.
pub const PING: &str = "ping";

/// The events a form offers, in display order.
pub const SELECTABLE: [&str; 11] = [
    PUSH,
    CREATE,
    DELETE,
    FORK,
    STAR,
    RELEASE,
    ISSUES,
    ISSUE_COMMENT,
    PULL_REQUEST,
    PULL_REQUEST_REVIEW,
    PULL_REQUEST_REVIEW_COMMENT,
];

/// The stored subscription for "send me everything".
pub const EVERYTHING: &str = "*";

/// Turns the checked events of a form into the stored subscription. "Everything" wins over
/// any other check, and a hook must subscribe to at least one event.
pub fn subscription(chosen: &[String]) -> Result<String, &'static str> {
    if chosen.iter().any(|name| name == EVERYTHING) {
        return Ok(EVERYTHING.to_owned());
    }
    let mut names: Vec<&str> = SELECTABLE
        .iter()
        .copied()
        .filter(|name| chosen.iter().any(|picked| picked == name))
        .collect();
    if names.is_empty() {
        return Err("Choose at least one event");
    }
    if names.len() != chosen.len() {
        return Err("Choose only events from the list");
    }
    names.sort_unstable();
    Ok(names.join(","))
}

/// Whether a stored subscription delivers `event`.
pub fn subscribes(stored: &str, event: &str) -> bool {
    stored == EVERYTHING || stored.split(',').any(|name| name == event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picked(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn subscription_is_sorted_and_checked_against_the_list() {
        assert_eq!(
            subscription(&picked(&["release", "push"])).as_deref(),
            Ok("push,release")
        );
        assert_eq!(subscription(&picked(&[])), Err("Choose at least one event"));
        assert_eq!(
            subscription(&picked(&["push", "ping"])),
            Err("Choose only events from the list")
        );
        assert_eq!(subscription(&picked(&["push", "*"])), Ok("*".to_owned()));
    }

    #[test]
    fn filter_matches_listed_events_and_everything() {
        assert!(subscribes("push,release", "release"));
        assert!(!subscribes("push,release", "issues"));
        assert!(!subscribes("push", "pushed"));
        assert!(subscribes("*", "issues"));
    }
}
