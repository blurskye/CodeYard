use super::{
    editor::DetailEditForm,
    label_style, merge_method_available, preselected_merge_method,
    review::{self, Entry, ReviewDraft, Reviewer},
    CommentPage, Comparison, Item, Kind, Label, Milestone, Repository, StackLayer, StackMap,
    MERGE_COMMIT_REFUSED,
};
use crate::webui::{
    components::{
        compact_date, references::References, Avatar, DraftSelect, Markdown, SelectOption,
    },
    encode_component,
    navigation::NavLink,
    pages::repositories::{self, ref_path::file_url},
};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DetailDraft {
    pub title: Option<String>,
    pub body: Option<String>,
    pub comment: String,
    pub label: String,
    pub preview: bool,
    /// The merge method that the submitted merge form chose, kept when the merge is refused.
    pub merge_method: String,
    /// The review form as it was submitted, kept when the review is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewDraft>,
    /// The reviewer name a refused request was typed with, kept in the request form.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reviewer: String,
}

pub(super) fn notice(error: Option<&str>) -> Element {
    rsx! { if let Some(error) = error { p { role: "alert", class: "collaboration-notice", "{error}" } } }
}
pub(super) fn csrf(token: &str) -> Element {
    rsx! { input { r#type: "hidden", name: "csrf_token", value: token } }
}
pub(super) fn labels(values: &[Label]) -> Element {
    rsx! { for label in values { span { class: "collaboration-label", style: label_style(&label.color), "{label.name}" } } }
}
/// The labels of the issue sidebar. A writer's chip ends in a small × form that removes that label.
fn label_chips(repo: &Repository, url: &str, values: &[Label]) -> Element {
    rsx! {
        for label in values {
            div { class: "collaboration-label", style: label_style(&label.color),
                "{label.name}"
                if repo.can_admin {
                    form { method: "post", action: format!("{url}/labels/{}/remove", label.id), class: "collaboration-label-remove",
                        {csrf(&repo.csrf)}
                        button { r#type: "submit", class: "collaboration-label-remove-button", title: "Remove label {label.name}", aria_label: "Remove label {label.name}", "×" }
                    }
                }
            }
        }
    }
}
pub(super) fn author_link(username: &str) -> Element {
    rsx! {
        if username == "Deleted user" { span { "Deleted user" } }
        else {
            NavLink { class: "collaboration-author", href: format!("/{}", encode_component(username)),
                Avatar { name: username.to_owned(), size: 20 }
                "{username}"
            }
        }
    }
}
pub(super) fn state_class(state: &str) -> &'static str {
    match state {
        "open" => "collaboration-state collaboration-state-open",
        "merged" => "collaboration-state collaboration-state-merged",
        _ => "collaboration-state collaboration-state-closed",
    }
}
pub(super) fn item_icon(kind: Kind, state: &str) -> Element {
    let class = match state {
        "open" => "collaboration-item-icon collaboration-icon-open",
        "merged" => "collaboration-item-icon collaboration-icon-merged",
        _ => "collaboration-item-icon collaboration-icon-closed",
    };
    rsx! {
        svg { class, view_box: "0 0 16 16", width: "16", height: "16", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true", "focusable": "false",
            if kind == Kind::Issue {
                circle { cx: "8", cy: "8", r: "6.25" }
                if state == "open" { circle { cx: "8", cy: "8", r: ".75", fill: "currentColor", stroke: "none" } }
                else { path { d: "m5 8 2 2 4-4" } }
            } else {
                circle { cx: "4", cy: "3", r: "1.75" }
                circle { cx: "12", cy: "13", r: "1.75" }
                path { d: "M4 5v9" }
                if state == "merged" { path { d: "M12 11V8c0-3-8-1-8-5" } }
                else { path { d: "M12 11V5c0-1.1-.9-2-2-2H8m2-2L8 3l2 2" } }
            }
        }
    }
}
/// The state, author, branches and draft marker under a title, shared by the Discussion and
/// Files changed tabs.
pub(super) fn pull_summary(repo: &Repository, item: &Item, kind: Kind) -> Element {
    rsx! {
        div { class: "collaboration-detail-meta",
            span { class: state_class(&item.state), {item_icon(kind, &item.state)} "{item.state}" }
            if let Some(stack) = &item.stack { span { class: "collaboration-muted", "{layer_position(stack)}" } }
            p { class: "collaboration-muted", {author_link(&item.author)} " opened this {kind.singular()} on " time { datetime: item.created.clone(), "{compact_date(&item.created)}" } }
        }
        if let Some(compare) = &item.comparison {
            p { class: "collaboration-comparison",
                if let Some(href) = &compare.head_url {
                    NavLink { class: "collaboration-branch", href: href.clone(), "{compare.head_label}" }
                } else { span { class: "collaboration-branch", "{compare.head_label}" } }
                " → "
                NavLink { class: "collaboration-branch", href: file_url(&repo.url(), "tree", &compare.base, ""), "{compare.base}" }
                if compare.draft { span { class: "collaboration-muted", " · Draft pull request" } }
            }
        }
    }
}

/// "Layer 2 of 3", as one text node.
fn layer_position(stack: &StackMap) -> String {
    format!("Layer {} of {}", stack.position, stack.layers.len())
}
fn layer_title(layer: &StackLayer) -> String {
    format!("#{} {}", layer.number, layer.title)
}
/// The sentence that offers to add a pull request to the stack of the one it is based on.
fn based_on_text(number: i32) -> String {
    format!("This pull request is based on #{number}'s branch.")
}
/// The class of a layer in the stack list. The viewed layer stands out, and a merged one reads as
/// finished.
fn stack_layer_class(layer: &StackLayer) -> &'static str {
    if layer.current {
        "collaboration-stack-layer collaboration-stack-current"
    } else if layer.state == "merged" {
        "collaboration-stack-layer collaboration-stack-merged"
    } else {
        "collaboration-stack-layer"
    }
}
/// Pull request numbers as `#1, #2`.
fn numbered(numbers: &[i32]) -> String {
    numbers
        .iter()
        .map(|number| format!("#{number}"))
        .collect::<Vec<_>>()
        .join(", ")
}
/// The label of a merge method on the merge form.
fn merge_method_label(method: &str) -> &'static str {
    match method {
        "squash" => "Squash and merge",
        "rebase" => "Rebase and merge",
        _ => "Create a merge commit",
    }
}
/// What a merge method does to the commits, shown under its label.
fn merge_method_note(method: &str) -> &'static str {
    match method {
        "squash" => "Adds all commits to the base branch as one commit.",
        "rebase" => "Adds each commit to the base branch without a merge commit.",
        _ => "Keeps every commit and records the merge in a merge commit.",
    }
}
/// Assignees, with the forms a writer uses to add one or remove one. Readers see the names only.
/// `assignable` holds the users who may still be added, and is empty when no more can be.
fn assignees_panel(repo: &Repository, url: &str, item: &Item, assignable: &[String]) -> Element {
    let can_assign_self = repo.can_write && assignable.contains(&repo.viewer);
    rsx! {
        section { class: "collaboration-sidebar-section",
            h3 { "Assignees" }
            if item.assignees.is_empty() { p { class: "collaboration-muted", "No one assigned." } }
            ul { class: "collaboration-assignees",
                for name in &item.assignees {
                    li { class: "collaboration-assignee",
                        {author_link(name)}
                        if repo.can_write {
                            form { method: "post", action: format!("{url}/assignees/{}/remove", encode_component(name)), class: "collaboration-assignee-action",
                                {csrf(&repo.csrf)}
                                button { r#type: "submit", class: "btn btn-secondary btn-sm", "Remove" span { class: "repo-sr-only", " {name}" } }
                            }
                        }
                    }
                }
            }
            if can_assign_self {
                form { method: "post", action: format!("{url}/assignees"), class: "collaboration-assignee-action",
                    {csrf(&repo.csrf)}
                    input { r#type: "hidden", name: "username", value: repo.viewer.clone() }
                    button { r#type: "submit", class: "btn btn-secondary btn-sm", "Assign yourself" }
                }
            }
            if !assignable.is_empty() {
                form { method: "post", action: format!("{url}/assignees"), class: "collaboration-assignee-form",
                    {csrf(&repo.csrf)}
                    label { "Assign "
                        DraftSelect { name: "username", class: "collaboration-input", initial_value: String::new(),
                            options: assignable.iter().map(|name| SelectOption::new(name.clone(), name.clone())).collect::<Vec<_>>() }
                    }
                    button { r#type: "submit", class: "btn btn-secondary", "Assign" }
                }
            }
        }
    }
}

/// The labels of an issue or pull request, with the form an administrator uses to add one. A
/// label chip ends in a small × form that removes it.
fn labels_panel(
    repo: &Repository,
    url: &str,
    item: &Item,
    available_labels: &[Label],
    draft: &DetailDraft,
) -> Element {
    rsx! {
        section { class: "collaboration-sidebar-section",
            h3 { "Labels" }
            div { class: "collaboration-labels", {label_chips(repo, url, &item.labels)} }
            if item.labels.is_empty() { p { class: "collaboration-muted", "No labels applied." } }
            if repo.can_admin {
                if !available_labels.is_empty() {
                    form { method: "post", action: format!("{url}/labels"), class: "collaboration-label-form",
                        {csrf(&repo.csrf)}
                        label { span { class: "repo-sr-only", "Label" }
                            DraftSelect { name: "label", class: "collaboration-input", initial_value: draft.label.clone(),
                                options: available_labels.iter().map(|label| SelectOption::new(label.id.to_string(), label.name.to_string())).collect::<Vec<_>>() }
                        }
                        button { r#type: "submit", class: "btn btn-secondary", "Add" }
                    }
                }
                NavLink { class: "collaboration-manage-labels", href: format!("{}/settings", repo.url()), "Manage labels" }
            }
        }
    }
}

/// The milestone of an issue, with the form a writer uses to set or clear it.
fn milestone_panel(repo: &Repository, url: &str, item: &Item, milestones: &[Milestone]) -> Element {
    let current = item
        .milestone
        .as_ref()
        .map(|milestone| milestone.number.to_string())
        .unwrap_or_default();
    rsx! {
        section { class: "collaboration-sidebar-section",
            h3 { "Milestone" }
            if repo.can_write && !milestones.is_empty() {
                // The select shows the current milestone, so the heading is its only visible label.
                form { method: "post", action: format!("{url}/milestone"), class: "collaboration-milestone-form",
                    {csrf(&repo.csrf)}
                    label { span { class: "repo-sr-only", "Milestone" }
                        DraftSelect { name: "milestone_number", class: "collaboration-input", initial_value: current.clone(),
                            options: std::iter::once(SelectOption::new("", "No milestone"))
                                .chain(milestones.iter().map(|milestone| SelectOption::new(milestone.number.to_string(), milestone.title.clone())))
                                .collect::<Vec<_>>() }
                    }
                    button { r#type: "submit", class: "btn btn-secondary", "Set milestone" }
                }
            } else if let Some(milestone) = &item.milestone {
                p { NavLink { class: "collaboration-title-link", href: format!("{}/milestones/{}", repo.url(), milestone.number), "{milestone.title}" } }
            } else { p { class: "collaboration-muted", "No milestone." } }
        }
    }
}

/// One entry of the discussion. A root comment on a file is wrapped in its thread anchor, which
/// resolve redirects use.
fn discussion_entry(
    repo: &Repository,
    url: &str,
    page: usize,
    entry: Entry,
    references: &References,
) -> Element {
    match entry {
        Entry::Review(review) => review::review_entry(repo, url, review, references),
        Entry::Comment(comment) => {
            let article = rsx! {
                article { id: format!("comment-{}", comment.id), class: "collaboration-panel collaboration-comment",
                    header { class: "collaboration-panel-header", {author_link(&comment.author)} " commented on " time { datetime: comment.created.clone(), "{compact_date(&comment.created)}" }
                        if let Some(conversation) = &comment.conversation { {review::conversation_badges(conversation)} }
                    }
                    if let Some(location) = &comment.location { p { class: "collaboration-comment-location", "{location}" } }
                    div { class: "collaboration-markdown", Markdown { content: comment.body.clone(), references: references.clone() } }
                    if let Some(conversation) = &comment.conversation {
                        if conversation.can_resolve { {review::resolve_form(repo, url, comment.id, conversation, false, page)} }
                    }
                }
            };
            match &comment.conversation {
                Some(_) => rsx! { div { id: format!("thread-{}", comment.id), {article} } },
                None => article,
            }
        }
    }
}
/// The merge box under the discussion. A writer gets the status, the blockers and the method form
/// while the pull request is open. Everyone else gets a status line, and a pull request that has
/// ended gets a banner instead of a form.
fn merge_box(
    repo: &Repository,
    url: &str,
    item: &Item,
    compare: &Comparison,
    default_method: Option<&str>,
) -> Element {
    if item.state != "open" {
        if item.state == "merged" {
            if let Some(sha) = compare.merged_commit.as_deref() {
                let commit_url = format!("{}/commit/{}", repo.url(), encode_component(sha));
                let short = sha.get(..7).unwrap_or(sha);
                return rsx! { p { class: "collaboration-state-banner", "This pull request was merged into {compare.base} as " NavLink { class: "collaboration-merge-sha", href: commit_url, "{short}" } "." } };
            }
            return rsx! { p { class: "collaboration-state-banner", "This pull request was merged." } };
        }
        return rsx! { p { class: "collaboration-state-banner", "This pull request is closed." } };
    }
    if !repo.can_write {
        return rsx! { p { class: "collaboration-state-banner collaboration-muted", "This pull request is open. Only people with write access can merge it." } };
    }
    let blocked = !compare.blockers.is_empty();
    rsx! {
        div { class: "collaboration-panel collaboration-merge-box",
            div { class: "collaboration-merge-status",
                span { class: if blocked { "collaboration-merge-mark collaboration-mark-changes" } else { "collaboration-merge-mark collaboration-mark-approved" }, "aria-hidden": "true",
                    if blocked { "×" } else { "✓" }
                }
                div {
                    h3 { if blocked { "Merging is blocked" } else { "Ready to merge" } }
                    p { class: "collaboration-muted", "Required reviews, status checks, and branch protection must pass before merging." }
                }
            }
            if blocked {
                ul { class: "collaboration-blockers",
                    for message in compare.blockers.iter() {
                        li { "{message}" }
                    }
                }
            }
            form { method: "post", action: format!("{url}/merge"), class: "collaboration-merge-form",
                {csrf(&repo.csrf)}
                input { r#type: "hidden", name: "expected_state", value: item.state.clone() }
                input { r#type: "hidden", name: "expected_updated", value: item.updated.clone() }
                input { r#type: "hidden", name: "expected_head", value: compare.head_sha.clone() }
                input { r#type: "hidden", name: "expected_base", value: compare.base_sha.clone() }
                input { r#type: "hidden", name: "expected_base_branch", value: compare.base.clone() }
                input { r#type: "hidden", name: "expected_target", value: compare.merge_target.clone() }
                div { class: "collaboration-form-fields",
                    fieldset { class: "collaboration-option-group",
                        legend { "Merge method" }
                        for method in compare.merge_methods.iter() {
                            label { class: "collaboration-option",
                                input { r#type: "radio", name: "merge_method", value: method.clone(), checked: default_method == Some(method.as_str()), disabled: !merge_method_available(compare, method) }
                                span { class: "collaboration-option-body",
                                    strong { {merge_method_label(method)} }
                                    span { class: "collaboration-option-note", {merge_method_note(method)} }
                                }
                            }
                        }
                        if compare.merge_commit_only {
                            p { class: "collaboration-muted", "Squash and rebase are unavailable while this pull request is stacked with open pull requests." }
                        }
                        if compare.merge_commit_refused {
                            p { class: "collaboration-muted", {MERGE_COMMIT_REFUSED} }
                        }
                    }
                    if compare.merge_with.len() > 1 {
                        p { class: "collaboration-muted", "Merging this pull request lands {numbered(&compare.merge_with)} on {compare.merge_target} together." }
                    }
                    label { class: "collaboration-checkbox", input { r#type: "checkbox", name: "confirm", value: "merge", required: true } " Confirm merge into {compare.merge_target}" }
                }
                div { class: "collaboration-card-footer",
                    button { r#type: "submit", class: "btn btn-primary", disabled: blocked || default_method.is_none(), "Merge pull request" }
                }
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Each field of the detail page model is a parameter; a struct would only wrap them."
)]
pub fn detail(
    repo: &Repository,
    kind: Kind,
    item: &Item,
    comments: &CommentPage,
    available_labels: &[Label],
    draft: &DetailDraft,
    error: Option<&str>,
    references: &References,
    reviewers: &[Reviewer],
    assignable: &[String],
    milestones: &[Milestone],
) -> Element {
    let url = format!("{}/{}", repo.family_url(kind), item.number);
    let page = comments.page;
    let has_more = comments.has_more;
    let default_method = item.comparison.as_ref().and_then(|compare| {
        preselected_merge_method(compare, &draft.merge_method).map(str::to_owned)
    });
    let head = item
        .comparison
        .as_ref()
        .map_or("", |compare| compare.head_sha.as_str());
    // Presentation only: the review handler checks the write right and the author again.
    let vote = kind == Kind::Pull
        && item.state == "open"
        && item
            .comparison
            .as_ref()
            .is_some_and(|compare| compare.can_vote);
    // The close or reopen form stays after the discussion. Its button sits in the composer footer
    // and submits that form through the form attribute, so the two forms never nest.
    let state_form = "item-state-form";
    let state_change = repo.can_write && (item.state == "open" || item.state == "closed");
    let state_label = if item.state == "open" {
        format!("Close {}", kind.singular())
    } else {
        format!("Reopen {}", kind.singular())
    };
    repositories::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-detail",
                {notice(error)}
                header { class: "collaboration-detail-heading",
                    h2 { "{item.title} ", span { class: "collaboration-number", "#{item.number}" } }
                    if repo.can_write {
                        details { open: draft.title.is_some() || draft.body.is_some(), class: "collaboration-edit", title: "Edit title and description",
                            summary { class: "btn btn-secondary btn-sm", "Edit" }
                            for edit_url in [url.clone()] {
                                DetailEditForm { key: "{edit_url}", url: edit_url, csrf_token: repo.csrf.clone(), item: item.clone(), draft: draft.clone(), references: references.clone() }
                            }
                        }
                    }
                    {pull_summary(repo, item, kind)}
                    if item.comparison.is_some() {
                        nav { class: "collaboration-detail-tabs", aria_label: "Pull request",
                            NavLink { class: "collaboration-detail-tab collaboration-detail-tab-active", href: url.clone(), span { aria_current: "page", "Discussion" } }
                            NavLink { class: "collaboration-detail-tab", href: format!("{url}/diff"), "Files changed" }
                        }
                        repositories::TabStripScroll { strip: ".collaboration-detail-tabs", path: url.clone() }
                    }
                }
                div { class: "collaboration-detail-layout",
                    div { class: "collaboration-timeline",
                        if let Some(stack) = &item.stack {
                            section { class: "collaboration-panel collaboration-stack", aria_label: "Stack",
                                header { class: "collaboration-panel-header", h3 { "Stack" } }
                                ol { class: "collaboration-stack-layers",
                                    for layer in stack.layers.iter().rev() {
                                        li { class: stack_layer_class(layer),
                                            span { class: "collaboration-stack-state", {item_icon(Kind::Pull, &layer.state)} span { class: "repo-sr-only", "{layer.state}" } }
                                            NavLink { class: "collaboration-title-link", href: format!("{}/{}", repo.family_url(Kind::Pull), layer.number),
                                                span { aria_current: if layer.current { "page" } else { "false" }, "{layer_title(layer)}" }
                                            }
                                            if layer.draft { span { class: "collaboration-muted", "Draft" } }
                                        }
                                    }
                                }
                                p { class: "collaboration-stack-base",
                                    span { class: "collaboration-muted", "Base" }
                                    NavLink { class: "collaboration-branch", href: file_url(&repo.url(), "tree", &stack.trunk, ""), "{stack.trunk}" }
                                }
                                if repo.can_write {
                                    if stack.restack {
                                        form { method: "post", action: format!("{url}/stack/rebase"), class: "collaboration-card-footer",
                                            {csrf(&repo.csrf)}
                                            p { class: "collaboration-muted", "Some open pull requests are not on top of the branch below them. Rebasing replays their own commits onto it." }
                                            button { r#type: "submit", class: "btn btn-secondary", "Rebase stack" }
                                        }
                                    }
                                    form { method: "post", action: format!("{url}/unstack"), class: "collaboration-card-footer",
                                        {csrf(&repo.csrf)}
                                        p { class: "collaboration-muted", "Unstacking keeps each open pull request on its base branch. Merged pull requests stay in the stack." }
                                        button { r#type: "submit", class: "btn btn-secondary", "Unstack" }
                                    }
                                }
                            }
                        }
                        if kind == Kind::Pull && item.comparison.as_ref().is_some_and(|compare| compare.can_ready) {
                            form { method: "post", action: format!("{url}/ready"), class: "collaboration-panel collaboration-action-row",
                                {csrf(&repo.csrf)}
                                input { r#type: "hidden", name: "expected_state", value: item.state.clone() }
                                input { r#type: "hidden", name: "expected_updated", value: item.updated.clone() }
                                p { class: "collaboration-muted", "This pull request is a draft. Mark it ready when it is time for review." }
                                button { r#type: "submit", class: "btn btn-primary", "Ready for review" }
                            }
                        }
                        if item.comparison.as_ref().is_some_and(|compare| compare.can_run_workflows) {
                            form { method: "post", action: format!("{url}/workflows"), class: "collaboration-panel collaboration-action-row",
                                {csrf(&repo.csrf)}
                                input { r#type: "hidden", name: "expected_head", value: item.comparison.as_ref().unwrap().head_sha.clone() }
                                p { class: "collaboration-muted", "Fork workflows need approval for each head commit and run without repository or organization secrets." }
                                button { r#type: "submit", class: "btn btn-primary", "Run workflows" }
                            }
                        }
                        if repo.can_write {
                            if let Some(number) = item.stack_candidate {
                                form { method: "post", action: format!("{url}/stack"), class: "collaboration-panel collaboration-action-row collaboration-stack-banner",
                                    {csrf(&repo.csrf)}
                                    p { class: "collaboration-muted", "{based_on_text(number)}" }
                                    button { r#type: "submit", class: "btn btn-secondary", "Add to stack" }
                                }
                            }
                        }
                        section { class: "collaboration-discussion", aria_label: "Discussion",
                            h3 { "Activity" }
                            div { class: "collaboration-thread",
                                article { class: "collaboration-panel collaboration-comment",
                                    header { class: "collaboration-panel-header", {author_link(&item.author)} " opened on " time { datetime: item.created.clone(), "{compact_date(&item.created)}" } }
                                    div { class: "collaboration-markdown",
                                        if item.body.trim().is_empty() { p { class: "collaboration-muted", "No description provided." } }
                                        else { Markdown { content: item.body.clone(), references: references.clone() } }
                                    }
                                }
                                for entry in review::timeline(&comments.comments, &comments.reviews) {
                                    {discussion_entry(repo, &url, page, entry, references)}
                                }
                            }
                            if let Some(limit) = comments.reviews_limit {
                                p { class: "collaboration-muted", "Showing the first {limit} reviews." }
                            }
                            if page > 1 || has_more {
                                nav { class: "repo-pagination", aria_label: "Discussion pages",
                                    if page > 1 { NavLink { class: "btn btn-secondary".to_owned(), href: format!("{url}?page={}", page - 1), "Previous" } }
                                    span { "Page {page}" }
                                    if has_more { NavLink { class: "btn btn-secondary".to_owned(), href: format!("{url}?page={}", page + 1), "Next" } }
                                }
                            }
                            if let Some(compare) = &item.comparison {
                                if kind == Kind::Pull {
                                    {merge_box(repo, &url, item, compare, default_method.as_deref())}
                                }
                                if let (true, Some(refused)) = (item.state != "open", draft.review.as_ref()) {
                                    {review::closed_review(refused)}
                                }
                            }
                            if repo.signed_in {
                                {review::composer(repo, &url, head, vote, draft, state_change.then_some(state_label.as_str()), references)}
                            } else { p { class: "collaboration-sign-in", NavLink { href: "/login", "Sign in" } " to join the discussion." } }
                            if state_change {
                                form { id: state_form, method: "post", action: format!("{url}/state"), hidden: true,
                                    {csrf(&repo.csrf)}
                                    input { r#type: "hidden", name: "expected_state", value: item.state.clone() }
                                    input { r#type: "hidden", name: "expected_updated", value: item.updated.clone() }
                                    input { r#type: "hidden", name: "state", value: if item.state == "open" { "closed" } else { "open" } }
                                }
                            }
                        }
                    }
                    aside { class: "collaboration-sidebar", aria_label: if kind == Kind::Issue { "Issue details" } else { "Pull request details" },
                        {assignees_panel(repo, &url, item, assignable)}
                        {labels_panel(repo, &url, item, available_labels, draft)}
                        {milestone_panel(repo, &url, item, milestones)}
                        if let Some(compare) = &item.comparison {
                            section { class: "collaboration-sidebar-section",
                                h3 { "Branches" }
                                dl { class: "collaboration-metadata",
                                    div { dt { "Into" } dd { NavLink { class: "collaboration-branch", href: file_url(&repo.url(), "tree", &compare.base, ""), "{compare.base}" } } }
                                    div { dt { "From" } dd { if let Some(href) = &compare.head_url { NavLink { class: "collaboration-branch", href: href.clone(), "{compare.head_label}" } } else { span { class: "collaboration-branch", "{compare.head_label}" } } } }
                                    div { dt { "Status" } dd { match item.state.as_str() { "merged" => "Merged", "closed" => "Closed", _ if compare.draft => "Draft", _ => "Open" } } }
                                }
                            }
                            section { class: "collaboration-sidebar-section",
                                h3 { "Revisions" }
                                dl { class: "collaboration-metadata",
                                    div { dt { "Head" } dd { code { title: compare.head_sha.clone(), "{compare.head_sha.get(..7).unwrap_or(&compare.head_sha)}" } } }
                                    div { dt { "Base" } dd { code { title: compare.base_sha.clone(), "{compare.base_sha.get(..7).unwrap_or(&compare.base_sha)}" } } }
                                }
                            }
                            {review::reviewers_panel(repo, &url, reviewers, &draft.reviewer, item.state == "open" && repo.can_write)}
                        }
                    }
                }
            }
        },
    )
}
