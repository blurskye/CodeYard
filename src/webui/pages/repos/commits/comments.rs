//! Commit discussions use native forms; Markdown preview also works before hydration.
use crate::webui::{
    components::{references::References, DraftTextarea, Markdown},
    navigation::NavLink,
    pages::repos::{
        chrome::{csrf, RepoChrome},
        issues::editor::MarkdownEditor,
    },
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Discussion {
    pub comments: Vec<Comment>,
    pub total: i64,
    pub page: usize,
    pub has_more: bool,
    pub can_comment: bool,
    pub draft: Option<Draft>,
    pub error: Option<String>,
    pub references: References,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: i64,
    pub author: Option<String>,
    pub body: String,
    pub created: String,
    pub edited: Option<String>,
    pub location: Option<String>,
    pub association: String,
    pub revision: String,
    pub can_edit: bool,
    pub can_delete: bool,
    #[serde(default)]
    pub reactions: Vec<crate::webui::pages::repos::issues::Reaction>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub id: Option<i64>,
    pub body: String,
    pub revision: String,
    pub preview: bool,
    pub repository_id: Option<String>,
    pub commit_sha: Option<String>,
}

#[component]
fn Composer(
    repo: RepoChrome,
    sha: String,
    draft: Draft,
    references: References,
    split: bool,
    page: usize,
) -> Element {
    // An unsent edit keeps its body and revision through unrelated page-model refreshes.
    let snapshot = use_signal(|| draft.clone());
    let draft = snapshot.read().clone();
    let repository_id = draft
        .repository_id
        .clone()
        .unwrap_or_else(|| repo.id.to_string());
    let commit_sha = draft.commit_sha.clone().unwrap_or_else(|| sha.clone());
    let base = format!("{}/commit/{sha}/comments", repo.base());
    let action = draft
        .id
        .map_or_else(|| base.clone(), |id| format!("{base}/{id}/edit"));
    let action = format!(
        "{action}?comments_page={page}{}",
        if split { "&view=split" } else { "" }
    );
    let editor = draft.id.map_or_else(
        || "new-commit-comment".into(),
        |id| format!("edit-commit-comment-{id}"),
    );
    rsx! {
        form {
            method: "post",
            action,
            class: "collaboration-comment-edit-form",
            {csrf(&repo.csrf_token)}
            input {
                r#type: "hidden",
                name: "repository_id",
                value: repository_id,
            }
            input { r#type: "hidden", name: "commit_sha", value: commit_sha }
            if draft.id.is_some() {
                input {
                    r#type: "hidden",
                    name: "expected_revision",
                    value: draft.revision,
                }
            }
            button {
                r#type: "submit",
                class: "sr-only",
                tabindex: "-1",
                aria_hidden: "true",
            }
            label { r#for: editor.clone(), class: "sr-only", "Comment (Markdown)" }
            // HTML maxlength counts UTF-16 units; two units leave room for every allowed Unicode scalar.
            MarkdownEditor {
                id: editor.clone(),
                initial: draft.body,
                rows: 8usize,
                required: true,
                initial_preview: draft.preview,
                maximum: 131_072,
                references,
            }
            div { class: "collaboration-form-actions",
                button { r#type: "submit", class: "btn btn-primary",
                    if draft.id.is_some() {
                        "Save changes"
                    } else {
                        "Comment"
                    }
                }
            }
        }
    }
}

/// A failed lookup must not consume the text submitted with a native form.
pub fn rejected(repo: &RepoChrome, sha: &str, draft: &Draft, message: &str) -> Element {
    crate::webui::pages::repos::chrome::chrome(
        repo,
        rsx! {
            section { class: "collaboration-panel",
                h3 { "Commit comment draft" }
                p { class: "collaboration-error", role: "alert", "{message}" }
                p { "Your submitted text is preserved below. Check the discussion before retrying." }
                DraftTextarea {
                    name: "body",
                    initial_value: draft.body.clone(),
                    readonly: true,
                    rows: 8,
                }
                if draft.preview {
                    Markdown { content: draft.body.clone() }
                }
                NavLink { href: format!("{}/commit/{sha}", repo.base()), "Reload commit" }
            }
        },
    )
}

pub fn discussion(repo: &RepoChrome, sha: &str, state: &Discussion, split: bool) -> Element {
    let base = format!("{}/commit/{sha}", repo.base());
    let page_link = |page| {
        format!(
            "{base}?comments_page={page}{}#comments",
            if split { "&view=split" } else { "" }
        )
    };
    rsx! {
        section { id: "comments", class: "collaboration-detail-main",
            h3 { "Comments ({state.total})" }
            if let Some(error) = &state.error {
                p { class: "collaboration-error", role: "alert", "{error}" }
            }
            for comment in &state.comments {
                article {
                    key: "{comment.id}",
                    id: format!("commitcomment-{}", comment.id),
                    class: "collaboration-panel collaboration-comment",
                    header { class: "collaboration-panel-header",
                        if let Some(author) = &comment.author {
                            NavLink { href: crate::webui::profile_path(author),
                                "{author}"
                            }
                        } else {
                            span { "Deleted user" }
                        }
                        " commented "
                        time { datetime: comment.created.clone(), "{comment.created}" }
                        if comment.association != "NONE" {
                            span { class: "collaboration-badge", "{comment.association}" }
                        }
                        if let Some(edited) = &comment.edited {
                            span {
                                class: "collaboration-muted",
                                title: format!("Edited {edited}"),
                                "edited"
                            }
                        }
                    }
                    if let Some(location) = &comment.location {
                        p { class: "collaboration-comment-location", "{location}" }
                    }
                    div { class: "collaboration-markdown",
                        Markdown {
                            content: comment.body.clone(),
                            references: state.references.clone(),
                        }
                    }
                    {
                        crate::webui::pages::repos::issues::reactions::reaction_form(
                            &repo.csrf_token,
                            state.can_comment,
                            &format!(
                                "{base}/comments/{}/react{}",
                                comment.id,
                                if split { "?view=split" } else { "" },
                            ),
                            None,
                            &comment.reactions,
                            None,
                        )
                    }
                    if comment.can_edit || comment.can_delete {
                        div { class: "collaboration-comment-controls",
                            if comment.can_edit {
                                details { open: state.draft.as_ref().is_some_and(|draft| draft.id == Some(comment.id)),
                                    summary { class: "btn btn-secondary btn-sm", "Edit" }
                                    Composer {
                                        key: "edit-{comment.id}-{repo.viewer_id}",
                                        repo: repo.clone(),
                                        split, page: state.page,
                                        sha: sha.to_owned(),
                                        references: state.references.clone(),
                                        draft: state
                                            .draft
                                            .as_ref()
                                            .filter(|draft| draft.id == Some(comment.id))
                                            .cloned()
                                            .unwrap_or_else(|| Draft {
                                                id: Some(comment.id),
                                                body: comment.body.clone(),
                                                revision: comment.revision.clone(),
                                                ..Draft::default()
                                            }),
                                    }
                                }
                            }
                            if comment.can_delete {
                                details { class: "repo-delete-menu",
                                    summary { class: "btn btn-danger btn-sm", "Delete" }
                                    form {
                                        method: "post",
                                        action: format!("{base}/comments/{}/delete?comments_page={}{}", comment.id, state.page, if split { "&view=split" } else { "" }),
                                        {csrf(&repo.csrf_token)}
                                        input {
                                            r#type: "hidden",
                                            name: "repository_id",
                                            value: repo.id.to_string(),
                                        }
                                        input {
                                            r#type: "hidden",
                                            name: "commit_sha",
                                            value: sha,
                                        }
                                        input {
                                            r#type: "hidden",
                                            name: "expected_revision",
                                            value: comment.revision.clone(),
                                        }
                                        p { "Delete this comment? Its text is removed for everyone." }
                                        button {
                                            r#type: "submit",
                                            class: "btn btn-danger",
                                            "Delete comment"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if state.comments.is_empty() {
                p { class: "collaboration-muted",
                    if state.total == 0 {
                        "No comments yet."
                    } else {
                        "No comments on this page."
                    }
                }
            }
            if state.page > 1 || state.has_more {
                nav {
                    aria_label: "Commit comment pages",
                    class: "collaboration-pagination",
                    if state.page > 1 {
                        NavLink { href: page_link(state.page - 1), "Previous" }
                    }
                    span { "Page {state.page}" }
                    if state.has_more {
                        NavLink { href: page_link(state.page + 1), "Next" }
                    }
                }
            }
            if let Some(draft) = state
                .draft
                .as_ref()
                .filter(|draft| {
                    draft
                        .id
                        .map_or(
                            !state.can_comment,
                            |id| {
                                !state
                                    .comments
                                    .iter()
                                    .any(|comment| comment.id == id && comment.can_edit)
                            },
                        )
                })
            {
                div { class: "collaboration-panel",
                    p { "This submission cannot be applied. Your text is preserved below." }
                    DraftTextarea {
                        name: "body",
                        initial_value: draft.body.clone(),
                        readonly: true,
                        rows: 8,
                    }
                    if draft.preview {
                        Markdown {
                            content: draft.body.clone(),
                            references: state.references.clone(),
                        }
                    }
                }
            }
            if state.can_comment {
                div { class: "collaboration-panel",
                    Composer {
                        key: "new-{repo.id}-{sha}-{repo.viewer_id}",
                        repo: repo.clone(),
                        split, page: state.page,
                        sha: sha.to_owned(),
                        references: state.references.clone(),
                        draft: state.draft.as_ref().filter(|draft| draft.id.is_none()).cloned().unwrap_or_default(),
                    }
                }
            } else if repo.is_archived {
                p { class: "collaboration-muted",
                    "This repository is archived. Commit discussions are read-only."
                }
            } else if !repo.signed_in {
                NavLink { href: "/login", "Sign in to comment" }
            }
        }
    }
}
