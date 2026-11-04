use crate::models::Repository;
pub(crate) fn allows_delivery(subscribed: bool, ignored: bool, kind: &str) -> bool {
    !ignored
        && (subscribed
            || matches!(
                kind,
                "mention"
                    | "assigned"
                    | "pull_request_review_requested"
                    | "issue_assigned"
                    | "pull_request_assigned"
                    | "pull_request_comment_reply"
                    | "ci_completed"
            ))
}

/// Account-only notices have no GitHub repository subject and remain in the web inbox.
pub fn subject(
    input: &crate::queries::notifications::records::NewNotification<'_>,
) -> Option<(&'static str, String)> {
    let repo = input.repo_id.filter(|id| *id > 0)?;
    if let Some(id) = input.metadata["run_id"]
        .as_str()
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
    {
        return Some(("CheckSuite", id.to_string()));
    }
    if let Some(tag) = input.metadata["release_tag"]
        .as_str()
        .or_else(|| {
            (input.kind == "release_published")
                .then(|| input.metadata["subject"].as_str())
                .flatten()
        })
        .filter(|tag| !tag.is_empty())
    {
        return Some(("Release", tag.into()));
    }
    // Native issue/pull numbers share a repository sequence. Keeping this identity also
    // preserves the thread when an issue is promoted to a pull request.
    for field in ["pull_number", "issue_number", "number"] {
        if let Some(number) = input.metadata[field]
            .as_i64()
            .or_else(|| input.metadata[field].as_str()?.parse().ok())
            .filter(|id| *id > 0 && *id <= i32::MAX as i64)
        {
            return Some(("Discussion", number.to_string()));
        }
    }
    // Comments on different commits are different conversations.
    if let Some(sha) = input.metadata["commit_sha"]
        .as_str()
        .filter(|sha| codeyard_git::is_full_commit_id(sha))
    {
        return Some(("Commit", sha.to_ascii_lowercase()));
    }
    Some(("Repository", repo.to_string()))
}
