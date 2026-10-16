//! Recipient-scoped inbox presentation and polling.
use crate::webui::components::Avatar;
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
    pub title: String,
    pub body: Option<String>,
    pub link: Option<String>,
    pub read: bool,
    pub created_at: String,
    /// The account that caused the notification, when it still exists.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationsPage {
    pub csrf_token: String,
    pub notifications: Vec<Notification>,
    pub unread_count: i64,
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

/// The body of a notification as plain text for the list: Markdown markup is dropped, the
/// words are kept, and a long body is cut with an ellipsis.
pub fn notification_preview(body: &str) -> String {
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
                out.push('[');
                rest = inner;
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

/// Refreshing an older page stays on it. The cursor is URL-safe base64.
fn refresh_href(unread_only: bool, before: Option<&str>) -> String {
    match (unread_only, before) {
        (true, Some(before)) => format!("/notifications?unread_only=1&before={before}"),
        (true, None) => "/notifications?unread_only=1".to_owned(),
        (false, Some(before)) => format!("/notifications?before={before}"),
        (false, None) => "/notifications".to_owned(),
    }
}

pub fn notifications_page(model: NotificationsPage) -> Element {
    let refresh = refresh_href(model.unread_only, model.before.as_deref());
    rsx! {
        div { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "mb-6 flex flex-wrap items-end justify-between gap-4",
                div { class: "space-y-1",
                    h1 { class: "text-2xl font-semibold tracking-tight", "Inbox" }
                    p { class: "text-sm text-muted-foreground",
                        if model.unread_count == 0 { "You're all caught up." } else if model.unread_count == 1 { "1 unread notification" } else { "{model.unread_count} unread notifications" }
                    }
                }
                div { class: "flex items-center gap-3", RefreshLink { href: refresh } }
            }
            div { class: "panel mb-4 flex flex-wrap items-center justify-between gap-3 p-2",
                form { method: "get", action: "/notifications", class: "flex items-center gap-2", aria_label: "Filter notifications",
                    button { r#type: "submit", class: if !model.unread_only { "rounded-md bg-accent px-3 py-2 text-sm font-medium" } else { "rounded-md px-3 py-2 text-sm text-muted-foreground hover:bg-accent" }, "All" }
                    button { r#type: "submit", name: "unread_only", value: "1", class: if model.unread_only { "rounded-md bg-accent px-3 py-2 text-sm font-medium" } else { "rounded-md px-3 py-2 text-sm text-muted-foreground hover:bg-accent" },
                        "Unread"
                        if model.unread_count > 0 { span { class: "notification-count", "{model.unread_count}" } }
                    }
                }
                form { method: "post", action: "/notifications/read-all",
                    input { r#type: "hidden", name: "csrf_token", value: model.csrf_token.clone() }
                    input { r#type: "hidden", name: "unread_only", value: if model.unread_only { "1" } else { "0" } }
                    button { r#type: "submit", class: "btn", "Mark all read" }
                }
            }
            section { class: "panel overflow-hidden", aria_label: "Notifications",
                if model.notifications.is_empty() {
                    div { class: "px-4 py-14 text-center",
                        h2 { class: "text-base font-semibold", if model.unread_only { "No unread notifications" } else { "Your inbox is empty" } }
                        p { class: "mt-2 text-sm text-muted-foreground", if model.unread_only { "New activity will appear here." } else { "Updates from repositories and conversations will appear here." } }
                    }
                }
                for notification in &model.notifications {
                    article { class: if notification.read { "notification-row" } else { "notification-row is-unread" },
                        span { class: "notification-dot", aria_label: if notification.read { "Read" } else { "Unread" } }
                        if let Some(actor) = &notification.actor {
                            Avatar { name: actor.clone(), size: 32 }
                        } else {
                            span { class: "notification-avatar-spacer", "aria-hidden": "true" }
                        }
                        div { class: "notification-main",
                            h2 { class: "notification-title",
                                if let Some(link) = notification.link.as_ref().filter(|link| safe_notification_link(link)) {
                                    // The link covers the whole row (see .notification-link::after).
                                    NavLink { href: link.clone(), class: "notification-link", "{notification.title}" }
                                } else { "{notification.title}" }
                            }
                            if let Some(preview) = notification.body.as_deref().map(notification_preview).filter(|preview| !preview.is_empty()) {
                                p { class: "notification-preview", "{preview}" }
                            }
                            p { class: "notification-meta",
                                time { datetime: notification.created_at.clone(), "{notification_date(&notification.created_at)}" }
                            }
                        }
                        if !notification.read {
                            form { method: "post", action: format!("/notifications/{}/read", notification.id), class: "notification-read-form",
                                input { r#type: "hidden", name: "csrf_token", value: model.csrf_token.clone() }
                                input { r#type: "hidden", name: "unread_only", value: if model.unread_only { "1" } else { "0" } }
                                if let Some(before) = &model.before {
                                    input { r#type: "hidden", name: "before", value: before.clone() }
                                }
                                button { r#type: "submit", class: "btn btn-sm", "Mark read" }
                            }
                        }
                    }
                }
            }
            if let Some(older) = model.older {
                nav { class: "mt-4 flex justify-end", aria_label: "Notification history",
                    NavLink { href: older, class: "btn", "Older" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn inbox_owner_keeps_native_forms_and_existing_serialized_variant() {
        let model = NotificationsPage {
            csrf_token: "inbox-token".into(),
            notifications: vec![Notification {
                id: 7,
                title: "<script>private title</script>".into(),
                body: Some("<private body>".into()),
                link: Some("//outside.invalid".into()),
                read: false,
                created_at: "2026-10-02".into(),
                actor: Some("bob".into()),
            }],
            unread_count: 1,
            unread_only: true,
            before: Some("cursor".into()),
            older: Some("/notifications?unread_only=true&before=cursor".into()),
        };
        let page = crate::webui::pages::actions::Page::Notifications(model.clone());
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["Notifications"], serde_json::to_value(&model).unwrap());
        assert_eq!(
            serde_json::from_value::<crate::webui::pages::actions::Page>(json).unwrap(),
            page
        );
        let mut dom =
            VirtualDom::new_with_props(NotificationsView, NotificationsViewProps { model });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        for expected in [
            "action=\"/notifications\"",
            "action=\"/notifications/read-all\"",
            "action=\"/notifications/7/read\"",
            "name=\"csrf_token\"",
            "value=\"inbox-token\"",
            "name=\"unread_only\"",
            "Mark all read",
            "Mark read",
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
        assert!(!html.contains("repo-chrome"));
        assert!(html.contains("datetime=\"2026-10-02\""));
        assert!(html.contains("Oct 2, 2026"));
        assert!(!html.contains(">2026-10-02<"));
        assert!(!html.contains("recipient-scoped"));
        assert!(html.contains("src=\"/avatars/bob\""));
        // The external link is dropped, so the title is not a link at all.
        assert!(!html.contains("notification-link"));
    }

    #[test]
    fn a_safe_link_is_the_title_link_of_its_row() {
        let page = |link: &str| NotificationsPage {
            csrf_token: "token".into(),
            notifications: vec![Notification {
                id: 1,
                title: "Reviewed".into(),
                body: None,
                link: Some(link.into()),
                read: true,
                created_at: "2026-10-02".into(),
                actor: None,
            }],
            unread_count: 0,
            unread_only: false,
            before: None,
            older: None,
        };
        let html = dioxus_ssr::render_element(notifications_page(page("/alice/repo/issues/1")));
        assert!(html.contains("class=\"notification-link\""));
        assert!(html.contains("href=\"/alice/repo/issues/1\""));
        assert!(!html.contains("/avatars/"));
    }

    #[test]
    fn previews_show_words_without_markdown_markup() {
        assert_eq!(notification_preview("**one nit**"), "one nit");
        assert_eq!(
            notification_preview("Use `cargo test` and ~~not~~ _this_"),
            "Use cargo test and not this"
        );
        assert_eq!(
            notification_preview("## Summary\n- first item\n> quoted line\n1. second"),
            "Summary · first item quoted line · second"
        );
        assert_eq!(
            notification_preview("| Name | Status |\n|---|:--:|\n| build | ok |"),
            "Name · Status · build · ok"
        );
        assert_eq!(
            notification_preview("See [the docs](https://example.test/x) and ![logo](/logo.png)"),
            "See the docs and logo"
        );
        assert_eq!(
            notification_preview("@alice reviewed snake_case_names"),
            "@alice reviewed snake_case_names"
        );
        assert_eq!(notification_preview("  \n\n  "), "");
    }

    #[test]
    fn long_previews_are_cut_on_a_character_boundary() {
        let short = notification_preview(&"é".repeat(160));
        assert_eq!(short.chars().count(), 160);
        assert!(!short.ends_with('…'));
        let long = notification_preview(&"word ".repeat(100));
        assert_eq!(long.chars().count(), 160);
        assert!(long.ends_with('…'));
        assert!(!long.contains("  "));
        assert!(!long[..long.len() - '…'.len_utf8()].ends_with(' '));
    }

    #[test]
    fn long_previews_break_at_a_word_boundary() {
        let cut = notification_preview(&"abcdefghij ".repeat(20));
        assert_eq!(cut, format!("{}…", ["abcdefghij"; 14].join(" ")));
    }

    #[test]
    fn previews_drop_fence_info_strings_and_short_table_dividers() {
        assert_eq!(
            notification_preview("Run this:\n```rust\nlet x = 1;\n```\ndone"),
            "Run this: let x = 1; done"
        );
        assert_eq!(
            notification_preview("| Name | Status |\n|-|:-|\n| build | ok |"),
            "Name · Status · build · ok"
        );
    }

    #[test]
    fn refresh_keeps_the_filter_and_the_older_page() {
        assert_eq!(refresh_href(false, None), "/notifications");
        assert_eq!(refresh_href(true, None), "/notifications?unread_only=1");
        assert_eq!(
            refresh_href(false, Some("abc")),
            "/notifications?before=abc"
        );
        assert_eq!(
            refresh_href(true, Some("abc")),
            "/notifications?unread_only=1&before=abc"
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
