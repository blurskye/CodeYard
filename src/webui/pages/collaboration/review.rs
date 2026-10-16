//! Review presentation for pull requests: the review form and reviews in the discussion,
//! the reviewers panel, conversation badges, and inline threads on the diff. Callers decide
//! what the viewer may do and pass those flags in. The server checks every submission again.
use super::{
    detail::{author_link, csrf, DetailDraft},
    editor::MarkdownEditor,
    Comment, Repository,
};
use crate::webui::{
    components::{compact_date, references::References, DraftInput, DraftTextarea, Markdown},
    encode_component,
    navigation::NavLink,
};
use dioxus::prelude::*;

/// A review form that a refused submission re-renders with the reviewer's text.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReviewDraft {
    /// `commented`, `approved` or `changes_requested`.
    pub state: String,
    pub body: String,
    pub preview: bool,
    /// The commit the reviewer submitted against. Re-rendered as is, so a later submit is still
    /// checked against it; empty means the current head.
    #[serde(default)]
    pub commit: String,
}

/// One review in the discussion.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Review {
    pub id: i64,
    pub author: String,
    /// `commented`, `approved`, `changes_requested` or `dismissed`.
    pub state: String,
    pub body: Option<String>,
    /// The commit the review was written for.
    pub commit: String,
    pub created: String,
    /// The commit is no longer the pull request head. Computed when the page is read.
    pub stale: bool,
    pub dismissed_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReviewerStatus {
    Approved,
    ChangesRequested,
    Dismissed,
    Requested,
}

/// A reviewer in the sidebar: the latest decision, or a request with no decision yet.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reviewer {
    pub username: String,
    pub status: ReviewerStatus,
    /// The decision was written for a commit other than the pull request head.
    pub stale: bool,
    /// A review request is open. A reviewer who already decided can have one again.
    pub pending: bool,
}

/// The state of a review conversation: a root comment on a file.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    pub resolved: bool,
    /// The conversation was written on a commit other than the pull request head.
    pub outdated: bool,
    /// The viewer is the pull request author or may write to the repository.
    pub can_resolve: bool,
    pub commit: String,
}

/// An inline conversation: a root comment on a diff line and its replies.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Thread {
    pub root: Comment,
    pub path: String,
    /// A conversation on a whole file has no line.
    pub line: Option<i32>,
    /// `LEFT` for the base side, `RIGHT` for the head side, empty without a line.
    pub side: String,
    pub replies: Vec<Comment>,
}

/// A comment being written under a diff line, or as a reply in a thread. A refused
/// submission keeps it, so the composer opens again with the same text.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LineComment {
    pub path: String,
    pub side: String,
    pub line: String,
    pub in_reply_to: Option<i64>,
    pub body: String,
    pub preview: bool,
    /// The commit the reviewer submitted against; empty means the current head.
    #[serde(default)]
    pub commit: String,
}

/// A comment or a review, in the order the discussion shows them.
pub(super) enum Entry<'a> {
    Comment(&'a Comment),
    Review(&'a Review),
}

/// Merges one discussion page. Both lists are oldest first. RFC 3339 times written by the
/// same server compare in time order as text.
pub(super) fn timeline<'a>(comments: &'a [Comment], reviews: &'a [Review]) -> Vec<Entry<'a>> {
    let mut entries = Vec::with_capacity(comments.len() + reviews.len());
    let mut reviews = reviews.iter().peekable();
    for comment in comments {
        while let Some(review) = reviews.next_if(|review| review.created < comment.created) {
            entries.push(Entry::Review(review));
        }
        entries.push(Entry::Comment(comment));
    }
    entries.extend(reviews.map(Entry::Review));
    entries
}

/// The short form of a commit, as the page shows it.
pub(super) fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn review_action(state: &str) -> &'static str {
    match state {
        "approved" => "approved these changes",
        "changes_requested" => "requested changes",
        "dismissed" => "review dismissed",
        _ => "reviewed",
    }
}

fn reviewer_status(status: ReviewerStatus) -> &'static str {
    match status {
        ReviewerStatus::Approved => "Approved",
        ReviewerStatus::ChangesRequested => "Changes requested",
        ReviewerStatus::Dismissed => "Dismissed",
        ReviewerStatus::Requested => "Review requested",
    }
}

/// The one composer under the discussion, for a signed-in viewer. Everybody gets a comment box.
/// A viewer who may vote also gets the review choice above it. Comment posts to the comments
/// endpoint; Approve and Request changes post to the review endpoint through `formaction`. Only
/// the button for the chosen radio shows, which CSS decides, so the form works without JavaScript.
/// `state_label` is the Close or Reopen button, which submits the state form beside this one.
pub(super) fn composer(
    repo: &Repository,
    url: &str,
    head: &str,
    vote: bool,
    draft: &DetailDraft,
    state_label: Option<&str>,
    references: &References,
) -> Element {
    // A refused review comes back only while the pull request accepts votes. Otherwise its text
    // is shown read-only by `closed_review`.
    let refused = draft.review.as_ref().filter(|_| vote);
    let choice = refused.map_or("commented", |review| review.state.as_str());
    let body = refused.map_or_else(|| draft.comment.clone(), |review| review.body.clone());
    let preview = refused.map_or(draft.preview, |review| review.preview) && draft.title.is_none();
    let commit = refused
        .filter(|review| !review.commit.is_empty())
        .map_or_else(|| head.to_owned(), |review| review.commit.clone());
    let title = if vote {
        "Review changes"
    } else {
        "Add a comment"
    };
    rsx! {
        form { method: "post", action: format!("{url}/comments"), class: "collaboration-panel collaboration-comment-form collaboration-composer", id: vote.then_some("review-form"),
            header { class: "collaboration-panel-header", h3 { "{title}" } }
            {csrf(&repo.csrf)}
            input { r#type: "hidden", name: "commit_sha", value: commit }
            div { class: "collaboration-form-fields",
                if vote {
                    fieldset { class: "collaboration-option-group",
                        legend { "Review" }
                        label { class: "collaboration-option",
                            input { r#type: "radio", name: "state", value: "commented", checked: choice == "commented" }
                            span { class: "collaboration-option-body",
                                strong { "Comment" }
                                span { class: "collaboration-option-note", "Share feedback without approving or blocking." }
                            }
                        }
                        label { class: "collaboration-option",
                            input { r#type: "radio", name: "state", value: "approved", checked: choice == "approved" }
                            span { class: "collaboration-option-body",
                                strong { "Approve" }
                                span { class: "collaboration-option-note", "Approve these changes for merging." }
                            }
                        }
                        label { class: "collaboration-option",
                            input { r#type: "radio", name: "state", value: "changes_requested", checked: choice == "changes_requested" }
                            span { class: "collaboration-option-body",
                                strong { "Request changes" }
                                span { class: "collaboration-option-note", "Ask the author to make changes before merging." }
                            }
                        }
                    }
                }
                label { r#for: "comment-body", "Comment (Markdown)" }
                MarkdownEditor { key: "{url}:comment:{choice}:{body}:{preview}", id: "comment-body", initial: body, rows: 6, required: !vote, initial_preview: preview, references: references.clone() }
            }
            div { class: "collaboration-card-footer",
                if let Some(label) = state_label { button { r#type: "submit", form: "item-state-form", class: "btn btn-secondary", "{label}" } }
                button { r#type: "submit", name: "intent", value: "comment", class: "btn btn-primary collaboration-submit-comment", "Comment" }
                if vote {
                    button { r#type: "submit", name: "intent", value: "submit", formaction: format!("{url}/reviews"), class: "btn btn-primary collaboration-submit-approve", "Approve" }
                    button { r#type: "submit", name: "intent", value: "submit", formaction: format!("{url}/reviews"), class: "btn btn-primary collaboration-submit-changes", "Request changes" }
                }
            }
        }
    }
}

/// The text of a review that a closed pull request refused. It can be copied, not submitted.
pub(super) fn closed_review(draft: &ReviewDraft) -> Element {
    rsx! {
        section { class: "collaboration-panel collaboration-form-fields", aria_label: "Review text that was not saved",
            p { class: "collaboration-muted", "This pull request is closed; your text was not saved." }
            // DraftTextarea escapes the text as the textarea's content. A text node here would carry
            // hydration comment markers into the field.
            DraftTextarea { name: "body", initial_value: draft.body.clone(), class: "collaboration-textarea", rows: 6u32, readonly: true }
        }
    }
}

/// One review in the discussion. The dismiss form appears to administrators only.
pub(super) fn review_entry(
    repo: &Repository,
    url: &str,
    review: &Review,
    references: &References,
) -> Element {
    let dismissed = review.state == "dismissed";
    rsx! {
        article { id: format!("review-{}", review.id), class: "collaboration-panel collaboration-comment collaboration-review",
            header { class: "collaboration-panel-header",
                {author_link(&review.author)} " " "{review_action(&review.state)}" " on "
                code { title: review.commit.clone(), "{short(&review.commit)}" }
                " "
                time { datetime: review.created.clone(), "{compact_date(&review.created)}" }
                if review.stale { span { class: "collaboration-badge", title: "This review was written for an earlier commit.", "Stale" } }
            }
            if let Some(reason) = &review.dismissed_reason { p { class: "collaboration-muted collaboration-entry-note", "Dismissed: {reason}" } }
            if let Some(body) = review.body.as_deref().filter(|body| !body.trim().is_empty()) {
                div { class: "collaboration-markdown", Markdown { content: body.to_owned(), references: references.clone() } }
            }
            if repo.can_admin && !dismissed {
                details { class: "disclosure",
                    summary { "Dismiss review" }
                    form { method: "post", action: format!("{url}/reviews/{}/dismiss", review.id), class: "collaboration-form-fields",
                        {csrf(&repo.csrf)}
                        label { r#for: format!("dismiss-reason-{}", review.id), "Reason (optional)" }
                        input { id: format!("dismiss-reason-{}", review.id), name: "reason", maxlength: "65536", class: "collaboration-input" }
                        div { class: "collaboration-actions", button { r#type: "submit", class: "btn btn-secondary", "Dismiss review" } }
                    }
                }
            }
        }
    }
}

/// The mark before a reviewer's name. The status text beside it says the same thing, so the mark
/// is hidden from assistive technology.
fn reviewer_glyph(status: ReviewerStatus) -> &'static str {
    match status {
        ReviewerStatus::Approved => "✓",
        ReviewerStatus::ChangesRequested => "×",
        ReviewerStatus::Dismissed => "–",
        ReviewerStatus::Requested => "•",
    }
}
fn reviewer_mark_class(status: ReviewerStatus) -> &'static str {
    match status {
        ReviewerStatus::Approved => "collaboration-mark collaboration-mark-approved",
        ReviewerStatus::ChangesRequested => "collaboration-mark collaboration-mark-changes",
        ReviewerStatus::Dismissed | ReviewerStatus::Requested => "collaboration-mark",
    }
}

/// The sidebar list: each reviewer's latest decision, then the open review requests. When `manage`
/// is set, a writer gets a Remove button on each open request, a Re-request button on each
/// reviewer who already decided, and a form to ask someone new. `typed` is the name of a refused
/// request, kept in that form.
pub(super) fn reviewers_panel(
    repo: &Repository,
    url: &str,
    reviewers: &[Reviewer],
    typed: &str,
    manage: bool,
) -> Element {
    let requests = format!("{url}/requested_reviewers");
    rsx! {
        section { id: "reviewers", class: "collaboration-sidebar-section",
            h3 { "Reviewers" }
            if reviewers.is_empty() { p { class: "collaboration-muted", "No reviews or review requests yet." } }
            ul { class: "collaboration-reviewers",
                for reviewer in reviewers {
                    li { class: "collaboration-reviewer",
                        span { class: reviewer_mark_class(reviewer.status), "aria-hidden": "true", {reviewer_glyph(reviewer.status)} }
                        {author_link(&reviewer.username)}
                        span { class: "collaboration-muted", "{reviewer_status(reviewer.status)}" }
                        if reviewer.stale { span { class: "collaboration-badge", "Stale" } }
                        if reviewer.pending && reviewer.status != ReviewerStatus::Requested { span { class: "collaboration-badge", "Review requested" } }
                        // A deleted account has no name to send back, so it gets no buttons.
                        if manage && reviewer.username != "Deleted user" {
                            if reviewer.pending {
                                form { method: "post", action: format!("{requests}/{}/remove", encode_component(&reviewer.username)), class: "collaboration-actions",
                                    {csrf(&repo.csrf)}
                                    button { r#type: "submit", class: "btn btn-secondary btn-sm", aria_label: format!("Remove the review request for {}", reviewer.username), "Remove" }
                                }
                            } else {
                                form { method: "post", action: requests.clone(), class: "collaboration-actions",
                                    {csrf(&repo.csrf)}
                                    input { r#type: "hidden", name: "username", value: reviewer.username.clone() }
                                    button { r#type: "submit", class: "btn btn-secondary btn-sm", aria_label: format!("Ask {} to review again", reviewer.username), "Re-request review" }
                                }
                            }
                        }
                    }
                }
            }
            if manage {
                form { method: "post", action: requests.clone(), class: "collaboration-request-form",
                    {csrf(&repo.csrf)}
                    label { r#for: "reviewer-username", "Request a review from" }
                    div { class: "collaboration-request-row",
                        DraftInput { id: "reviewer-username", name: "username", initial_value: typed.to_owned(), required: true, class: "collaboration-input" }
                        button { r#type: "submit", class: "btn btn-secondary", "Request review" }
                    }
                }
            }
        }
    }
}

pub(super) fn conversation_badges(conversation: &Conversation) -> Element {
    rsx! {
        if conversation.resolved { span { class: "collaboration-badge", "Resolved" } }
        if conversation.outdated { span { class: "collaboration-badge", title: format!("Written on {}", short(&conversation.commit)), "Outdated" } }
    }
}

/// Resolves or reopens a conversation. `from_diff` returns the reviewer to the diff; otherwise the
/// discussion page they were on is kept.
pub(super) fn resolve_form(
    repo: &Repository,
    url: &str,
    comment_id: i64,
    conversation: &Conversation,
    from_diff: bool,
    page: usize,
) -> Element {
    let (action, caption) = if conversation.resolved {
        ("unresolve", "Reopen conversation")
    } else {
        ("resolve", "Resolve conversation")
    };
    rsx! {
        form { method: "post", action: format!("{url}/comments/{comment_id}/{action}"), class: "collaboration-actions",
            {csrf(&repo.csrf)}
            if from_diff { input { r#type: "hidden", name: "from", value: "diff" } }
            if !from_diff { input { r#type: "hidden", name: "page", value: page.to_string() } }
            button { r#type: "submit", class: "btn btn-secondary", "{caption}" }
        }
    }
}

/// A comment box for one diff line or for a reply. It posts the head the reviewer saw, and
/// `from=diff` returns them to the diff. `split` keeps the side-by-side view on the way back: the
/// post goes to a URL that names it, so a refused comment comes back in that view too.
pub(super) fn line_composer(
    repo: &Repository,
    url: &str,
    head: &str,
    target: &LineComment,
    references: &References,
    split: bool,
) -> Element {
    let (id, caption) = match target.in_reply_to {
        Some(root) => (format!("reply-body-{root}"), "Reply (Markdown)"),
        None => (
            "line-comment-body".to_owned(),
            "Comment on this line (Markdown)",
        ),
    };
    let commit = if target.commit.is_empty() {
        head.to_owned()
    } else {
        target.commit.clone()
    };
    let view = if split { "?view=split" } else { "" };
    rsx! {
        form { method: "post", action: format!("{url}/comments{view}"), class: "collaboration-form-fields collaboration-line-composer",
            {csrf(&repo.csrf)}
            input { r#type: "hidden", name: "path", value: target.path.clone() }
            input { r#type: "hidden", name: "side", value: target.side.clone() }
            input { r#type: "hidden", name: "line", value: target.line.clone() }
            if let Some(root) = target.in_reply_to { input { r#type: "hidden", name: "in_reply_to", value: root.to_string() } }
            input { r#type: "hidden", name: "commit_sha", value: commit }
            input { r#type: "hidden", name: "from", value: "diff" }
            label { r#for: id.clone(), "{caption}" }
            MarkdownEditor { key: "{url}:{id}:{target.body}:{target.preview}", id: id.clone(), initial: target.body.clone(), rows: 4, required: true, initial_preview: target.preview, references: references.clone() }
            div { class: "collaboration-actions",
                button { r#type: "submit", name: "intent", value: "comment", class: "btn btn-primary", "Comment" }
                NavLink { class: "btn btn-secondary", href: format!("{url}/diff{view}"), "Cancel" }
            }
        }
    }
}

/// An inline conversation under its diff line: the root, its replies, resolve, and a reply
/// form. `draft` reopens the reply form when a refused reply belongs to this thread.
pub(super) fn thread_block(
    repo: &Repository,
    url: &str,
    head: &str,
    thread: &Thread,
    draft: Option<&LineComment>,
    references: &References,
    split: bool,
) -> Element {
    let root = &thread.root;
    let replying = draft.is_some_and(|draft| draft.in_reply_to == Some(root.id));
    let reply = match draft {
        Some(draft) if replying => draft.clone(),
        _ => LineComment {
            path: thread.path.clone(),
            side: thread.side.clone(),
            line: thread.line.map(|line| line.to_string()).unwrap_or_default(),
            in_reply_to: Some(root.id),
            ..Default::default()
        },
    };
    rsx! {
        div { class: "collaboration-panel collaboration-diff-thread", id: format!("thread-{}", root.id),
            header { class: "collaboration-thread-header",
                {author_link(&root.author)} " commented " time { datetime: root.created.clone(), "{compact_date(&root.created)}" }
                if let Some(conversation) = &root.conversation { {conversation_badges(conversation)} }
            }
            div { class: "collaboration-markdown", Markdown { content: root.body.clone(), references: references.clone() } }
            for entry in &thread.replies {
                article { id: format!("comment-{}", entry.id), class: "collaboration-thread-reply",
                    header { class: "collaboration-thread-header", {author_link(&entry.author)} " replied " time { datetime: entry.created.clone(), "{compact_date(&entry.created)}" } }
                    div { class: "collaboration-markdown", Markdown { content: entry.body.clone(), references: references.clone() } }
                }
            }
            if let Some(conversation) = &root.conversation {
                if conversation.can_resolve { {resolve_form(repo, url, root.id, conversation, true, 1)} }
            }
            if repo.signed_in {
                details { class: "collaboration-thread-reply-form", open: replying,
                    summary { "Reply" }
                    {line_composer(repo, url, head, &reply, references, split)}
                }
            }
        }
    }
}
