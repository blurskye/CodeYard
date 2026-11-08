//! Event names a webhook can subscribe to. The names are GitHub's, so receivers that
//! already branch on them keep working. The form's EVENT_OPTIONS is the list a hook can
//! subscribe to.
/// Every event a hook can subscribe to, with its label, in display order. Subscriptions are
/// checked against this list, so the form always shows each stored event.
pub const EVENT_OPTIONS: [(&str, &str); 25] = [
    ("repository", "Repository changes"),
    ("push", "Pushes"),
    ("create", "Branch or tag created"),
    ("delete", "Branch or tag deleted"),
    ("fork", "Forks"),
    ("star", "Stars"),
    ("release", "Releases"),
    ("deployment", "Deployments"),
    ("deployment_status", "Deployment statuses"),
    ("deployment_protection_rule", "Deployment protection rules"),
    ("issues", "Issues"),
    ("issue_comment", "Issue comments"),
    ("commit_comment", "Commit comments"),
    ("code_scanning_alert", "Code scanning alerts"),
    ("secret_scanning_alert", "Secret scanning alerts"),
    (
        "secret_scanning_alert_location",
        "Secret scanning alert locations",
    ),
    ("pull_request", "Pull requests"),
    ("pull_request_review", "Pull request reviews"),
    (
        "pull_request_review_comment",
        "Pull request review comments",
    ),
    ("check_run", "Check runs"),
    ("check_suite", "Check suites"),
    ("status", "Commit statuses"),
    ("repository_dispatch", "Repository dispatches"),
    ("workflow_run", "Workflow runs"),
    ("workflow_job", "Workflow jobs"),
];

pub const REPOSITORY: &str = "repository";
pub const PUSH: &str = "push";
pub const CREATE: &str = "create";
pub const DELETE: &str = "delete";
pub const FORK: &str = "fork";
pub const STAR: &str = "star";
pub const RELEASE: &str = "release";
pub const DEPLOYMENT: &str = "deployment";
pub const DEPLOYMENT_STATUS: &str = "deployment_status";
pub const DEPLOYMENT_PROTECTION_RULE: &str = "deployment_protection_rule";
pub const ISSUES: &str = "issues";
pub const ISSUE_COMMENT: &str = "issue_comment";
pub const COMMIT_COMMENT: &str = "commit_comment";
pub const CODE_SCANNING_ALERT: &str = "code_scanning_alert";
pub const SECRET_SCANNING_ALERT: &str = "secret_scanning_alert";
pub const SECRET_SCANNING_ALERT_LOCATION: &str = "secret_scanning_alert_location";
pub const PULL_REQUEST: &str = "pull_request";
pub const PULL_REQUEST_REVIEW: &str = "pull_request_review";
pub const PULL_REQUEST_REVIEW_COMMENT: &str = "pull_request_review_comment";
pub const CHECK_RUN: &str = "check_run";
pub const CHECK_SUITE: &str = "check_suite";
pub const STATUS: &str = "status";
pub const REPOSITORY_DISPATCH: &str = "repository_dispatch";
pub const WORKFLOW_RUN: &str = "workflow_run";
pub const WORKFLOW_JOB: &str = "workflow_job";
/// Sent when a hook is created or tested. Not a subscription: every hook gets it.
pub const PING: &str = "ping";

/// The stored subscription for "send me everything".
pub const EVERYTHING: &str = "*";

/// Turns the checked events of a form into the stored subscription. "Everything" wins over
/// any other check, and a hook must subscribe to at least one event.
pub fn subscription(chosen: &[String]) -> Result<String, &'static str> {
    if chosen.iter().any(|name| name == EVERYTHING) {
        return Ok(EVERYTHING.to_owned());
    }
    let mut names: Vec<&str> = EVENT_OPTIONS
        .iter()
        .map(|(name, _)| *name)
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

/// Whether two stored subscriptions share an event.
pub fn overlap(first: &str, second: &str) -> bool {
    second == EVERYTHING || second.split(',').any(|event| subscribes(first, event))
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
    fn every_event_can_be_subscribed() {
        for event in [
            REPOSITORY,
            PUSH,
            CREATE,
            DELETE,
            FORK,
            STAR,
            RELEASE,
            DEPLOYMENT,
            DEPLOYMENT_STATUS,
            DEPLOYMENT_PROTECTION_RULE,
            ISSUES,
            ISSUE_COMMENT,
            COMMIT_COMMENT,
            CODE_SCANNING_ALERT,
            SECRET_SCANNING_ALERT,
            SECRET_SCANNING_ALERT_LOCATION,
            PULL_REQUEST,
            PULL_REQUEST_REVIEW,
            PULL_REQUEST_REVIEW_COMMENT,
            CHECK_RUN,
            CHECK_SUITE,
            STATUS,
            REPOSITORY_DISPATCH,
            WORKFLOW_RUN,
            WORKFLOW_JOB,
        ] {
            assert_eq!(subscription(&picked(&[event])).as_deref(), Ok(event));
        }
    }

    #[test]
    fn filter_matches_listed_events_and_everything() {
        assert!(subscribes("push,release", "release"));
        assert!(!subscribes("push,release", "issues"));
        assert!(!subscribes("push", "pushed"));
        assert!(subscribes("*", "issues"));
    }
}
