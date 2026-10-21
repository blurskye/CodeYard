//! The reasons the notification list filters by. A reason has a name for the URL and a label
//! for the page, and covers the stored kinds that the notifications were created with.

/// Reason name, label and the stored kinds it covers, in menu order.
pub const REASONS: [(&str, &str, &[&str]); 13] = [
    ("mention", "Mentioned", &["mention"]),
    ("assigned", "Assigned", &["assigned"]),
    (
        "review_requested",
        "Review requested",
        &["pull_request_review_requested"],
    ),
    ("review", "Reviews", &["pull_request_review"]),
    (
        "comment",
        "Comments",
        &[
            "issue_comment",
            "pull_request_comment",
            "pull_request_comment_reply",
        ],
    ),
    ("issue_opened", "Issue opened", &["issue_opened"]),
    (
        "pull_request_opened",
        "Pull request opened",
        &["pull_request_opened"],
    ),
    (
        "pull_request_merged",
        "Pull request merged",
        &["pull_request_merged"],
    ),
    (
        "repository_forked",
        "Repository forked",
        &["repository_forked"],
    ),
    (
        "release_published",
        "Release published",
        &["release_published"],
    ),
    ("user_followed", "Followed", &["user_followed"]),
    (
        "repository_activity",
        "Stars and watches",
        &["repository_starred", "repository_watched"],
    ),
    ("invitation", "Invitations", &["organization_invitation"]),
];

/// The stored kinds a reason covers, or None when the name is not a reason.
pub fn kinds(reason: &str) -> Option<&'static [&'static str]> {
    REASONS
        .iter()
        .find(|(name, _, _)| *name == reason)
        .map(|(_, _, kinds)| *kinds)
}

/// The label of the reason a stored kind belongs to, or None for a kind no reason covers.
pub fn label_for_kind(kind: &str) -> Option<&'static str> {
    REASONS
        .iter()
        .find(|(_, _, kinds)| kinds.contains(&kind))
        .map(|(_, label, _)| *label)
}
