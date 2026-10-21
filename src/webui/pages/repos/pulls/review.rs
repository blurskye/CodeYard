//! Review presentation for pull requests: the review form and reviews in the discussion,
//! the reviewers panel, conversation badges, and inline threads on the diff. Callers decide
//! what the viewer may do and pass those flags in. The server checks every submission again.
use crate::webui::pages::repos::issues::{
    detail::{
        ago, author_link, comment_entry, csrf, glyph, picker_heading, DetailDraft, Glyph,
        PickerPopover, PickerRow,
    },
    editor::MarkdownEditor,
    reactions::{react_action, reaction_bar},
    Comment, Event, Reaction, Repository,
};
use crate::webui::{
    components::{
        confirm_dialog, count_label, references::References, Avatar, DraftInput, DraftTextarea,
        Markdown,
    },
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
    /// The inline comments filed in this review, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Comment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments_more: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments_start: Option<String>,
    /// The reactions on the review itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
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
    /// The reviewer is named as an owner of a changed file in CODEOWNERS.
    #[serde(default)]
    pub code_owner: bool,
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
    /// The thread is in the viewer's pending review. Nobody else sees it until the review is submitted.
    #[serde(default)]
    pub pending: bool,
}

/// A comment being written under a diff line, or as a reply in a thread. A refused
/// submission keeps it, so the composer opens again with the same text.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LineComment {
    #[serde(default)]
    pub repository_id: Option<i64>,
    pub path: String,
    pub side: String,
    pub line: String,
    pub in_reply_to: Option<i64>,
    pub body: String,
    pub preview: bool,
    /// The commit the reviewer submitted against; empty means the current head.
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub base: String,
}

/// A comment, a review or a change, in the order the discussion shows them.
pub(in crate::webui::pages::repos) enum Entry<'a> {
    Comment(&'a Comment),
    Review(&'a Review),
    Event(&'a Event),
}

/// Merges one discussion page. Each list is oldest first. RFC 3339 times written by the same
/// server compare in time order as text, and the sort is stable, so at the same time a comment
/// comes before a review and a review before a change.
pub(in crate::webui::pages::repos) fn timeline<'a>(
    comments: &'a [Comment],
    reviews: &'a [Review],
    events: &'a [Event],
) -> Vec<Entry<'a>> {
    let mut entries: Vec<(&str, Entry)> = comments
        .iter()
        .map(|comment| (comment.created.as_str(), Entry::Comment(comment)))
        .chain(
            reviews
                .iter()
                .map(|review| (review.created.as_str(), Entry::Review(review))),
        )
        .chain(
            events
                .iter()
                .map(|event| (event.created.as_str(), Entry::Event(event))),
        )
        .collect();
    entries.sort_by_key(|(created, _)| *created);
    entries.into_iter().map(|(_, entry)| entry).collect()
}

/// The short form of a commit, as the page shows it.
fn short(sha: &str) -> &str {
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

/// The comment box under the discussion, for a signed-in viewer. It posts one comment on its own.
/// Reviews are written on the Files changed page. `state_label` is the Close or Reopen button,
/// which submits the state form beside this one.
pub(in crate::webui::pages::repos) fn composer(
    repo: &Repository,
    url: &str,
    head: &str,
    item_id: i64,
    draft: &DetailDraft,
    state_label: Option<&str>,
    references: &References,
) -> Element {
    rsx! { CommentComposer { repo: repo.clone(), url: url.to_owned(), head: head.to_owned(), item_id, draft: draft.clone(), state_label: state_label.map(str::to_owned), references: references.clone() } }
}

#[component]
fn CommentComposer(
    repo: Repository,
    url: String,
    head: String,
    item_id: i64,
    draft: DetailDraft,
    state_label: Option<String>,
    references: References,
) -> Element {
    let repository_id = use_signal(|| {
        draft
            .repository_id
            .clone()
            .unwrap_or_else(|| repo.chrome.id.to_string())
    });
    let issue_id = use_signal(|| {
        draft
            .issue_id
            .clone()
            .unwrap_or_else(|| item_id.to_string())
    });
    let preview = draft.preview && draft.title.is_none() && draft.editing.is_none();
    let comment = if draft.editing.is_some() {
        String::new()
    } else {
        draft.comment.clone()
    };
    rsx! {
        form { method: "post", action: format!("{url}/comments"), class: "collaboration-panel collaboration-comment-form collaboration-composer",
            header { class: "collaboration-panel-header", h3 { "Add a comment" } }
            {csrf(&repo.csrf)}
            input { r#type: "hidden", name: "commit_sha", value: head.to_owned() }
            input { r#type: "hidden", name: "repository_id", value: repository_id() }
            input { r#type: "hidden", name: "issue_id", value: issue_id() }
            div { class: "collaboration-form-fields",
                // The header already names the box, so the label is for screen readers only.
                label { r#for: "comment-body", class: "repo-sr-only", "Comment (Markdown)" }
                MarkdownEditor { key: "{url}:comment:{comment}:{preview}", id: "comment-body", initial: comment, rows: 6, required: true, initial_preview: preview, references: references.clone() }
            }
            div { class: "collaboration-card-footer",
                if let Some(label) = state_label { button { r#type: "submit", form: "item-state-form", class: "btn btn-secondary", "{label}" } }
                button { r#type: "submit", name: "intent", value: "comment", class: "btn btn-primary collaboration-submit-comment", "Comment" }
            }
        }
    }
}

/// The Review changes menu on the Files changed page. While the pull request is open it holds the
/// review form: a summary, the Comment, Approve and Request changes choices, and Submit review. The
/// menu's count is the viewer's pending comments, which Discard review removes, even after the pull
/// request closes. `author` and `vote` only pick the explanation shown beside the choices that the
/// viewer may not make; the server checks them again.
#[expect(
    clippy::too_many_arguments,
    reason = "The review form takes the page's values one by one, as the other page forms do."
)]
pub(in crate::webui::pages::repos) fn review_changes(
    repo: &Repository,
    url: &str,
    head: &str,
    open: bool,
    author: bool,
    vote: bool,
    draft: Option<&ReviewDraft>,
    pending_comments: usize,
    references: &References,
    split: bool,
) -> Element {
    let choice = draft.map_or("commented", |draft| draft.state.as_str());
    let body = draft.map_or_else(String::new, |draft| draft.body.clone());
    let preview = draft.is_some_and(|draft| draft.preview);
    let commit = draft
        .filter(|draft| !draft.commit.is_empty())
        .map_or_else(|| head.to_owned(), |draft| draft.commit.clone());
    let reason = if author {
        "You opened this pull request, so you can comment but not approve or request changes."
    } else if !vote {
        "Approving and requesting changes needs write access to this repository."
    } else {
        ""
    };
    let pending_text = count_label(pending_comments, "pending comment", "pending comments");
    rsx! {
        details { class: "native-menu collaboration-review-menu", open: draft.is_some(),
            summary { class: "btn btn-primary collaboration-review-summary", title: "Review these changes", aria_label: "Review changes",
                "Review changes"
                if pending_comments > 0 { span { class: "collaboration-badge collaboration-review-count", title: "Comments waiting in your pending review", "{pending_comments}" } }
                {glyph(Glyph::Caret)}
            }
            div { class: "native-menu-panel collaboration-review-panel",
                if open {
                    form { id: "review-form", method: "post", action: format!("{url}/reviews{}", if split { "?view=split" } else { "" }), class: "collaboration-form-fields collaboration-review-form",
                        {csrf(&repo.csrf)}
                        div { class: "collaboration-review-body",
                            h3 { "Review changes" }
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
                                    input { r#type: "radio", name: "state", value: "approved", checked: choice == "approved", disabled: !vote }
                                    span { class: "collaboration-option-body",
                                        strong { "Approve" }
                                        span { class: "collaboration-option-note", "Approve these changes for merging." }
                                    }
                                }
                                label { class: "collaboration-option",
                                    input { r#type: "radio", name: "state", value: "changes_requested", checked: choice == "changes_requested", disabled: !vote }
                                    span { class: "collaboration-option-body",
                                        strong { "Request changes" }
                                        span { class: "collaboration-option-note", "Ask the author to make changes before merging." }
                                    }
                                }
                            }
                            if !reason.is_empty() { p { class: "collaboration-muted collaboration-review-reason", "{reason}" } }
                            // The header already names the box, so the label is for screen readers only.
                            label { r#for: "review-body", class: "repo-sr-only", "Summary (Markdown)" }
                            ReviewEditor { key: "{url}:review:{choice}:{body}:{preview}", commit, body, preview, references: references.clone() }
                        }
                        div { class: "collaboration-card-footer",
                            button { r#type: "submit", name: "intent", value: "submit", class: "btn btn-primary collaboration-submit-review", "Submit review" }
                        }
                    }
                }
                if pending_comments > 0 {
                    details { class: "native-menu collaboration-review-discard",
                        summary { class: "btn btn-secondary", "Discard review" }
                        // Cancel reloads the Files changed page, which closes the dialog without JavaScript.
                        {confirm_dialog(None, "Discard review", &format!("{url}/diff"), &format!("{url}/reviews/pending/discard"), rsx! {
                            {csrf(&repo.csrf)}
                            strong { "Discard your review?" }
                            p { "This deletes {pending_text}. This cannot be undone." }
                            label { class: "repo-check-label", input { r#type: "checkbox", required: true, name: "confirm", value: "yes" } " I understand" }
                        }, "Discard review")}
                    }
                }
            }
        }
    }
}

#[component]
fn ReviewEditor(commit: String, body: String, preview: bool, references: References) -> Element {
    let mut revision = use_signal(|| commit.clone());
    rsx! {
        input { r#type: "hidden", name: "commit_sha", value: revision() }
        MarkdownEditor { id: "review-body", initial: body, rows: 6, required: false, initial_preview: preview, references, revision: Some(commit), on_revision: move |value: Option<String>| revision.set(value.unwrap_or_default()) }
    }
}

/// The label of a review's state, as its badge and the timeline show it.
fn review_badge(state: &str) -> &'static str {
    match state {
        "approved" => "Approved",
        "changes_requested" => "Changes requested",
        "dismissed" => "Dismissed",
        _ => "Commented",
    }
}

/// The text of a review that a closed pull request refused. It can be copied, not submitted.
pub(in crate::webui::pages::repos) fn closed_review(draft: &ReviewDraft) -> Element {
    rsx! {
        section { class: "collaboration-panel collaboration-form-fields", aria_label: "Review text that was not saved",
            p { class: "collaboration-muted", "This pull request is closed; your text was not saved." }
            // DraftTextarea escapes the text as the textarea's content. A text node here would carry
            // hydration comment markers into the field.
            DraftTextarea { name: "body", initial_value: draft.body.clone(), class: "collaboration-textarea", rows: 6u32, readonly: true }
        }
    }
}

/// One review in the discussion, with the inline comments filed in it. The dismiss form appears
/// to administrators only.
pub(in crate::webui::pages::repos) fn review_entry(
    repo: &Repository,
    url: &str,
    page: usize,
    item_author: &str,
    review: &Review,
    references: &References,
) -> Element {
    rsx! {
        article { id: format!("review-{}", review.id), class: "collaboration-panel collaboration-comment collaboration-review",
            header { class: "collaboration-panel-header",
                {author_link(&review.author)} " " "{review_action(&review.state)}" " on "
                code { title: review.commit.clone(), "{short(&review.commit)}" }
                " "
                {ago(&review.created)}
                span { class: "collaboration-badge collaboration-review-state", "{review_badge(&review.state)}" }
                if review.stale { span { class: "collaboration-badge", title: "This review was written for an earlier commit.", "Stale" } }
            }
            if let Some(reason) = &review.dismissed_reason { p { class: "collaboration-muted collaboration-entry-note", "Dismissed: {reason}" } }
            if let Some(body) = review.body.as_deref().filter(|body| !body.trim().is_empty()) {
                div { class: "collaboration-markdown", Markdown { content: body.to_owned(), references: references.clone() } }
            }
            {reaction_bar(repo, &react_action(url, page), Some(("review", review.id)), &review.reactions)}
            for comment in &review.comments {
                {comment_entry(repo, url, page, item_author, comment, references)}
            }
            if review.comments_start.is_some() || review.comments_more.is_some() {
                nav { class: "collaboration-pagination", aria_label: "Review comments",
                    if let Some(href) = &review.comments_start { NavLink { href: href.clone(), "First review comments" } }
                    if let Some(href) = &review.comments_more { NavLink { href: href.clone(), "More review comments" } }
                }
            }
            if repo.can_admin && matches!(review.state.as_str(), "approved" | "changes_requested") {
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
/// reviewer who already decided, and a picker to ask someone new. `typed` is the name of a refused
/// request, kept in the picker's form.
pub(in crate::webui::pages::repos) fn reviewers_panel(
    repo: &Repository,
    url: &str,
    reviewers: &[Reviewer],
    assignable: &[String],
    typed: &str,
    manage: bool,
) -> Element {
    let requests = format!("{url}/requested_reviewers");
    let menu = manage.then(|| reviewer_options(repo, &requests, reviewers, assignable, typed));
    rsx! {
        section { id: "reviewers", class: "collaboration-sidebar-section",
            {picker_heading("Reviewers", menu)}
            if reviewers.is_empty() { p { class: "collaboration-muted", "No reviews or review requests yet." } }
            ul { class: "collaboration-reviewers",
                for reviewer in reviewers {
                    li { class: "collaboration-reviewer",
                        span { class: reviewer_mark_class(reviewer.status), "aria-hidden": "true", {reviewer_glyph(reviewer.status)} }
                        {author_link(&reviewer.username)}
                        span { class: "collaboration-muted", "{reviewer_status(reviewer.status)}" }
                        if reviewer.stale { span { class: "collaboration-badge", "Stale" } }
                        if reviewer.pending && reviewer.status != ReviewerStatus::Requested { span { class: "collaboration-badge", "Review requested" } }
                        if reviewer.code_owner { span { class: "collaboration-badge", title: "Owns a file this pull request changes", "Code owner" } }
                        if manage {
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
        }
    }
}

/// The reviewer picker: a row for each person who may be asked. A click asks them, and a click on
/// someone with an open request removes it. A name that is not listed is typed below the rows.
fn reviewer_options(
    repo: &Repository,
    requests: &str,
    reviewers: &[Reviewer],
    assignable: &[String],
    typed: &str,
) -> Element {
    let rows: Vec<(&String, bool)> = assignable
        .iter()
        .map(|name| {
            let pending = reviewers
                .iter()
                .any(|reviewer| reviewer.pending && reviewer.username == *name);
            (name, pending)
        })
        .collect();
    rsx! {
        PickerPopover { title: "Request reviews from",
            ul { class: "collaboration-picker-list",
                for (name, pending) in rows {
                    PickerRow { key: "{name}", text: name.clone(),
                        if pending {
                            form { method: "post", action: format!("{requests}/{}/remove", encode_component(name)), class: "collaboration-picker-form",
                                {csrf(&repo.csrf)}
                                button { r#type: "submit", class: "collaboration-picker-option", aria_pressed: "true",
                                    span { class: "collaboration-picker-check", aria_hidden: "true", "✓" }
                                    Avatar { name: name.clone(), size: 20 }
                                    "{name}"
                                }
                            }
                        } else {
                            form { method: "post", action: requests.to_owned(), class: "collaboration-picker-form",
                                {csrf(&repo.csrf)}
                                input { r#type: "hidden", name: "username", value: name.clone() }
                                button { r#type: "submit", class: "collaboration-picker-option", aria_pressed: "false",
                                    span { class: "collaboration-picker-check", aria_hidden: "true" }
                                    Avatar { name: name.clone(), size: 20 }
                                    "{name}"
                                }
                            }
                        }
                    }
                }
            }
            form { method: "post", action: requests.to_owned(), class: "collaboration-request-form",
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

pub(in crate::webui::pages::repos) fn conversation_badges(conversation: &Conversation) -> Element {
    rsx! {
        if conversation.resolved { span { class: "collaboration-badge", "Resolved" } }
        if conversation.outdated { span { class: "collaboration-badge", title: format!("Written on {}", short(&conversation.commit)), "Outdated" } }
    }
}

/// Resolves or reopens a conversation. `from_diff` returns the reviewer to the diff; otherwise the
/// discussion page they were on is kept.
pub(in crate::webui::pages::repos) fn resolve_form(
    repo: &Repository,
    url: &str,
    comment_id: i64,
    conversation: &Conversation,
    from_diff: bool,
    split: bool,
    page: usize,
) -> Element {
    let (action, caption) = if conversation.resolved {
        ("unresolve", "Reopen conversation")
    } else {
        ("resolve", "Resolve conversation")
    };
    // The post names the side-by-side view so the diff comes back in it.
    let view = if from_diff && split {
        "?view=split"
    } else {
        ""
    };
    rsx! {
        form { method: "post", action: format!("{url}/comments/{comment_id}/{action}{view}"), class: "collaboration-actions",
            {csrf(&repo.csrf)}
            if from_diff { input { r#type: "hidden", name: "from", value: "diff" } }
            if !from_diff { input { r#type: "hidden", name: "page", value: page.to_string() } }
            button { r#type: "submit", class: "btn btn-secondary", "{caption}" }
        }
    }
}

/// A comment box for one diff line or for a reply. It posts the head the reviewer saw, and
/// `from=diff` returns them to the diff. `split` keeps the side-by-side view on the way back: the
/// post goes to a URL that names it, so a refused comment comes back in that view too. A new line
/// comment starts the viewer's review, or adds to it when `review` is `Some(true)`, or posts on its
/// own. `review` is None on a closed pull request, where no review can be started. A reply posts on
/// its own: it joins the review of the thread it answers.
pub(super) fn line_composer(
    repo: &Repository,
    url: &str,
    revisions: (&str, &str),
    target: &LineComment,
    references: &References,
    split: bool,
    review: Option<bool>,
) -> Element {
    rsx! { LineComposer { repo: repo.clone(), url: url.to_owned(), head: revisions.0.to_owned(), base: revisions.1.to_owned(), target: target.clone(), references: references.clone(), split, review } }
}

#[component]
fn LineComposer(
    repo: Repository,
    url: String,
    head: String,
    base: String,
    target: LineComment,
    references: References,
    split: bool,
    review: Option<bool>,
) -> Element {
    let repository_id = use_signal(|| target.repository_id.unwrap_or(repo.chrome.id));
    let mut commit = use_signal(|| {
        if target.commit.is_empty() {
            head.clone()
        } else {
            target.commit.clone()
        }
    });
    let mut comparison_base = use_signal(|| {
        if target.base.is_empty() {
            base.clone()
        } else {
            target.base.clone()
        }
    });
    let revision = format!("{head}:{base}");
    let repo = &repo;
    let url = &url;
    let target = &target;
    let references = &references;
    let (id, caption) = match target.in_reply_to {
        Some(root) => (format!("reply-body-{root}"), "Reply (Markdown)"),
        None => (
            "line-comment-body".to_owned(),
            "Comment on this line (Markdown)",
        ),
    };
    let view = if split { "?view=split" } else { "" };
    let reply = target.in_reply_to.is_some();
    let start = if review == Some(true) {
        "Add to review"
    } else {
        "Start a review"
    };
    rsx! {
        form { method: "post", action: format!("{url}/comments{view}"), class: "collaboration-form-fields collaboration-line-composer",
            {csrf(&repo.csrf)}
            input { r#type: "hidden", name: "path", value: target.path.clone() }
            input { r#type: "hidden", name: "side", value: target.side.clone() }
            input { r#type: "hidden", name: "line", value: target.line.clone() }
            if let Some(root) = target.in_reply_to { input { r#type: "hidden", name: "in_reply_to", value: root.to_string() } }
            input { r#type: "hidden", name: "commit_sha", value: commit() }
            input { r#type: "hidden", name: "base_sha", value: comparison_base() }
            input { r#type: "hidden", name: "from", value: "diff" }
            input { r#type: "hidden", name: "repository_id", value: repository_id().to_string() }
            label { r#for: id.clone(), "{caption}" }
            MarkdownEditor { key: "{url}:{id}:{target.body}:{target.preview}", id: id.clone(), initial: target.body.clone(), rows: 4, required: true, initial_preview: target.preview, references: references.clone(), revision: Some(revision), on_revision: move |revision: Option<String>| {
                let revision = revision.unwrap_or_default();
                let (head, base) = revision.split_once(':').unwrap_or((&revision, ""));
                commit.set(head.to_owned());
                comparison_base.set(base.to_owned());
            } }
            div { class: "collaboration-actions",
                if reply {
                    button { r#type: "submit", name: "intent", value: "comment", class: "btn btn-primary", "Comment" }
                } else if review.is_some() {
                    button { r#type: "submit", name: "intent", value: "review", class: "btn btn-primary collaboration-submit-review-comment", "{start}" }
                    button { r#type: "submit", name: "intent", value: "comment", class: "btn btn-secondary", "Add single comment" }
                } else {
                    button { r#type: "submit", name: "intent", value: "comment", class: "btn btn-primary", "Add single comment" }
                }
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
    revisions: (&str, &str),
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
    // A reaction sent from the diff comes back to the diff, in the same view.
    let view = if split { "&view=split" } else { "" };
    let react = format!("{url}/react?from=diff{view}");
    rsx! {
        div { class: "collaboration-panel collaboration-diff-thread", id: format!("thread-{}", root.id),
            header { class: "collaboration-thread-header", id: format!("comment-{}", root.id),
                {author_link(&root.author)} " commented " {ago(&root.created)}
                if thread.pending { span { class: "collaboration-badge collaboration-pending", title: "Only you can see this comment until you submit your review.", "Pending" } }
                if let Some(conversation) = &root.conversation { {conversation_badges(conversation)} }
            }
            div { class: "collaboration-markdown", Markdown { content: root.body.clone(), references: references.clone() } }
            if !thread.pending { {reaction_bar(repo, &react, Some(("comment", root.id)), &root.reactions)} }
            for entry in &thread.replies {
                article { id: format!("comment-{}", entry.id), class: "collaboration-thread-reply",
                    header { class: "collaboration-thread-header", {author_link(&entry.author)} " replied " {ago(&entry.created)} }
                    div { class: "collaboration-markdown", Markdown { content: entry.body.clone(), references: references.clone() } }
                    if !thread.pending { {reaction_bar(repo, &react, Some(("comment", entry.id)), &entry.reactions)} }
                }
            }
            if let Some(conversation) = &root.conversation {
                if conversation.can_resolve && !thread.pending { {resolve_form(repo, url, root.id, conversation, true, split, 1)} }
            }
            if repo.signed_in {
                details { class: "collaboration-thread-reply-form", open: replying,
                    summary { "Reply" }
                    {line_composer(repo, url, revisions, &reply, references, split, None)}
                }
            }
        }
    }
}

#[cfg(test)]
mod draft_tests {
    use super::*;
    #[test]
    fn comment_composers_keep_the_original_repository_after_a_page_refresh() {
        use std::{cell::RefCell, rc::Rc};
        #[derive(Clone)]
        struct Harness(Rc<RefCell<Option<Signal<i64>>>>);
        fn root(props: Harness) -> Element {
            let id = use_signal(|| 1);
            *props.0.borrow_mut() = Some(id);
            let repo = Repository {
                owner: "u".into(),
                name: "r".into(),
                signed_in: true,
                can_write: false,
                can_admin: false,
                csrf: String::new(),
                viewer: String::new(),
                chrome: crate::webui::pages::repos::chrome::RepoChrome {
                    id: id(),
                    ..Default::default()
                },
            };
            rsx! { div {
                CommentComposer { repo: repo.clone(), url: "/u/r/pulls/1", head: "head", item_id: 1, draft: DetailDraft::default(), state_label: None, references: References::default() }
                LineComposer { repo, url: "/u/r/pulls/1", head: "head", base: "base", target: LineComment::default(), references: References::default(), split: false, review: None }
            } }
        }
        let control = Rc::new(RefCell::new(None));
        let mut dom = VirtualDom::new_with_props(root, Harness(control.clone()));
        dom.rebuild_in_place();
        let mut id = control.borrow().unwrap();
        dom.in_runtime(|| id.set(2));
        dom.render_immediate_to_vec();
        let html = dioxus_ssr::render(&dom);
        assert_eq!(
            html.matches("name=\"repository_id\" value=\"1\"").count(),
            2
        );
        assert!(!html.contains("name=\"repository_id\" value=\"2\""));
    }

    #[test]
    fn an_open_line_composer_keeps_the_commit_its_text_was_written_on() {
        use std::{cell::RefCell, rc::Rc};
        #[derive(Clone)]
        struct Harness(Rc<RefCell<Option<Signal<String>>>>);
        fn root(props: Harness) -> Element {
            let head = use_signal(|| "old-head".to_owned());
            *props.0.borrow_mut() = Some(head);
            rsx! { LineComposer { repo: Repository { owner: "u".into(), name: "r".into(), signed_in: true, can_write: false, can_admin: false, csrf: String::new(), viewer: String::new(), chrome: Default::default() }, url: "/u/r/pulls/1", head: head(), base: format!("base-{}", head()), target: LineComment::default(), references: References::default(), split: false, review: None } }
        }
        let control = Rc::new(RefCell::new(None));
        let mut dom = VirtualDom::new_with_props(root, Harness(control.clone()));
        dom.rebuild_in_place();
        let mut head = control.borrow().unwrap();
        dom.in_runtime(|| head.set("new-head".into()));
        dom.render_immediate_to_vec();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("name=\"commit_sha\" value=\"old-head\""));
        assert!(html.contains("name=\"base_sha\" value=\"base-old-head\""));
        assert!(!html.contains("value=\"new-head\""));
    }
}
