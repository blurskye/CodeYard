//! Recipient-scoped notification presentation and polling.
use crate::webui::components::{Avatar, DraftInput};
use crate::webui::encode_component;
use crate::webui::navigation::{NavLink, RefreshLink};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[component]
pub(crate) fn NotificationsView(model: NotificationsPage) -> Element {
    use_future(move || async move {
        #[cfg(target_arch = "wasm32")]
        loop {
            gloo_timers::future::TimeoutFuture::new(15000).await;
            crate::webui::navigation::refresh_page();
        }
    });
    notifications_page(model)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: i64,
    /// What happened, as a sentence such as "bob commented on issue #1".
    pub title: String,
    /// The issue, pull request or repository the notification is about. Rows lead with it.
    #[serde(default)]
    pub subject: Option<String>,
    /// The label of the reason the notification was sent for, such as "Comments".
    #[serde(default)]
    pub reason: Option<String>,
    pub body: Option<String>,
    pub link: Option<String>,
    pub read: bool,
    /// Kept for later.
    pub saved: bool,
    /// Marked done, so it is out of the inbox until it is undone.
    pub done: bool,
    pub created_at: String,
    /// The account that caused the notification, when it still exists.
    #[serde(default)]
    pub actor: Option<String>,
    /// The repository the notification is about, as owner/name, when it still exists.
    #[serde(default)]
    pub repository: Option<String>,
}

/// How many notifications each tab holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewCounts {
    pub inbox: i64,
    pub saved: i64,
    pub done: i64,
}

/// A repository the repository filter can name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryOption {
    pub id: i64,
    pub name: String,
}

/// A reason the reason filter can name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonOption {
    pub slug: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationsPage {
    pub csrf_token: String,
    pub notifications: Vec<Notification>,
    /// Unread notifications in the inbox: the count the unread badge shows.
    pub unread_count: i64,
    /// The tab shown: "inbox", "saved" or "done".
    pub view: String,
    pub counts: ViewCounts,
    pub repository: Option<i64>,
    pub repositories: Vec<RepositoryOption>,
    pub reason: Option<String>,
    pub reasons: Vec<ReasonOption>,
    pub query: Option<String>,
    pub unread_only: bool,
    /// The validated cursor of the page shown; its native forms carry it back.
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub older: Option<String>,
}

/// Stored notification URLs are not trusted navigation instructions.
pub fn safe_notification_link(link: &str) -> bool {
    let lower = link.to_ascii_lowercase();
    link.starts_with('/')
        && !link.starts_with("//")
        && !link.contains(['\\', '\r', '\n'])
        && !lower.contains("%5c")
        && !lower.contains("%0d")
        && !lower.contains("%0a")
        && !lower.starts_with("/%2f")
}

/// A Markdown body as plain text for a list row, such as a notification or a search result:
/// Markdown markup is dropped, the words are kept, and a long body is cut with an ellipsis.
pub fn body_preview(body: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut text = String::new();
    let mut after_table_row = false;
    for raw in body.lines() {
        // A fence line only opens or closes a code block, and its info string (`rust`) is not
        // text. The code inside the block stays, as words.
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            continue;
        }
        if is_rule_or_table_divider(raw) {
            continue;
        }
        let line = strip_block_markers(raw);
        let table_row = line.starts_with('|');
        let piece = if table_row {
            table_row_text(line)
        } else {
            strip_inline_markup(line)
        };
        let joined = is_list_item(raw) || (table_row && after_table_row);
        after_table_row = table_row;
        if piece.trim().is_empty() {
            continue;
        }
        if joined && !text.trim().is_empty() {
            text.push_str("· ");
        }
        text.push_str(&piece);
        text.push(' ');
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_CHARS {
        return collapsed;
    }
    let mut cut: String = collapsed.chars().take(MAX_CHARS - 1).collect();
    // Break at the last word boundary, unless the next character continues the word or the
    // first word alone is longer than the limit.
    if !collapsed[cut.len()..].starts_with(' ') {
        if let Some(space) = cut.rfind(' ') {
            cut.truncate(space);
        }
    }
    format!("{}…", cut.trim_end_matches(['·', ' ']))
}

/// A table row reads as its cells joined by " · ", so neighbouring cells do not run together.
fn table_row_text(line: &str) -> String {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| strip_inline_markup(cell).trim().to_owned())
        .filter(|cell| !cell.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A bullet, task or numbered list item. Its text is joined to the one before with " · ".
fn is_list_item(line: &str) -> bool {
    let line = line.trim_start();
    if ["- ", "* ", "+ "]
        .iter()
        .any(|marker| line.starts_with(*marker))
    {
        return true;
    }
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    digits > 0 && line[digits..].starts_with(['.', ')']) && line[digits + 1..].starts_with(' ')
}

/// Thematic breaks such as `---`, and the divider row of a table such as `|:--|-:|`.
fn is_rule_or_table_divider(line: &str) -> bool {
    let line = line.trim();
    if line.contains('|') {
        return line.trim_matches('|').split('|').all(|cell| {
            let cell = cell.trim();
            cell.contains('-') && cell.chars().all(|c| c == '-' || c == ':')
        });
    }
    line.chars().all(|c| "-*_= \t".contains(c))
        && line.chars().filter(|c| "-*_=".contains(*c)).count() >= 3
}

/// Quote, heading, list and task markers at the start of a line.
fn strip_block_markers(line: &str) -> &str {
    let mut line = line.trim_start();
    while let Some(rest) = line.strip_prefix('>') {
        line = rest.trim_start();
    }
    let heading = line.trim_start_matches('#');
    if heading.len() < line.len() && heading.starts_with(' ') {
        line = heading.trim_start();
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            line = rest.trim_start();
            break;
        }
    }
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 {
        if let Some(rest) = line[digits..]
            .strip_prefix(['.', ')'])
            .and_then(|rest| rest.strip_prefix(' '))
        {
            line = rest.trim_start();
        }
    }
    for task in ["[ ] ", "[x] ", "[X] "] {
        if let Some(rest) = line.strip_prefix(task) {
            line = rest;
            break;
        }
    }
    line
}

/// Link and image syntax keeps its text, so `[docs](https://example)` reads `docs`.
fn strip_inline_markup(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let (before, from_open) = rest.split_at(open);
        // The "!" of an image belongs to its link text.
        out.push_str(before.strip_suffix('!').unwrap_or(before));
        let inner = &from_open[1..];
        match inner
            .find("](")
            .and_then(|close| inner[close + 2..].find(')').map(|end| (close, end)))
        {
            Some((close, end)) => {
                out.push_str(&inner[..close]);
                rest = &inner[close + 2 + end + 1..];
            }
            None => {
                // With no "](" and ")" pair ahead, no later bracket can close a link either. The
                // rest is plain text, and stopping here keeps the scan linear.
                rest = from_open;
                break;
            }
        }
    }
    out.push_str(rest);
    // Emphasis, code and strikethrough marks go; `|` separates table cells.
    let mut plain = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    let mut previous: Option<char> = None;
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek().is_some_and(|next| next.is_ascii_punctuation()) => {
                plain.extend(chars.next());
            }
            '*' | '`' | '~' => {}
            '|' => plain.push(' '),
            // An underscore inside a word is part of the word, as in `snake_case`.
            '_' if !(previous.is_some_and(char::is_alphanumeric)
                && chars.peek().is_some_and(|next| next.is_alphanumeric())) => {}
            other => plain.push(other),
        }
        previous = Some(c);
    }
    plain
}

fn notification_date(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|date| {
            date.with_timezone(&chrono::Utc)
                .format("%b %-d, %Y at %H:%M UTC")
                .to_string()
        })
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .map(|date| date.format("%b %-d, %Y").to_string())
        })
        .unwrap_or_else(|_| "Date unavailable".to_owned())
}

/// The address of a notification list: its tab, its filters, the unread toggle and the page
/// position. Values at their defaults stay out of the query string, so the inbox is `/notifications`.
pub fn notifications_href(
    view: &str,
    repository: Option<i64>,
    reason: Option<&str>,
    query: Option<&str>,
    unread_only: bool,
    before: Option<&str>,
) -> String {
    let mut params = Vec::new();
    if view != "inbox" {
        params.push(format!("view={view}"));
    }
    if let Some(repository) = repository {
        params.push(format!("repo={repository}"));
    }
    if let Some(reason) = reason {
        params.push(format!("reason={}", encode_component(reason)));
    }
    if let Some(query) = query {
        params.push(format!("q={}", encode_component(query)));
    }
    if unread_only {
        params.push("unread_only=1".to_owned());
    }
    if let Some(before) = before {
        params.push(format!("before={}", encode_component(before)));
    }
    if params.is_empty() {
        "/notifications".to_owned()
    } else {
        format!("/notifications?{}", params.join("&"))
    }
}

pub fn notifications_page(model: NotificationsPage) -> Element {
    let refresh = notifications_href(
        &model.view,
        model.repository,
        model.reason.as_deref(),
        model.query.as_deref(),
        model.unread_only,
        model.before.as_deref(),
    );
    let filtered = model.repository.is_some() || model.reason.is_some() || model.query.is_some();
    let clear = notifications_href(&model.view, None, None, None, false, None);
    let unread_toggle = notifications_href(
        &model.view,
        model.repository,
        model.reason.as_deref(),
        model.query.as_deref(),
        !model.unread_only,
        None,
    );
    let every_repository = notifications_href(
        &model.view,
        None,
        model.reason.as_deref(),
        model.query.as_deref(),
        model.unread_only,
        None,
    );
    let every_reason = notifications_href(
        &model.view,
        model.repository,
        None,
        model.query.as_deref(),
        model.unread_only,
        None,
    );
    rsx! {
        div { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "mb-6 flex flex-wrap items-end justify-between gap-4",
                div { class: "space-y-1",
                    h1 { class: "text-2xl font-semibold tracking-tight", "Notifications" }
                    p { class: "text-sm text-muted-foreground",
                        if model.unread_count == 0 { "You're all caught up." } else if model.unread_count == 1 { "1 unread notification" } else { "{model.unread_count} unread notifications" }
                    }
                }
                div { class: "flex items-center gap-3", RefreshLink { href: refresh } }
            }
            nav { class: "notification-tabs", aria_label: "Notification views",
                {view_tab(&model, "inbox", "Inbox", model.counts.inbox)}
                {view_tab(&model, "saved", "Saved", model.counts.saved)}
                {view_tab(&model, "done", "Done", model.counts.done)}
            }
            div { class: "notification-toolbar",
                form { method: "get", action: "/notifications", "role": "search", class: "notification-search", aria_label: "Search notifications",
                    input { r#type: "hidden", name: "view", value: model.view.clone() }
                    if let Some(repository) = model.repository {
                        input { r#type: "hidden", name: "repo", value: "{repository}" }
                    }
                    if let Some(reason) = &model.reason {
                        input { r#type: "hidden", name: "reason", value: reason.clone() }
                    }
                    if model.unread_only {
                        input { r#type: "hidden", name: "unread_only", value: "1" }
                    }
                    // A draft, so the 15-second refresh does not overwrite text not yet searched.
                    DraftInput { name: "q", input_type: "search".to_string(), class: "notification-search-input".to_string(), placeholder: "Search notifications".to_string(), aria_label: Some("Search notifications".to_string()), initial_value: model.query.clone().unwrap_or_default() }
                    button { r#type: "submit", class: "btn btn-sm", "Search" }
                }
                div { class: "notification-toolbar-actions",
                    {filter_menu("Repository", model.repositories.iter().find(|repository| Some(repository.id) == model.repository).map(|repository| repository.name.clone()), rsx! {
                        {filter_option(every_repository, "All repositories", model.repository.is_none())}
                        for repository in &model.repositories {
                            {filter_option(notifications_href(&model.view, Some(repository.id), model.reason.as_deref(), model.query.as_deref(), model.unread_only, None), &repository.name, model.repository == Some(repository.id))}
                        }
                    })}
                    {filter_menu("Reason", model.reasons.iter().find(|reason| model.reason.as_deref() == Some(reason.slug.as_str())).map(|reason| reason.label.clone()), rsx! {
                        {filter_option(every_reason, "Any reason", model.reason.is_none())}
                        for reason in &model.reasons {
                            {filter_option(notifications_href(&model.view, model.repository, Some(reason.slug.as_str()), model.query.as_deref(), model.unread_only, None), &reason.label, model.reason.as_deref() == Some(reason.slug.as_str()))}
                        }
                    })}
                    NavLink { href: unread_toggle, class: if model.unread_only { "btn btn-sm is-current" } else { "btn btn-sm" }, aria_current: if model.unread_only { Some("true".to_owned()) } else { None }, mark_current: false,
                        "Unread"
                        if model.unread_count > 0 { span { class: "notification-count", "{model.unread_count}" } }
                    }
                    if filtered {
                        NavLink { href: clear, class: "notification-clear", mark_current: false, "Clear filters" }
                    }
                    if model.view != "done" && !model.notifications.is_empty() {
                        form { method: "post", action: "/notifications/done-all", class: "notification-bulk-form",
                            input { r#type: "hidden", name: "csrf_token", value: model.csrf_token.clone() }
                            {list_fields(&model, false)}
                            button { r#type: "submit", class: "btn btn-sm", "Mark all as done" }
                        }
                    }
                }
            }
            section { class: "panel", aria_label: "Notifications",
                if model.notifications.is_empty() {
                    div { class: "px-4 py-14 text-center",
                        h2 { class: "text-base font-semibold",
                            if filtered { "No notifications match these filters" } else if model.view == "saved" { "Nothing saved" } else if model.view == "done" { "Nothing marked done" } else if model.unread_only { "No unread notifications" } else { "Your inbox is empty" }
                        }
                        p { class: "mt-2 text-sm text-muted-foreground",
                            if filtered { "Clear a filter to see more." } else if model.view == "saved" { "Save a notification to keep it here for later." } else if model.view == "done" { "Notifications you mark as done appear here." } else if model.unread_only { "New activity will appear here." } else { "Updates from repositories and conversations will appear here." }
                        }
                    }
                }
                for notification in &model.notifications {
                    article { class: if is_unread(notification) { "notification-row is-unread" } else { "notification-row" },
                        span { class: "notification-dot", aria_label: if is_unread(notification) { "Unread" } else { "Read" } }
                        if let Some(actor) = &notification.actor {
                            Avatar { name: actor.clone(), size: 32 }
                        } else {
                            span { class: "notification-avatar-spacer", "aria-hidden": "true" }
                        }
                        div { class: "notification-main",
                            h2 { class: "notification-title",
                                if let Some(link) = notification.link.as_ref().filter(|link| safe_notification_link(link)) {
                                    // The link covers the whole row (see .notification-link::after).
                                    NavLink { href: link.clone(), class: "notification-link", "{headline(notification)}" }
                                } else { "{headline(notification)}" }
                            }
                            if let Some(preview) = notification.body.as_deref().map(body_preview).filter(|preview| !preview.is_empty()) {
                                p { class: "notification-preview", "{preview}" }
                            }
                            p { class: "notification-meta",
                                if let Some(repository) = &notification.repository {
                                    span { class: "notification-repo", "{repository}" }
                                    " · "
                                }
                                if let Some(reason) = &notification.reason {
                                    span { "{reason}" }
                                    " · "
                                }
                                if notification.subject.is_some() {
                                    span { "{notification.title}" }
                                    " · "
                                }
                                time { datetime: notification.created_at.clone(), "{notification_date(&notification.created_at)}" }
                            }
                        }
                        div { class: "notification-actions",
                            if is_unread(notification) {
                                form { method: "post", action: format!("/notifications/{}/read", notification.id), class: "notification-read-form",
                                    input { r#type: "hidden", name: "csrf_token", value: model.csrf_token.clone() }
                                    {list_fields(&model, true)}
                                    button { r#type: "submit", class: "btn btn-sm", "Mark read" }
                                }
                            }
                            form { method: "post", action: format!("/notifications/{}/save", notification.id), class: "notification-action-form",
                                input { r#type: "hidden", name: "csrf_token", value: model.csrf_token.clone() }
                                input { r#type: "hidden", name: "saved", value: if notification.saved { "0" } else { "1" } }
                                {list_fields(&model, true)}
                                button { r#type: "submit", class: "notification-action icon-tooltip", "aria-label": if notification.saved { "Unsave" } else { "Save" }, "aria-pressed": if notification.saved { "true" } else { "false" },
                                    {save_icon()}
                                }
                            }
                            form { method: "post", action: format!("/notifications/{}/done", notification.id), class: "notification-action-form",
                                input { r#type: "hidden", name: "csrf_token", value: model.csrf_token.clone() }
                                input { r#type: "hidden", name: "done", value: if notification.done { "0" } else { "1" } }
                                {list_fields(&model, true)}
                                button { r#type: "submit", class: "notification-action icon-tooltip", "aria-label": if notification.done { "Undo" } else { "Done" },
                                    {if notification.done { undo_icon() } else { check_icon() }}
                                }
                            }
                        }
                    }
                }
            }
            if let Some(older) = model.older.clone() {
                nav { class: "mt-4 flex justify-end", aria_label: "Notification history",
                    NavLink { href: older, class: "btn", "Older" }
                }
            }
        }
    }
}

/// One tab. The active tab is the current page; the others are plain links to their list.
fn view_tab(model: &NotificationsPage, name: &str, label: &str, count: i64) -> Element {
    let current = model.view == name;
    let href = notifications_href(
        name,
        model.repository,
        model.reason.as_deref(),
        model.query.as_deref(),
        model.unread_only,
        None,
    );
    rsx! {
        NavLink {
            href: href,
            class: if current { "notification-tab is-current" } else { "notification-tab" },
            aria_current: if current { Some("page".to_owned()) } else { None },
            mark_current: false,
            "{label}"
            span { class: "notification-count", "{count}" }
        }
    }
}

/// A filter picker: a button that names the chosen value, and a scrolling list of choices.
fn filter_menu(label: &str, chosen: Option<String>, options: Element) -> Element {
    let shown = match &chosen {
        Some(value) => format!("{label}: {value}"),
        None => label.to_owned(),
    };
    rsx! {
        details { class: "native-menu notification-filter-menu",
            summary { class: if chosen.is_some() { "btn btn-sm is-current" } else { "btn btn-sm" }, "{shown}" }
            div { class: "native-menu-panel notification-filter-panel", role: "group", aria_label: "{label}",
                div { class: "notification-filter-list", {options} }
            }
        }
    }
}

/// One choice of a filter picker. The chosen one has a check mark and is marked for assistive
/// technology.
fn filter_option(href: String, label: &str, chosen: bool) -> Element {
    rsx! {
        NavLink {
            href: href,
            class: "notification-filter-option",
            aria_current: if chosen { Some("true".to_owned()) } else { None },
            mark_current: false,
            span { class: "notification-filter-check", "aria-hidden": "true", if chosen { {check_icon()} } }
            "{label}"
        }
    }
}

/// A notification is unread only while it is in the inbox. A done one has left it, so it shows
/// no unread mark, and the header count leaves it out too.
fn is_unread(notification: &Notification) -> bool {
    !notification.read && !notification.done
}

/// The row's headline: the issue, pull request or repository, or the sentence when there is none.
fn headline(notification: &Notification) -> &str {
    notification
        .subject
        .as_deref()
        .unwrap_or(&notification.title)
}

/// The list's state as hidden fields, so a native form returns to the same list. The page
/// position is carried only by the forms that act on one row.
fn list_fields(model: &NotificationsPage, with_position: bool) -> Element {
    rsx! {
        input { r#type: "hidden", name: "view", value: model.view.clone() }
        if let Some(repository) = model.repository {
            input { r#type: "hidden", name: "repo", value: "{repository}" }
        }
        if let Some(reason) = &model.reason {
            input { r#type: "hidden", name: "reason", value: reason.clone() }
        }
        if let Some(query) = &model.query {
            input { r#type: "hidden", name: "q", value: query.clone() }
        }
        if model.unread_only {
            input { r#type: "hidden", name: "unread_only", value: "1" }
        }
        if with_position {
            if let Some(before) = &model.before {
                input { r#type: "hidden", name: "before", value: before.clone() }
            }
        }
    }
}

/// The stroke icons of the row actions, drawn like the shell's icons.
fn action_icon(paths: Element) -> Element {
    rsx! {
        svg { class: "notification-action-icon", view_box: "0 0 24 24", width: "18", height: "18", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true", "focusable": "false",
            {paths}
        }
    }
}

fn save_icon() -> Element {
    action_icon(rsx! {
        path { d: "m19 21-7-4-7 4V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v14z" }
    })
}

fn check_icon() -> Element {
    action_icon(rsx! {
        path { d: "M20 6 9 17l-5-5" }
    })
}

fn undo_icon() -> Element {
    action_icon(rsx! {
        path { d: "M9 14 4 9l5-5" }
        path { d: "M4 9h10.5a5.5 5.5 0 0 1 0 11H11" }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(id: i64) -> Notification {
        Notification {
            id,
            title: "<script>private title</script>".into(),
            subject: None,
            reason: None,
            body: Some("<private body>".into()),
            link: Some("//outside.invalid".into()),
            read: false,
            saved: false,
            done: false,
            created_at: "2026-10-02".into(),
            actor: Some("bob".into()),
            repository: Some("alice/proj".into()),
        }
    }

    /// The page inside a Dioxus scope, as the app renders it. The search box is a component with
    /// state, so it needs the scope that the app always provides.
    fn render_page(model: NotificationsPage) -> String {
        crate::webui::tests::in_scope(|| dioxus_ssr::render_element(notifications_page(model)))
    }

    fn page(notifications: Vec<Notification>) -> NotificationsPage {
        NotificationsPage {
            csrf_token: "inbox-token".into(),
            notifications,
            unread_count: 1,
            view: "inbox".into(),
            counts: ViewCounts {
                inbox: 1,
                saved: 0,
                done: 0,
            },
            repository: None,
            repositories: vec![],
            reason: None,
            reasons: vec![],
            query: None,
            unread_only: false,
            before: None,
            older: None,
        }
    }

    /// The part of the first element that carries `marker`, through its `close` tag.
    /// The element that holds `marker`: from its opening tag through the first `close` after it.
    fn element_holding<'a>(html: &'a str, marker: &str, open: &str, close: &str) -> &'a str {
        let at = html
            .find(marker)
            .unwrap_or_else(|| panic!("missing {marker}"));
        let start = html[..at].rfind(open).unwrap_or(0);
        let end = html[at..]
            .find(close)
            .map_or(html.len(), |end| at + end + close.len());
        &html[start..end]
    }

    fn element_at<'a>(html: &'a str, marker: &str, close: &str) -> &'a str {
        let at = html
            .find(marker)
            .unwrap_or_else(|| panic!("missing {marker}"));
        let end = html[at..]
            .find(close)
            .map_or(html.len(), |end| at + end + close.len());
        &html[at..end]
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn inbox_owner_keeps_native_forms_and_existing_serialized_variant() {
        let mut model = page(vec![notification(7)]);
        model.unread_only = true;
        model.before = Some("cursor".into());
        model.older = Some("/notifications?unread_only=1&before=cursor".into());
        model.repositories = vec![RepositoryOption {
            id: 3,
            name: "alice/proj".into(),
        }];
        let wrapped = crate::webui::pages::ci::Page::Notifications(model.clone());
        let json = serde_json::to_value(&wrapped).unwrap();
        assert_eq!(json["Notifications"], serde_json::to_value(&model).unwrap());
        assert_eq!(
            serde_json::from_value::<crate::webui::pages::ci::Page>(json).unwrap(),
            wrapped
        );
        let mut dom =
            VirtualDom::new_with_props(NotificationsView, NotificationsViewProps { model });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        for expected in [
            "action=\"/notifications/7/read\"",
            "action=\"/notifications/7/save\"",
            "action=\"/notifications/7/done\"",
            "action=\"/notifications/done-all\"",
            "name=\"csrf_token\"",
            "value=\"inbox-token\"",
            "name=\"unread_only\"",
            "Mark read",
            "Mark all as done",
            "Older",
            "before=cursor",
            "name=\"before\"",
            "value=\"cursor\"",
        ] {
            assert!(
                html.contains(expected),
                "Missing inbox contract: {expected}"
            );
        }
        assert!(!html.contains("//outside.invalid"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<private body>"));
        assert!(html.contains("&#60;script&#62;private title&#60;/script&#62;"));
        assert!(html.contains("&#60;private body&#62;"));
        assert!(!html.contains("class=\"repo-content "));
        assert!(html.contains("datetime=\"2026-10-02\""));
        assert!(html.contains("Oct 2, 2026"));
        assert!(!html.contains(">2026-10-02<"));
        assert!(!html.contains("recipient-scoped"));
        assert!(html.contains("src=\"/avatars/bob\""));
        assert!(html.contains("alice/proj"));
        // The external link is dropped, so the title is not a link at all.
        assert!(!html.contains("notification-link"));
    }

    #[test]
    fn a_safe_link_is_the_title_link_of_its_row() {
        let mut item = notification(1);
        item.title = "Reviewed".into();
        item.link = Some("/alice/repo/issues/1".into());
        item.read = true;
        item.actor = None;
        item.repository = None;
        let html = render_page(page(vec![item]));
        assert!(html.contains("class=\"notification-link\""));
        assert!(html.contains("href=\"/alice/repo/issues/1\""));
        assert!(!html.contains("/avatars/"));
    }

    #[test]
    fn tabs_show_their_counts_and_mark_the_current_view() {
        let mut model = page(vec![notification(7)]);
        model.view = "saved".into();
        model.counts = ViewCounts {
            inbox: 4,
            saved: 2,
            done: 9,
        };
        let html = render_page(model);
        let saved = element_at(&html, "notification-tab is-current", "</a>");
        assert!(saved.contains("Saved"), "{saved}");
        assert!(saved.contains("aria-current=\"page\""), "{saved}");
        assert!(saved.contains(">2</span>"), "{saved}");
        let inbox = element_at(&html, "href=\"/notifications\"", "</a>");
        assert!(inbox.contains(">4</span>"), "{inbox}");
        assert!(!inbox.contains("aria-current=\"page\""), "{inbox}");
        let done = element_at(&html, "href=\"/notifications?view=done\"", "</a>");
        assert!(done.contains(">9</span>"), "{done}");
        // The saved list can be cleared in one go, as the inbox can.
        assert!(html.contains("action=\"/notifications/done-all\""));
    }

    #[test]
    fn the_done_list_offers_undo_and_no_bulk_action() {
        let mut item = notification(7);
        item.done = true;
        let mut model = page(vec![item]);
        model.view = "done".into();
        let html = render_page(model);
        assert!(!html.contains("Mark all as done"));
        assert!(!html.contains("notification-bulk-form"));
        assert!(html.contains("aria-label=\"Undo\""));
    }

    #[test]
    fn a_done_notification_shows_no_unread_mark_or_mark_read_action() {
        let mut item = notification(7);
        item.done = true;
        let mut model = page(vec![item]);
        model.view = "done".into();
        let html = render_page(model);
        assert!(!html.contains("notification-row is-unread"), "{html}");
        assert!(html.contains("aria-label=\"Read\""), "{html}");
        assert!(!html.contains("action=\"/notifications/7/read\""), "{html}");
        assert!(!html.contains("Mark read"), "{html}");
    }

    #[test]
    fn rows_lead_with_the_subject_and_keep_the_sentence_and_reason_as_metadata() {
        let mut item = notification(7);
        item.title = "bob commented on issue #1".into();
        item.subject = Some("Crash when saving an empty file".into());
        item.reason = Some("Comments".into());
        item.read = true;
        let html = render_page(page(vec![item]));
        let title = element_at(&html, "class=\"notification-title\"", "</h2>");
        assert!(title.contains("Crash when saving an empty file"), "{title}");
        assert!(!title.contains("commented on issue"), "{title}");
        let meta = element_at(&html, "class=\"notification-meta\"", "</p>");
        assert!(meta.contains("alice/proj"), "{meta}");
        assert!(meta.contains("Comments"), "{meta}");
        assert!(meta.contains("bob commented on issue #1"), "{meta}");
    }

    #[test]
    fn repository_and_reason_are_two_pickers_and_the_chosen_choice_is_checked() {
        let mut model = page(vec![notification(7)]);
        model.repositories = vec![RepositoryOption {
            id: 3,
            name: "alice/proj".into(),
        }];
        model.repository = Some(3);
        model.reasons = vec![ReasonOption {
            slug: "comment".into(),
            label: "Comments".into(),
        }];
        let html = render_page(model);
        assert!(
            html.contains(
                "<summary class=\"btn btn-sm is-current\">Repository: alice/proj</summary>"
            ),
            "{html}"
        );
        assert!(
            html.contains("<summary class=\"btn btn-sm\">Reason</summary>"),
            "{html}"
        );
        assert!(html.contains("aria-label=\"Reason\""), "{html}");
        let repository_menu = element_at(&html, "aria-label=\"Repository\"", "</details>");
        let chosen = element_holding(
            repository_menu,
            "href=\"/notifications?repo=3\"",
            "<a",
            "</a>",
        );
        assert!(chosen.contains("aria-current=\"true\""), "{chosen}");
        assert!(chosen.contains("<svg"), "{chosen}");
    }

    #[test]
    fn row_actions_are_named_native_forms_that_say_their_state() {
        let html = render_page(page(vec![notification(7)]));
        let save = element_at(&html, "action=\"/notifications/7/save\"", "</form>");
        assert!(save.contains("name=\"saved\""), "{save}");
        assert!(save.contains("value=\"1\""), "{save}");
        let done = element_at(&html, "action=\"/notifications/7/done\"", "</form>");
        assert!(done.contains("name=\"done\""), "{done}");
        assert!(html.contains("aria-label=\"Save\""));
        assert!(html.contains("aria-label=\"Done\""));
        assert!(html.contains("aria-pressed=\"false\""));
        // The list does not clip, so the row actions' tooltips can show past its last row.
        assert!(html.contains("<section class=\"panel\" aria-label=\"Notifications\">"));

        let mut saved = notification(8);
        saved.saved = true;
        saved.done = true;
        let mut model = page(vec![saved]);
        model.view = "done".into();
        let html = render_page(model);
        assert!(html.contains("aria-label=\"Unsave\""));
        assert!(html.contains("aria-label=\"Undo\""));
        assert!(html.contains("aria-pressed=\"true\""));
        let undo = element_at(&html, "action=\"/notifications/8/done\"", "</form>");
        assert!(undo.contains("value=\"0\""), "{undo}");
    }

    #[test]
    fn the_search_box_is_labelled_and_shows_the_query() {
        let mut model = page(vec![notification(7)]);
        model.query = Some("build".into());
        let html = render_page(model);
        let search = element_at(&html, "notification-search-input", ">");
        assert!(
            search.contains("aria-label=\"Search notifications\""),
            "{search}"
        );
        assert!(search.contains("value=\"build\""), "{search}");
    }

    #[test]
    fn the_unread_toggle_keeps_its_pressed_state_inside_the_app() {
        // Inside the router, a link that is not marked as current must still keep its own state.
        let mut model = page(vec![notification(7)]);
        model.unread_only = true;
        let envelope = crate::webui::app::PageEnvelope {
            title: "Notifications".into(),
            location: "/notifications?unread_only=1".into(),
            chrome: crate::webui::document::Chrome::default(),
            page: crate::webui::app::Page::Actions(crate::webui::pages::ci::Page::Notifications(
                model,
            )),
        };
        let html = crate::webui::document::render_model(envelope, "/assets/app.js").unwrap();
        let at = html
            .find("class=\"btn btn-sm is-current\"")
            .expect("the unread toggle");
        let start = html[..at].rfind('<').unwrap();
        let tag = &html[start..at + html[at..].find('>').unwrap()];
        assert!(tag.contains("aria-current=\"true\""), "{tag}");
    }

    #[test]
    fn row_actions_carry_the_list_state_back_to_the_same_list() {
        let mut model = page(vec![notification(7)]);
        model.view = "saved".into();
        model.repository = Some(3);
        model.reason = Some("comment".into());
        model.query = Some("build".into());
        model.unread_only = true;
        model.before = Some("cursor".into());
        let html = render_page(model);
        let save = element_at(&html, "action=\"/notifications/7/save\"", "</form>");
        for expected in [
            "name=\"view\"",
            "value=\"saved\"",
            "name=\"repo\"",
            "value=\"3\"",
            "name=\"reason\"",
            "value=\"comment\"",
            "name=\"q\"",
            "value=\"build\"",
            "name=\"unread_only\"",
            "name=\"before\"",
        ] {
            assert!(save.contains(expected), "{expected}: {save}");
        }
    }

    #[test]
    fn list_addresses_keep_every_part_of_the_state() {
        assert_eq!(
            notifications_href("inbox", None, None, None, false, None),
            "/notifications"
        );
        assert_eq!(
            notifications_href("inbox", None, None, None, true, Some("abc")),
            "/notifications?unread_only=1&before=abc"
        );
        assert_eq!(
            notifications_href(
                "saved",
                Some(3),
                Some("comment"),
                Some("build log"),
                false,
                None
            ),
            "/notifications?view=saved&repo=3&reason=comment&q=build%20log"
        );
    }

    #[test]
    fn previews_show_words_without_markdown_markup() {
        assert_eq!(body_preview("**one nit**"), "one nit");
        assert_eq!(
            body_preview("Use `cargo test` and ~~not~~ _this_"),
            "Use cargo test and not this"
        );
        assert_eq!(
            body_preview("## Summary\n- first item\n> quoted line\n1. second"),
            "Summary · first item quoted line · second"
        );
        assert_eq!(
            body_preview("| Name | Status |\n|---|:--:|\n| build | ok |"),
            "Name · Status · build · ok"
        );
        assert_eq!(
            body_preview("See [the docs](https://example.test/x) and ![logo](/logo.png)"),
            "See the docs and logo"
        );
        assert_eq!(
            body_preview("@alice reviewed snake_case_names"),
            "@alice reviewed snake_case_names"
        );
        assert_eq!(body_preview("  \n\n  "), "");
    }

    #[test]
    fn long_previews_are_cut_on_a_character_boundary() {
        let short = body_preview(&"é".repeat(160));
        assert_eq!(short.chars().count(), 160);
        assert!(!short.ends_with('…'));
        let long = body_preview(&"word ".repeat(100));
        assert_eq!(long.chars().count(), 160);
        assert!(long.ends_with('…'));
        assert!(!long.contains("  "));
        assert!(!long[..long.len() - '…'.len_utf8()].ends_with(' '));
    }

    #[test]
    fn long_previews_break_at_a_word_boundary() {
        let cut = body_preview(&"abcdefghij ".repeat(20));
        assert_eq!(cut, format!("{}…", ["abcdefghij"; 14].join(" ")));
    }

    #[test]
    fn unmatched_brackets_stay_text_and_take_linear_time() {
        assert_eq!(body_preview("see [a [b (c"), "see [a [b (c");
        // Each unmatched bracket once searched the rest of the line, so this body took
        // quadratic time.
        let body = "[".repeat(200_000);
        assert_eq!(body_preview(&body), format!("{}…", "[".repeat(159)));
    }

    #[test]
    fn previews_drop_fence_info_strings_and_short_table_dividers() {
        assert_eq!(
            body_preview("Run this:\n```rust\nlet x = 1;\n```\ndone"),
            "Run this: let x = 1; done"
        );
        assert_eq!(
            body_preview("| Name | Status |\n|-|:-|\n| build | ok |"),
            "Name · Status · build · ok"
        );
    }

    #[test]
    fn notification_dates_are_stable_and_human_readable() {
        assert_eq!(notification_date("2026-10-02"), "Oct 2, 2026");
        assert_eq!(
            notification_date("2026-10-02T17:45:00+00:00"),
            "Oct 2, 2026 at 17:45 UTC"
        );
        assert_eq!(notification_date("not-a-date"), "Date unavailable");
    }

    #[test]
    fn notification_urls_reject_external_or_ambiguous_paths() {
        assert!(safe_notification_link("/alice/repo/actions?job=abc"));
        for link in [
            "https://evil.test",
            "//evil.test",
            "/\\evil",
            "/%2fevil",
            "/%5cevil",
            "/x%0d%0aLocation:x",
            "javascript:alert(1)",
        ] {
            assert!(!safe_notification_link(link), "{link}");
        }
    }
}
