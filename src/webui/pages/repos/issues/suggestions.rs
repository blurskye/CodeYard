use super::{
    detail::ago,
    page::{IssueSuggestions, SuggestionDraft},
    Repository,
};
use crate::webui::{
    components::{DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, SelectOption},
    navigation::NavLink,
    pages::repos::chrome::csrf,
};
use dioxus::prelude::*;
fn proposal_form(
    repo: &Repository,
    url: &str,
    issue_id: i64,
    draft: Option<&SuggestionDraft>,
    disabled: bool,
) -> Element {
    let change = draft
        .map(|d| d.change.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("add_label");
    rsx! {
        form {
            key: "{url}:proposal",
            method: "post",
            action: format!("{url}/suggestions"),
            class: "collaboration-form-fields",
            {csrf(&repo.csrf)}
            input { r#type: "hidden", name: "repository_id", value: draft.map(|d| d.repository_id).unwrap_or(Some(repo.chrome.id)).map(|id| id.to_string()).unwrap_or_default() }
            input { r#type: "hidden", name: "issue_id", value: draft.map(|d| d.issue_id).unwrap_or(Some(issue_id)).map(|id| id.to_string()).unwrap_or_default() }
            input { r#type: "hidden", name: "action", value: "propose" }
            label {
                "Change"
                DraftSelect {
                    name: "change",
                    class: "collaboration-input",
                    disabled,
                    initial_value: change.to_owned(),
                    options: ["set_type", "add_label", "add_field", "add_assignee", "close_issue"]
                        .into_iter()
                        .map(|action| SelectOption::new(action, action))
                        .collect(),
                }
            }
            label {
                "Target name"
                DraftInput {
                    name: "target",
                    class: "collaboration-input",
                    initial_value: draft.map(|d| d.target.clone()).unwrap_or_default(),
                    readonly: disabled,
                }
            }
            p { class: "collaboration-muted",
                "Use a type, label or field name, or an assignee username. For duplicate closures use an issue reference. Leave the target blank to clear the type."
            }
            label {
                "Proposed value"
                DraftTextarea {
                    name: "value",
                    class: "collaboration-input",
                    initial_value: draft.map(|d| d.value.clone()).unwrap_or_default(),
                    readonly: disabled,
                }
            }
            p { class: "collaboration-muted",
                "Field values use the field's data type; for multi-select, use one option per line. Closing accepts completed, not_planned or duplicate (default: completed)."
            }
            label {
                "Rationale"
                DraftTextarea {
                    name: "rationale",
                    class: "collaboration-input",
                    initial_value: draft.map(|d| d.rationale.clone()).unwrap_or_default(),
                    maxlength: Some(16384),
                    readonly: disabled,
                }
            }
            label {
                "Confidence"
                DraftSelect {
                    name: "confidence",
                    class: "collaboration-input",
                    disabled,
                    initial_value: draft.map(|d| d.confidence.clone()).unwrap_or_default(),
                    options: vec![
                        SelectOption::new("", "Not specified"),
                        SelectOption::new("LOW", "LOW"),
                        SelectOption::new("MEDIUM", "MEDIUM"),
                        SelectOption::new("HIGH", "HIGH"),
                    ],
                }
            }
            if !disabled {
                button { r#type: "submit", class: "btn btn-secondary btn-sm", "Propose change" }
            }
        }
    }
}
pub(super) fn panel(
    repo: &Repository,
    url: &str,
    issue_id: i64,
    data: Option<&IssueSuggestions>,
    draft: Option<&SuggestionDraft>,
) -> Element {
    let Some(data) = data else {
        return rsx! {
            section {
                class: "collaboration-sidebar-section",
                id: "issue-suggestions",
                h3 { "Unsaved suggestion" }
                p { class: "collaboration-muted",
                    "Your suggestion was not saved. Its text is preserved below."
                }
                {proposal_form(repo, url, issue_id, draft, true)}
            }
        };
    };
    let can_review = data.can_review && !repo.chrome.is_archived;
    rsx! {
        section {
            class: "collaboration-sidebar-section",
            id: "issue-suggestions",
            aria_label: "Issue suggestions",
            h3 { "Suggestions" }
            if repo.can_admin && !repo.chrome.is_archived {
                form { method: "post", action: format!("{url}/suggestions"),
                    {csrf(&repo.csrf)}
                    input { r#type: "hidden", name: "repository_id", value: repo.chrome.id.to_string() }
                    input { r#type: "hidden", name: "issue_id", value: issue_id.to_string() }
                    input {
                        r#type: "hidden",
                        name: "action",
                        value: "set_enabled",
                    }
                    label {
                        DraftCheckbox {
                            name: "enabled",
                            submission_value: "true",
                            initial_checked: data.enabled,
                        }
                        " Enable issue suggestions for this repository"
                    }
                    button { r#type: "submit", class: "btn btn-secondary btn-sm", "Save setting" }
                }
            }
            if data.enabled {
                form { method: "get", action: format!("{url}#issue-suggestions"),
                    label {
                        "Suggestion state"
                        DraftSelect {
                            key: "{url}:state:{data.state}",
                            name: "suggestion_state",
                            class: "collaboration-input",
                            initial_value: data.state.clone(),
                            options: ["pending", "all", "approved", "dismissed", "applied", "replaced", "invalidated"]
                                .into_iter()
                                .map(|state| SelectOption::new(state, state))
                                .collect(),
                        }
                    }
                    button { r#type: "submit", class: "btn btn-secondary btn-sm", "Filter suggestions" }
                }
                for row in &data.rows {
                    article {
                        class: "collaboration-sidebar-section",
                        id: format!("suggestion-{}", row.id),
                        strong { "{row.action}: {row.target_name}" }
                        p { class: "collaboration-muted",
                            "{row.state} · "
                            {ago(&row.created)}
                        }
                        if let Some(actor) = &row.actor {
                            p {
                                "Proposed by "
                                NavLink { href: crate::webui::profile_path(actor),
                                    "{actor}"
                                }
                            }
                        }
                        if let Some(confidence) = &row.confidence {
                            p { class: "collaboration-muted", "Confidence: {confidence}" }
                        }
                        if !row.value.is_null() {
                            details {
                                summary { "Proposed value" }
                                p { class: "repo-description",
                                    "{row.value.as_str().map(str::to_owned).unwrap_or_else(||row.value.to_string())}"
                                }
                            }
                        }
                        if let Some(rationale) = &row.rationale {
                            details {
                                summary { "Rationale" }
                                p { class: "repo-description", "{rationale}" }
                            }
                        }
                        if let Some(reviewer) = &row.resolved_by {
                            p { class: "collaboration-muted",
                                "Reviewed by {reviewer} · "
                                {ago(&row.updated)}
                            }
                        }
                        if can_review && row.state == "pending" {
                            div { class: "collaboration-actions",
                                for action in ["approve", "dismiss"] {
                                    if action == "dismiss" || row.action != "add_field" || repo.can_write {
                                        form {
                                            method: "post",
                                            action: format!("{url}/suggestions"),
                                            {csrf(&repo.csrf)}
                                            input { r#type: "hidden", name: "repository_id", value: repo.chrome.id.to_string() }
                                            input { r#type: "hidden", name: "issue_id", value: issue_id.to_string() }
                                            input {
                                                r#type: "hidden",
                                                name: "action",
                                                value: action,
                                            }
                                            input {
                                                r#type: "hidden",
                                                name: "suggestion_id",
                                                value: row.id.to_string(),
                                            }
                                            button {
                                                r#type: "submit",
                                                class: "btn btn-secondary btn-sm",
                                                "{action}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if data.rows.is_empty() {
                    p { class: "collaboration-muted", "No matching suggestions." }
                }
                if data.page > 1 {
                    NavLink {
                        class: "btn btn-secondary btn-sm",
                        href: format!(
                            "{url}?suggestion_state={}&suggestion_page={}#issue-suggestions",
                            data.state,
                            data.page - 1,
                        ),
                        "Previous suggestions"
                    }
                }
                if data.has_more {
                    NavLink {
                        class: "btn btn-secondary btn-sm",
                        href: format!(
                            "{url}?suggestion_state={}&suggestion_page={}#issue-suggestions",
                            data.state,
                            data.page + 1,
                        ),
                        "Next suggestions"
                    }
                }
            }
            if data.can_propose || draft.is_some() {
                details { open: draft.is_some(),
                    summary { "Propose an issue change" }
                    if !data.can_propose {
                        p { class: "collaboration-muted",
                            "This issue cannot currently accept proposals. Your text is preserved."
                        }
                    }
                    {proposal_form(repo, url, issue_id, draft, !data.can_propose)}
                }
            }
        }
    }
}
