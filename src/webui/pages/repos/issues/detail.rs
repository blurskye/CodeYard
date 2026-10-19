use super::{
    editor::{DetailEditForm, MarkdownEditor},
    label_style, merge_method_available, preselected_merge_method,
    reactions::{react_action, reaction_bar},
    Comment, CommentPage, Comparison, Event, Item, Kind, Label, Milestone, Repository, StackLayer,
    StackMap, MERGE_COMMIT_REFUSED,
};
use crate::webui::{
    components::{
        compact_date, confirm_dialog, count_label, current_time, references::References,
        relative_date, Avatar, Markdown,
    },
    encode_component,
    navigation::NavLink,
    pages::repos::pulls::review::{self, Entry, Reviewer},
    pages::repos::{self, ref_path::file_url},
};
use dioxus::prelude::*;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DetailDraft {
    pub title: Option<String>,
    pub body: Option<String>,
    pub comment: String,
    /// The comment being edited, with its text in `comment`. None when the comment box is in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editing: Option<i64>,
    pub label: String,
    pub preview: bool,
    /// The merge method that the submitted merge form chose, kept when the merge is refused.
    pub merge_method: String,
    /// The reviewer name a refused request was typed with, kept in the request form.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reviewer: String,
}

pub(in crate::webui::pages::repos) fn notice(error: Option<&str>) -> Element {
    rsx! { if let Some(error) = error { p { role: "alert", class: "collaboration-notice", "{error}" } } }
}
pub(in crate::webui::pages::repos) fn csrf(token: &str) -> Element {
    rsx! { input { r#type: "hidden", name: "csrf_token", value: token } }
}
/// One label chip. Lists, the sidebar and the labels page all show a label this way.
pub(in crate::webui::pages::repos) fn label_chip(label: &Label) -> Element {
    rsx! { span { class: "collaboration-label", style: label_style(&label.color), "{label.name}" } }
}
/// A label as a picker option: its colour dot and name, then what the label is for when it is written.
pub(super) fn label_option_text(label: &Label) -> Element {
    rsx! {
        span { class: "collaboration-picker-dot", style: label_style(&label.color), aria_hidden: "true" }
        span { class: "collaboration-picker-name", "{label.name}" }
        if !label.description.is_empty() { span { class: "collaboration-picker-description", "{label.description}" } }
    }
}
pub(super) fn labels(values: &[Label]) -> Element {
    rsx! { for label in values { {label_chip(label)} } }
}
/// A time as "3 days ago", with the exact date as its tooltip. Text that is not a time is shown as
/// it is.
pub(in crate::webui::pages::repos) fn ago(value: &str) -> Element {
    let text = relative_date(value, current_time()).unwrap_or_else(|| compact_date(value));
    rsx! { time { datetime: value.to_owned(), title: compact_date(value), "{text}" } }
}
/// The badge after a commenter's name: "Author" for the person who opened the item, "Owner" for
/// the repository owner. A deleted account gets none.
pub(super) fn role_badge(owner: &str, item_author: &str, author: &str) -> Option<&'static str> {
    if author == "Deleted user" {
        None
    } else if author == item_author {
        Some("Author")
    } else if author == owner {
        Some("Owner")
    } else {
        None
    }
}
/// The small icons of the sidebar, the filter menu caret and the comment count.
#[derive(Clone, Copy)]
pub(in crate::webui::pages::repos) enum Glyph {
    Gear,
    Caret,
    Check,
    Comment,
    Cross,
    Pending,
    Milestone,
    Pencil,
    Person,
    Tag,
}
pub(in crate::webui::pages::repos) fn glyph(icon: Glyph) -> Element {
    let shapes = match icon {
        Glyph::Gear => rsx! {
            circle { cx: "8", cy: "8", r: "5.25", stroke_width: "2.5", stroke_dasharray: "2 2.1" }
            circle { cx: "8", cy: "8", r: "2" }
        },
        Glyph::Caret => rsx! {
            path { d: "M4 6.5l4 4 4-4" }
        },
        Glyph::Check => rsx! {
            path { d: "M3.5 8.5l3 3 6-7" }
        },
        Glyph::Comment => rsx! {
            path { d: "M2.5 3.5h11v7H7l-3 2.5v-2.5h-1.5z" }
        },
        Glyph::Cross => rsx! {
            path { d: "M4 4l8 8M12 4l-8 8" }
        },
        Glyph::Pending => rsx! {
            circle { cx: "8", cy: "8", r: "5" }
        },
        Glyph::Milestone => rsx! {
            path { d: "M3.5 2v12M3.5 3h8.5l-2 2.5 2 2.5H3.5" }
        },
        Glyph::Pencil => rsx! {
            path { d: "M10.5 2.5l3 3L6 13l-3.5.5.5-3.5z" }
        },
        Glyph::Person => rsx! {
            circle { cx: "8", cy: "5.5", r: "2.75" }
            path { d: "M2.75 14c.5-2.6 2.5-4 5.25-4s4.75 1.4 5.25 4" }
        },
        Glyph::Tag => rsx! {
            path { d: "M2.5 2.5h5l6 6-5 5-6-6z" }
            circle { cx: "5.25", cy: "5.25", r: ".6", fill: "currentColor" }
        },
    };
    rsx! {
        svg { class: "collaboration-glyph", view_box: "0 0 16 16", width: "16", height: "16", fill: "none", stroke: "currentColor", stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true", "focusable": "false",
            {shapes}
        }
    }
}
pub(in crate::webui::pages::repos) fn author_link(username: &str) -> Element {
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
pub(crate) fn state_class(state: &str) -> &'static str {
    match state {
        "open" => "collaboration-state collaboration-state-open",
        "merged" => "collaboration-state collaboration-state-merged",
        _ => "collaboration-state collaboration-state-closed",
    }
}
/// The state icon of an issue or pull request. Open is green. A merged pull request and a closed
/// issue are done, so they are purple. A closed pull request is red, and a draft is grey.
pub(crate) fn item_icon(kind: Kind, state: &str) -> Element {
    let class = match (kind, state) {
        (_, "open") => "collaboration-item-icon collaboration-icon-open",
        (_, "merged") | (Kind::Issue, _) => "collaboration-item-icon collaboration-icon-merged",
        (_, "draft") => "collaboration-item-icon collaboration-icon-draft",
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
pub(in crate::webui::pages::repos) fn pull_summary(
    repo: &Repository,
    item: &Item,
    kind: Kind,
) -> Element {
    // A closed pull request is declined, which reads red; a closed issue reads purple like a merge.
    let state = if kind == Kind::Pull && item.state == "closed" {
        "collaboration-state collaboration-state-declined"
    } else {
        state_class(&item.state)
    };
    let comments = count_label(item.comment_count as usize, "comment", "comments");
    rsx! {
        div { class: "collaboration-detail-meta",
            span { class: state, {item_icon(kind, &item.state)} "{item.state}" }
            if let Some(stack) = &item.stack { span { class: "collaboration-muted", "{layer_position(stack)}" } }
            p { class: "collaboration-muted", {author_link(&item.author)} " opened this {kind.singular()} " {ago(&item.created)}
                if kind == Kind::Issue { span { class: "collaboration-nowrap", " · {comments}" } }
            }
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

/// The tab a pull request page is, so its tab is marked as the current page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::webui::pages::repos) enum PullTab {
    Conversation,
    Commits,
    Checks,
    Files,
}

/// The tabs of a pull request: Conversation, Commits, Checks and Files changed. `active` is the
/// page shown. The Conversation tab counts the comments, the Commits tab the commits, the Checks
/// tab the runs on the head commit and the Files changed tab the files.
pub(in crate::webui::pages::repos) fn pull_tabs(
    url: &str,
    item: &Item,
    active: PullTab,
) -> Element {
    let tab_class = |tab: PullTab| {
        if tab == active {
            "collaboration-detail-tab collaboration-detail-tab-active"
        } else {
            "collaboration-detail-tab"
        }
    };
    let tab_current = |tab: PullTab| if tab == active { "page" } else { "false" };
    // A count at the listing limit has more commits behind it, so it reads "100+".
    let commits = item.commit_count.map(|count| {
        if count as usize >= repos::pulls::MAX_LISTED_COMMITS {
            format!("{count}+")
        } else {
            count.to_string()
        }
    });
    // On a phone Commits and Checks move into the More menu, as the repository tabs do.
    let more_current = matches!(active, PullTab::Commits | PullTab::Checks);
    rsx! {
        div { class: "collaboration-detail-tab-bar",
            nav { class: "collaboration-detail-tabs", aria_label: "Pull request",
                NavLink { class: tab_class(PullTab::Conversation), href: url.to_owned(),
                    span { aria_current: tab_current(PullTab::Conversation), "Conversation" }
                    span { class: "collaboration-tab-count", "{item.comment_count}" }
                }
                div { class: "collaboration-detail-tabs-secondary",
                    NavLink { class: tab_class(PullTab::Commits), href: format!("{url}/commits"),
                        span { aria_current: tab_current(PullTab::Commits), "Commits" }
                        if let Some(count) = &commits { span { class: "collaboration-tab-count", "{count}" } }
                    }
                    NavLink { class: tab_class(PullTab::Checks), href: format!("{url}/checks"),
                        span { aria_current: tab_current(PullTab::Checks), "Checks" }
                        span { class: "collaboration-tab-count", "{item.check_count}" }
                    }
                }
                NavLink { class: tab_class(PullTab::Files), href: format!("{url}/diff"),
                    span { aria_current: tab_current(PullTab::Files), "Files changed" }
                    if let Some(count) = item.file_count { span { class: "collaboration-tab-count", "{count}" } }
                }
            }
            details { class: "native-menu collaboration-detail-tabs-more",
                summary { class: if more_current { "collaboration-detail-tab collaboration-detail-tab-active" } else { "collaboration-detail-tab" }, aria_label: "More pull request pages", title: "More pull request pages",
                    {repos::chrome::icon("more")}
                }
                div { class: "native-menu-panel",
                    div { class: "site-menu-links",
                        NavLink { class: "site-menu-link".to_owned(), href: format!("{url}/commits"), mark_current: false,
                            {repos::chrome::icon("commits")} span { "Commits" }
                        }
                        NavLink { class: "site-menu-link".to_owned(), href: format!("{url}/checks"), mark_current: false,
                            {repos::chrome::icon("check")} span { "Checks" }
                        }
                    }
                }
            }
        }
        repos::chrome::TabStripScroll { strip: ".collaboration-detail-tabs", path: url.to_owned() }
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
/// The heading of a sidebar row. A writer gets an Edit button that opens `menu`. Each option in the
/// menu is its own form, so one click applies that choice. Readers get the heading alone.
pub(in crate::webui::pages::repos) fn picker_heading(
    title: &str,
    menu: Option<Element>,
) -> Element {
    rsx! {
        div { class: "collaboration-sidebar-head",
            h3 { "{title}" }
            if let Some(menu) = menu {
                // A native menu, so a click outside it or Escape closes the picker.
                details { class: "native-menu collaboration-picker",
                    summary { class: "collaboration-picker-gear", title: format!("Change {title}"), aria_label: format!("Change {title}"), {glyph(Glyph::Gear)} }
                    div { class: "collaboration-picker-menu", {menu} }
                }
            }
        }
    }
}

/// The text typed in a picker's filter box. Each option reads it to show or hide itself.
#[derive(Clone, Copy)]
struct PickerFilter(Signal<String>);

/// The popover of a sidebar picker: its title, then a filter box and the options.
#[component]
pub(in crate::webui::pages::repos) fn PickerPopover(title: String, children: Element) -> Element {
    rsx! {
        div { class: "collaboration-picker-title", "{title}" }
        FilterBox { placeholder: "Filter", {children} }
    }
}

/// A list with a filter box above it. The box is added once the page runs, so without JavaScript
/// the list is shown whole. Each `PickerRow` in it hides itself when its text does not match.
#[component]
pub(in crate::webui::pages::repos) fn FilterBox(placeholder: String, children: Element) -> Element {
    let mut query = use_signal(String::new);
    let mut enhanced = use_signal(|| false);
    use_effect(move || enhanced.set(true));
    use_context_provider(|| PickerFilter(query));
    rsx! {
        if enhanced() {
            input { r#type: "search", class: "collaboration-input collaboration-picker-filter", placeholder: "{placeholder}", aria_label: "{placeholder}", value: query(), oninput: move |event| query.set(event.value()) }
        }
        {children}
    }
}

/// One row of a filtered list. It is hidden while the filter box holds text that `text` does not
/// contain.
#[component]
pub(in crate::webui::pages::repos) fn PickerRow(
    text: String,
    #[props(default)] class: String,
    children: Element,
) -> Element {
    let PickerFilter(query) = use_context();
    let filter = query().to_lowercase();
    let shown = filter.is_empty() || text.to_lowercase().contains(&filter);
    let class = if shown {
        class
    } else {
        format!("{class} collaboration-picker-hidden")
    };
    rsx! {
        li { class: "{class}", {children} }
    }
}

/// Assignees: the names, and for a writer a picker. `assignable` holds the users who may be
/// assigned; the picker lists them with the current assignees, so the viewer needs no other control.
fn assignees_panel(repo: &Repository, url: &str, item: &Item, assignable: &[String]) -> Element {
    let menu = repo
        .can_write
        .then(|| assignee_options(repo, url, item, assignable));
    rsx! {
        section { class: "collaboration-sidebar-section",
            {picker_heading("Assignees", menu)}
            if item.assignees.is_empty() { p { class: "collaboration-muted", "No one" } }
            ul { class: "collaboration-assignees",
                for name in &item.assignees {
                    li { key: "{name}", class: "collaboration-assignee", {author_link(name)} }
                }
            }
        }
    }
}

/// The assignee picker. A click on a current assignee removes it, and a click on anyone else adds
/// them.
fn assignee_options(repo: &Repository, url: &str, item: &Item, assignable: &[String]) -> Element {
    let addable: Vec<&String> = assignable
        .iter()
        .filter(|name| !item.assignees.contains(*name))
        .collect();
    // The viewer can take the item themselves when they may be assigned and are not assigned yet.
    let viewer = repo.viewer.as_str();
    let self_assign = !viewer.is_empty() && addable.iter().any(|name| name.as_str() == viewer);
    rsx! {
        PickerPopover { title: "Assign people to this issue",
            if self_assign {
                form { method: "post", action: format!("{url}/assignees"), class: "collaboration-picker-form",
                    {csrf(&repo.csrf)}
                    input { r#type: "hidden", name: "username", value: viewer.to_owned() }
                    button { r#type: "submit", class: "collaboration-picker-self", "Assign yourself" }
                }
            }
            if item.assignees.is_empty() && addable.is_empty() { p { class: "collaboration-muted", "No one to assign." } }
            ul { class: "collaboration-picker-list",
                for name in &item.assignees {
                    PickerRow { key: "{name}", text: name.clone(),
                        form { method: "post", action: format!("{url}/assignees/{}/remove", encode_component(name)), class: "collaboration-picker-form",
                            {csrf(&repo.csrf)}
                            button { r#type: "submit", class: "collaboration-picker-option", aria_pressed: "true",
                                span { class: "collaboration-picker-check", aria_hidden: "true", "✓" }
                                Avatar { name: name.clone(), size: 20 }
                                "{name}"
                            }
                        }
                    }
                }
                for name in addable {
                    PickerRow { key: "{name}", text: name.clone(),
                        form { method: "post", action: format!("{url}/assignees"), class: "collaboration-picker-form",
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
    }
}

/// Labels: the chips, and for an administrator a picker over every label of the repository. A
/// click adds a label, or removes it when the item already has it.
fn labels_panel(repo: &Repository, url: &str, item: &Item, available_labels: &[Label]) -> Element {
    let menu = repo
        .can_admin
        .then(|| label_options(repo, url, item, available_labels));
    rsx! {
        section { class: "collaboration-sidebar-section",
            {picker_heading("Labels", menu)}
            div { class: "collaboration-labels", {labels(&item.labels)} }
            if item.labels.is_empty() { p { class: "collaboration-muted", "None yet" } }
        }
    }
}

/// The label picker, with a link to the labels page where labels are created and changed.
fn label_options(repo: &Repository, url: &str, item: &Item, available_labels: &[Label]) -> Element {
    rsx! {
        PickerPopover { title: "Apply labels to this issue",
            if available_labels.is_empty() { p { class: "collaboration-muted", "No labels yet." } }
            ul { class: "collaboration-picker-list",
                for label in available_labels {
                    PickerRow { key: "{label.id}", text: label.name.clone(),
                        if item.labels.iter().any(|applied| applied.id == label.id) {
                            form { method: "post", action: format!("{url}/labels/{}/remove", label.id), class: "collaboration-picker-form",
                                {csrf(&repo.csrf)}
                                button { r#type: "submit", class: "collaboration-picker-option", aria_pressed: "true",
                                    span { class: "collaboration-picker-check", aria_hidden: "true", "✓" }
                                    {label_option_text(label)}
                                }
                            }
                        } else {
                            form { method: "post", action: format!("{url}/labels"), class: "collaboration-picker-form",
                                {csrf(&repo.csrf)}
                                input { r#type: "hidden", name: "label", value: label.id.to_string() }
                                button { r#type: "submit", class: "collaboration-picker-option", aria_pressed: "false",
                                    span { class: "collaboration-picker-check", aria_hidden: "true" }
                                    {label_option_text(label)}
                                }
                            }
                        }
                    }
                }
            }
            NavLink { class: "collaboration-manage-labels", href: format!("{}/labels", repo.url()), "Edit labels" }
        }
    }
}

/// The milestone: its title, and for a writer a picker over the repository's milestones. "No
/// milestone" clears it.
fn milestone_panel(repo: &Repository, url: &str, item: &Item, milestones: &[Milestone]) -> Element {
    let menu = (repo.can_write && !milestones.is_empty())
        .then(|| milestone_options(repo, url, item, milestones));
    rsx! {
        section { class: "collaboration-sidebar-section",
            {picker_heading("Milestone", menu)}
            if let Some(milestone) = &item.milestone {
                p { NavLink { class: "collaboration-title-link", href: format!("{}/milestones/{}", repo.url(), milestone.number), "{milestone.title}" } }
            } else { p { class: "collaboration-muted", "No milestone." } }
        }
    }
}

/// The milestone picker. The current milestone is marked, and a click sets another one.
fn milestone_options(
    repo: &Repository,
    url: &str,
    item: &Item,
    milestones: &[Milestone],
) -> Element {
    let current = item.milestone.as_ref().map(|milestone| milestone.number);
    rsx! {
        PickerPopover { title: "Set milestone",
            ul { class: "collaboration-picker-list",
                PickerRow { text: "No milestone".to_owned(),
                    form { method: "post", action: format!("{url}/milestone"), class: "collaboration-picker-form",
                        {csrf(&repo.csrf)}
                        input { r#type: "hidden", name: "milestone_number", value: "" }
                        button { r#type: "submit", class: "collaboration-picker-option", aria_pressed: if current.is_none() { "true" } else { "false" },
                            span { class: "collaboration-picker-check", aria_hidden: "true", if current.is_none() { "✓" } }
                            "No milestone"
                        }
                    }
                }
                for milestone in milestones {
                    PickerRow { key: "{milestone.number}", text: milestone.title.clone(),
                        form { method: "post", action: format!("{url}/milestone"), class: "collaboration-picker-form",
                            {csrf(&repo.csrf)}
                            input { r#type: "hidden", name: "milestone_number", value: milestone.number.to_string() }
                            button { r#type: "submit", class: "collaboration-picker-option", aria_pressed: if current == Some(milestone.number) { "true" } else { "false" },
                                span { class: "collaboration-picker-check", aria_hidden: "true", if current == Some(milestone.number) { "✓" } }
                                "{milestone.title}"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The people in the discussion: whoever wrote the item or one of the comments on the loaded page,
/// each once, in the order they first wrote. A deleted account has no profile, so it is left out.
fn participants<'a>(item: &'a Item, comments: &'a CommentPage) -> Vec<&'a str> {
    let mut names: Vec<&str> = Vec::new();
    let authors = std::iter::once(item.author.as_str()).chain(
        comments
            .comments
            .iter()
            .map(|comment| comment.author.as_str()),
    );
    for name in authors {
        if name != "Deleted user" && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The Participants section: a picture that links to each person's profile. The picture has no
/// text, so the name is the link's title and accessible name.
fn participants_panel(item: &Item, comments: &CommentPage) -> Element {
    let names = participants(item, comments);
    rsx! {
        section { class: "collaboration-sidebar-section",
            h3 { "Participants" }
            ul { class: "collaboration-participants",
                for name in names {
                    li { key: "{name}",
                        NavLink { class: "collaboration-participant", href: format!("/{}", encode_component(name)), title: Some(name.to_owned()), aria_label: Some(name.to_owned()),
                            Avatar { name: name.to_owned(), size: 24 }
                        }
                    }
                }
            }
        }
    }
}

/// One change in the discussion: who did what, in a single quiet line. A badge on the spine shows
/// the kind of change, so events read apart from comments.
fn event_row(event: &Event, kind: Kind) -> Element {
    let detail = event.detail.as_str();
    let (badge, label, what) = match event.kind.as_str() {
        "closed" => (item_icon(kind, "closed"), "Closed", rsx! { "closed this" }),
        "reopened" => (
            item_icon(kind, "open"),
            "Reopened",
            rsx! { "reopened this" },
        ),
        "merged" => (
            item_icon(Kind::Pull, "merged"),
            "Merged",
            rsx! { "merged this into " code { "{detail}" } },
        ),
        "renamed" => (
            glyph(Glyph::Pencil),
            "Renamed",
            rsx! { "changed the title " del { "{event.previous}" } " " ins { "{detail}" } },
        ),
        "labeled" => (
            glyph(Glyph::Tag),
            "Label added",
            rsx! { "added the " span { class: "collaboration-event-chip", "{detail}" } " label" },
        ),
        "unlabeled" => (
            glyph(Glyph::Tag),
            "Label removed",
            rsx! { "removed the " span { class: "collaboration-event-chip", "{detail}" } " label" },
        ),
        "assigned" => (
            glyph(Glyph::Person),
            "Assigned",
            rsx! { "assigned " NavLink { href: format!("/{}", encode_component(detail)), "{detail}" } },
        ),
        "unassigned" => (
            glyph(Glyph::Person),
            "Unassigned",
            rsx! { "unassigned " NavLink { href: format!("/{}", encode_component(detail)), "{detail}" } },
        ),
        "milestoned" => (
            glyph(Glyph::Milestone),
            "Milestone added",
            rsx! { "added this to the " strong { "{detail}" } " milestone" },
        ),
        "demilestoned" => (
            glyph(Glyph::Milestone),
            "Milestone removed",
            rsx! { "removed this from the " strong { "{detail}" } " milestone" },
        ),
        _ => (glyph(Glyph::Pending), "Update", rsx! { "updated this" }),
    };
    rsx! {
        div { class: "collaboration-event",
            span { class: "collaboration-event-icon", title: label.to_owned(), {badge} }
            {author_link(&event.actor)} " " {what} " " {ago(&event.created)}
        }
    }
}

/// One entry of the discussion: a review with its inline comments, a comment or a change.
fn discussion_entry(
    repo: &Repository,
    url: &str,
    page: usize,
    item_author: &str,
    entry: Entry,
    references: &References,
    kind: Kind,
) -> Element {
    match entry {
        Entry::Review(review) => {
            review::review_entry(repo, url, page, item_author, review, references)
        }
        Entry::Event(event) => event_row(event, kind),
        Entry::Comment(comment) => comment_entry(repo, url, page, item_author, comment, references),
    }
}

/// The edit and delete controls of a comment the viewer may change. Both open in place, so they
/// work without JavaScript. The server checks the same rights again when the form is sent.
fn comment_controls(
    repo: &Repository,
    url: &str,
    comment: &Comment,
    references: &References,
) -> Element {
    let id = comment.id;
    let action = format!("{url}/comments/{id}");
    let body = comment
        .edit
        .as_ref()
        .map_or_else(|| comment.body.clone(), |edit| edit.body.clone());
    let preview = comment.edit.as_ref().is_some_and(|edit| edit.preview);
    let open = comment.edit.is_some();
    rsx! {
        div { class: "collaboration-comment-controls",
            if comment.can_edit {
                details { class: "collaboration-comment-edit", open,
                    summary { class: "btn btn-secondary btn-sm", title: "Edit this comment", "Edit" }
                    form { method: "post", action: format!("{action}/edit"), class: "collaboration-comment-edit-form",
                        {csrf(&repo.csrf)}
                        MarkdownEditor { id: format!("edit-comment-{id}"), initial: body, rows: 8usize, required: true, initial_preview: preview, references: references.clone() }
                        div { class: "collaboration-form-actions",
                            button { class: "btn btn-primary", r#type: "submit", "Save changes" }
                        }
                    }
                }
            }
            if comment.can_delete {
                details { class: "native-menu repo-delete-menu",
                    summary { class: "btn btn-danger btn-sm", title: "Delete this comment", "Delete" }
                    {confirm_dialog(None, "Delete comment", &format!("{url}#comment-{id}"), &format!("{action}/delete"), rsx! {
                        {csrf(&repo.csrf)}
                        strong { "Delete this comment?" }
                        p { class: "collaboration-muted", "Its text is removed for everyone. This cannot be undone." }
                    }, "Delete comment")}
                }
            }
        }
    }
}

/// One comment as an article. A root comment on a file is wrapped in its thread anchor, which
/// resolve redirects use.
pub(in crate::webui::pages::repos) fn comment_entry(
    repo: &Repository,
    url: &str,
    page: usize,
    item_author: &str,
    comment: &Comment,
    references: &References,
) -> Element {
    // A writer or the author ticks the task boxes of a comment through its own form.
    let task_form = (comment.can_edit && comment.conversation.is_none())
        .then(|| format!("task-comment-{}", comment.id));
    let article = rsx! {
        article { id: format!("comment-{}", comment.id), class: "collaboration-panel collaboration-comment",
            header { class: "collaboration-panel-header", {author_link(&comment.author)} " commented " {ago(&comment.created)}
                if let Some(text) = role_badge(&repo.owner, item_author, &comment.author) { span { class: "collaboration-badge", "{text}" } }
                if let Some(conversation) = &comment.conversation { {review::conversation_badges(conversation)} }
                if let Some(edited) = &comment.edited { span { class: "collaboration-muted collaboration-edited", title: format!("Edited {}", compact_date(edited)), "edited" } }
            }
            // A thread's location links to the thread on the Files changed tab, where its lines are shown.
            if let Some(location) = &comment.location {
                p { class: "collaboration-comment-location",
                    if comment.conversation.is_some() { NavLink { href: format!("{url}/diff#thread-{}", comment.id), "{location}" } }
                    else { "{location}" }
                }
            }
            div { class: "collaboration-markdown", Markdown { content: comment.body.clone(), references: references.clone(), task_form: task_form.clone() } }
            if let Some(form_id) = &task_form {
                form { id: form_id.clone(), method: "post", action: format!("{url}/tasks"), hidden: true,
                    {csrf(&repo.csrf)}
                    input { r#type: "hidden", name: "comment", value: comment.id.to_string() }
                }
            }
            {reaction_bar(repo, &react_action(url, page), Some(("comment", comment.id)), &comment.reactions)}
            if comment.can_edit || comment.can_delete { {comment_controls(repo, url, comment, references)} }
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
/// The merge box under the discussion. A writer gets the status, the blockers and the method form
/// while the pull request is open. Everyone else gets a status line, and a pull request that has
/// ended gets a banner instead of a form.
fn merge_box(
    repo: &Repository,
    url: &str,
    item: &Item,
    compare: &Comparison,
    default_method: Option<&str>,
    reviewers: &[Reviewer],
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
    // The reviewers whose latest review asks for changes. Branch protection alone decides whether the
    // merge is allowed, so the box shows the open request next to that answer.
    let changes: Vec<&str> = reviewers
        .iter()
        .filter(|reviewer| reviewer.status == review::ReviewerStatus::ChangesRequested)
        .map(|reviewer| reviewer.username.as_str())
        .collect();
    let requested = format!("Changes requested by {}.", changes.join(", "));
    let policy = if blocked {
        "Branch protection blocks this merge until the blockers below are resolved."
    } else {
        "Branch protection still allows this merge."
    };
    // A green check means something only when a check ran or a review was asked for.
    let unverified = !blocked && !compare.has_checks && reviewers.is_empty();
    let failed = compare.failed_checks;
    let noun = if failed == 1 { "check" } else { "checks" };
    let (mark_class, mark) = if blocked || !changes.is_empty() || failed > 0 {
        ("collaboration-merge-mark collaboration-mark-changes", "×")
    } else if unverified || compare.checks_running {
        ("collaboration-merge-mark collaboration-mark-neutral", "–")
    } else {
        ("collaboration-merge-mark collaboration-mark-approved", "✓")
    };
    rsx! {
        div { class: "collaboration-panel collaboration-merge-box",
            div { class: "collaboration-merge-status",
                span { class: mark_class, aria_hidden: "true", "{mark}" }
                div {
                    h3 {
                        if blocked { span { "Merging is blocked" } }
                        else if !changes.is_empty() { span { "Changes requested" } }
                        else if failed > 0 { span { "Some checks were not successful" } }
                        else if unverified && compare.has_workflows {
                            NavLink { class: "collaboration-merge-checks", href: format!("{}/actions", repo.url()), "No checks configured" }
                        }
                        else if unverified { span { "No checks configured" } }
                        else if compare.checks_running { span { "Checks are still running" } }
                        else if compare.has_checks { span { "All checks have passed" } }
                        else { span { "Ready to merge" } }
                    }
                    p { class: "collaboration-muted",
                        if !changes.is_empty() { "{requested} {policy}" }
                        else if failed > 0 {
                            "{failed} {noun} failed. "
                            NavLink { class: "collaboration-merge-checks", href: format!("{url}/checks"), "View checks" }
                        }
                        else if unverified { "No reviews requested. Merging is still allowed." }
                        else { "Required reviews, status checks, and branch protection must pass before merging." }
                    }
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
                    if compare.merge_with.len() > 1 {
                        p { class: "collaboration-muted", "Merging this pull request lands {numbered(&compare.merge_with)} on {compare.merge_target} together." }
                    }
                    if compare.merge_commit_only {
                        p { class: "collaboration-muted", "Squash and rebase are unavailable while this pull request is stacked with open pull requests." }
                    }
                    if compare.merge_commit_refused {
                        p { class: "collaboration-muted", {MERGE_COMMIT_REFUSED} }
                    }
                    label { class: "collaboration-checkbox", input { r#type: "checkbox", name: "confirm", value: "merge", required: true } " Confirm merge into {compare.merge_target}" }
                }
                div { class: "collaboration-card-footer",
                    div { class: "collaboration-split-button",
                        button { r#type: "submit", class: "btn btn-primary", disabled: blocked || default_method.is_none(), "Merge pull request" }
                        // The caret opens the merge methods. They are radio inputs in this form, so
                        // the chosen one is sent with the merge.
                        details { class: "native-menu collaboration-split-menu",
                            summary { class: "btn btn-primary", title: "Choose the merge method", aria_label: "Choose the merge method", {glyph(Glyph::Caret)} }
                            div { class: "native-menu-panel collaboration-merge-methods",
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
                                }
                            }
                        }
                    }
                    button { r#type: "submit", form: "item-state-form", class: "btn btn-secondary", "Close pull request" }
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
    // The close or reopen form stays after the discussion. Its button sits in the composer footer
    // and submits that form through the form attribute, so the two forms never nest.
    let state_form = "item-state-form";
    let state_change = repo.can_write && (item.state == "open" || item.state == "closed");
    let state_label = if item.state == "open" {
        format!("Close {}", kind.singular())
    } else {
        format!("Reopen {}", kind.singular())
    };
    // An open pull request keeps Close in its merge box, so the composer offers only Reopen there.
    let composer_state = if kind == Kind::Pull && item.state == "open" && item.comparison.is_some()
    {
        None
    } else {
        state_change.then_some(state_label.as_str())
    };
    // The author cannot review their own pull request, so the reviewer picker leaves them out. The
    // server refuses such a request too.
    let requestable: Vec<String> = assignable
        .iter()
        .filter(|name| *name != &item.author)
        .cloned()
        .collect();
    repos::chrome::chrome(
        &repo.chrome,
        rsx! {
            section { class: "collaboration-page collaboration-detail",
                {notice(error)}
                header { class: "collaboration-detail-heading",
                    h2 { "{item.title} ", span { class: "collaboration-number", "#{item.number}" } }
                    if kind == Kind::Issue && repo.signed_in {
                        NavLink { class: "btn btn-primary collaboration-heading-action", href: format!("{}/new", repo.family_url(kind)), "New issue" }
                    }
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
                        {pull_tabs(&url, item, PullTab::Conversation)}
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
                            div { class: "collaboration-thread",
                                article { class: "collaboration-panel collaboration-comment",
                                    header { class: "collaboration-panel-header", {author_link(&item.author)} " opened " {ago(&item.created)}
                                        if let Some(text) = role_badge(&repo.owner, &item.author, &item.author) { span { class: "collaboration-badge", "{text}" } }
                                    }
                                    div { class: "collaboration-markdown",
                                        if item.body.trim().is_empty() { p { class: "collaboration-muted", "No description provided." } }
                                        else { Markdown { content: item.body.clone(), references: references.clone(), task_form: item.can_tick.then(|| "task-body".to_owned()) } }
                                    }
                                    if item.can_tick && !item.body.trim().is_empty() {
                                        form { id: "task-body", method: "post", action: format!("{url}/tasks"), hidden: true,
                                            {csrf(&repo.csrf)}
                                            input { r#type: "hidden", name: "expected_state", value: item.state.clone() }
                                            input { r#type: "hidden", name: "expected_updated", value: item.updated.clone() }
                                        }
                                    }
                                    {reaction_bar(repo, &react_action(&url, page), None, &item.reactions)}
                                }
                                for entry in review::timeline(&comments.comments, &comments.reviews, &comments.events) {
                                    {discussion_entry(repo, &url, page, &item.author, entry, references, kind)}
                                }
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
                                    {merge_box(repo, &url, item, compare, default_method.as_deref(), reviewers)}
                                }
                            }
                            if repo.signed_in {
                                {review::composer(repo, &url, head, draft, composer_state, references)}
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
                        {labels_panel(repo, &url, item, available_labels)}
                        {milestone_panel(repo, &url, item, milestones)}
                        if let Some(compare) = &item.comparison {
                            section { class: "collaboration-sidebar-section",
                                dl { class: "collaboration-metadata",
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
                            {review::reviewers_panel(repo, &url, reviewers, &requestable, &draft.reviewer, item.state == "open" && repo.can_write)}
                        }
                        {participants_panel(item, comments)}
                    }
                }
            }
        },
    )
}
