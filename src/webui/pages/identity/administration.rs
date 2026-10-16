//! Site administration account and activity presentation.
use super::*;
use crate::webui::components::{count_label, format_bytes, DraftSelect, SelectOption};

/// The daily table lists this many of the newest days. The chart always shows the whole window.
const TABLE_DAYS: usize = 30;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Admin {
    pub current_user_id: i64,
    pub accounts: Vec<Account>,
    pub activity: Vec<Activity>,
    pub days: i32,
    pub manage_access: bool,
    pub requests: Vec<Signup>,
    pub values: FormValues,
    /// `block` or `allow`.
    pub ai_crawlers: String,
}
pub fn normalize_days(raw: Option<&str>) -> i32 {
    raw.and_then(|s| s.parse().ok())
        .filter(|days| [7, 30, 90, 180, 365].contains(days))
        .unwrap_or(180)
}
/// Plot area of the activity chart, in its 800 x 260 viewBox.
const PLOT_LEFT: f64 = 48.0;
const PLOT_RIGHT: f64 = 792.0;
const PLOT_TOP: f64 = 12.0;
const PLOT_BOTTOM: f64 = 220.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Column {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One column per day, scaled to the busiest day. An all-zero window draws flat columns.
pub(super) fn columns(values: &[i64]) -> Vec<Column> {
    let peak = values.iter().copied().max().unwrap_or(0).max(1) as f64;
    let slot = (PLOT_RIGHT - PLOT_LEFT) / values.len().max(1) as f64;
    let width = (slot * 0.7).max(1.0);
    values
        .iter()
        .enumerate()
        .map(|(day, value)| {
            let height = (*value).max(0) as f64 / peak * (PLOT_BOTTOM - PLOT_TOP);
            Column {
                x: PLOT_LEFT + day as f64 * slot + (slot - width) / 2.0,
                y: PLOT_BOTTOM - height,
                width,
                height,
            }
        })
        .collect()
}

/// Totals for the cards: contributions, uploaded bytes, and days with any contribution.
pub(super) fn activity_totals(activity: &[Activity]) -> (i64, i64, usize) {
    (
        activity.iter().map(|point| point.contributions).sum(),
        activity.iter().map(|point| point.bytes).sum(),
        activity
            .iter()
            .filter(|point| point.contributions > 0)
            .count(),
    )
}

/// The AI crawler setting. A component, so its select runs inside the renderer's runtime.
#[component]
fn AiCrawlersPanel(token: String, value: String) -> Element {
    rsx! {
        section { class: "identity-admin-panel", aria_labelledby: "admin-crawlers-title",
            header { class: "identity-admin-panel-header",
                div { h2 { id: "admin-crawlers-title", "AI crawlers" } p { "Block AI training crawlers in robots.txt and ai.txt, and ask crawlers not to use HTML pages." } }
            }
            form { method: "post", action: "/admin/ai-crawlers", class: "identity-admin-crawlers",
                {csrf(&token)}
                label { r#for: "ai_crawlers", "AI crawlers" }
                DraftSelect { id: Some("ai_crawlers".to_owned()), name: "ai_crawlers", class: "border border-border bg-background",
                    initial_value: value,
                    options: vec![
                        SelectOption::new("block", "Block (default)"),
                        SelectOption::new("allow", "Allow"),
                    ] }
                button { r#type: "submit", class: "btn", "Save" }
            }
        }
    }
}

/// SSR offers a native GET form. Once hydrated, applying a timeframe uses the
/// same authorized URL through foundation navigation instead of a document load.
#[component]
pub(super) fn AdminTimeframe(days: i32) -> Element {
    let mut selected = use_signal(|| days);
    let mut ready = use_signal(|| false);
    use_effect(move || ready.set(true));
    rsx! {
        form { method: "get", action: "/admin", class: "flex items-center gap-3",
            label { r#for: "days", "Timeframe" }
            DraftSelect { id: Some("days".to_owned()), name: "days", class: "border border-border bg-background", initial_value: selected().to_string(),
                options: [7, 30, 90, 180, 365].into_iter().map(|days| SelectOption::new(days.to_string(), format!("Last {days} days"))).collect::<Vec<_>>(),
                on_value: move |value: String| selected.set(normalize_days(Some(&value))) }
            if ready() {
                NavLink { href: format!("/admin?days={}", selected()), class: "btn", "Apply" }
            } else { button { r#type: "submit", class: "btn", "Apply" } }
        }
    }
}

#[component]
fn BanAccountAction(account: Account, token: String, days: i32) -> Element {
    let action = format!("/admin/accounts/{}/ban", account.id);
    #[cfg(target_arch = "wasm32")]
    let dialog_id = format!("ban-account-{}", account.id);
    let title_id = format!("ban-account-title-{}", account.id);
    rsx! {
        a { href: format!("{action}?days={days}"), class: "text-destructive text-sm",
            onclick: move |event| {
                #[cfg(target_arch = "wasm32")]
                {
                    use wasm_bindgen::JsCast;
                    if let Some(dialog) = web_sys::window()
                        .and_then(|window| window.document())
                        .and_then(|document| document.get_element_by_id(&dialog_id))
                        .and_then(|element| element.dyn_into::<web_sys::HtmlDialogElement>().ok())
                    {
                        if dialog.show_modal().is_ok() {
                            event.prevent_default();
                        }
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = event;
            },
            "Ban account…"
        }
        dialog { id: format!("ban-account-{}", account.id), class: "admin-ban-dialog", "aria-labelledby": title_id.clone(),
            h2 { id: title_id, class: "text-lg font-semibold", "Ban {account.username}?" }
            p { class: "mt-3 text-sm", "Banning this account signs it out, revokes its tokens and stops its CI runs. Its repositories are kept, and unbanning restores access." }
            form { method: "post", action: action, class: "mt-6 flex flex-wrap justify-end gap-3",
                {csrf(&token)}
                input { r#type: "hidden", name: "banned", value: "true" }
                input { r#type: "hidden", name: "confirm_delete", value: "yes" }
                input { r#type: "hidden", name: "days", value: days.to_string() }
                button { r#type: "button", class: "rounded-md border border-border px-4 py-2",
                    onclick: move |_| {
                        #[cfg(target_arch = "wasm32")]
                        {
                            use wasm_bindgen::JsCast;
                            if let Some(dialog) = web_sys::window()
                                .and_then(|window| window.document())
                                .and_then(|document| document.get_element_by_id(&format!("ban-account-{}", account.id)))
                                .and_then(|element| element.dyn_into::<web_sys::HtmlDialogElement>().ok())
                            {
                                dialog.close();
                            }
                        }
                    },
                    "Cancel"
                }
                button { r#type: "submit", class: "rounded-md bg-destructive px-4 py-2 text-destructive-foreground",
                    "Ban account"
                }
            }
        }
    }
}

pub fn ban_confirmation_page(username: &str, user_id: i64, token: &str, days: i32) -> Element {
    rsx! {
        section { class: "page-shell max-w-5xl py-8 pb-14",
            header { class: "admin-header",
                div { class: "admin-header-main",
                    nav { class: "admin-crumbs", aria_label: "Breadcrumb",
                        NavLink { href: "/admin".to_string(), "Administration" }
                        " / "
                        NavLink { href: format!("/admin?days={days}#admin-accounts-title"), "Accounts" }
                        " / {username}"
                    }
                    h1 { "Ban {username}?" }
                    p { "Review what a ban does before you confirm it." }
                }
            }
            section { class: "panel", aria_label: "Confirm the ban",
                div { class: "panel-body",
                    p { "Banning this account signs it out, revokes its tokens and stops its CI runs. Its repositories are kept, and unbanning restores access." }
                    form { method: "post", action: format!("/admin/accounts/{user_id}/ban"), class: "admin-confirm-actions",
                        {csrf(token)}
                        input { r#type: "hidden", name: "banned", value: "true" }
                        input { r#type: "hidden", name: "confirm_delete", value: "yes" }
                        input { r#type: "hidden", name: "days", value: days.to_string() }
                        NavLink { href: format!("/admin?days={days}"), class: "btn", "Cancel" }
                        button { r#type: "submit", class: "btn btn-destructive", "Ban account" }
                    }
                }
            }
        }
    }
}

pub fn admin_page(model: Admin, token: &str, message: Option<&str>, error: bool) -> Element {
    let values: Vec<i64> = model
        .activity
        .iter()
        .map(|point| point.contributions)
        .collect();
    let day_columns = columns(&values);
    let top = values.iter().copied().max().unwrap_or(0);
    // Each tick is (value, line y, label y). The labels sit a little below their line.
    let ticks = [
        (top, PLOT_TOP, PLOT_TOP + 4.0),
        (
            top / 2,
            (PLOT_TOP + PLOT_BOTTOM) / 2.0,
            (PLOT_TOP + PLOT_BOTTOM) / 2.0 + 4.0,
        ),
        (0, PLOT_BOTTOM, PLOT_BOTTOM + 4.0),
    ];
    let ai_crawlers = model.ai_crawlers.clone();
    let (contributions, bytes, active_days) = activity_totals(&model.activity);
    let table_rows = &model.activity[model.activity.len().saturating_sub(TABLE_DAYS)..];
    let first_day = model
        .activity
        .first()
        .map_or("", |point| point.date.as_str());
    let last_day = model
        .activity
        .last()
        .map_or("", |point| point.date.as_str());
    rsx! {
        style { dangerous_inner_html: include_str!("profile.css") }
        div { class: "identity-settings identity-admin page-shell",
            header { class: "admin-header",
                div { class: "admin-header-main",
                    nav { class: "admin-crumbs", aria_label: "Breadcrumb",
                        NavLink { href: "/admin".to_string(), "Administration" }
                        " / Overview"
                    }
                    h1 { "Overview" }
                    p { "Monitor activity and manage accounts across this installation." }
                }
                nav { class: "admin-header-actions", aria_label: "Administration",
                    NavLink { href: "/admin/nodes".to_string(), class: "btn", "Nodes" }
                    NavLink { href: "/admin/storage".to_string(), class: "btn", "Storage" }
                }
            }
            {notice(message, error)}
            AiCrawlersPanel { token: token.to_owned(), value: ai_crawlers }
            section { class: "identity-admin-panel", aria_labelledby: "admin-activity-title",
                header { class: "identity-admin-panel-header",
                    div { h2 { id: "admin-activity-title", "Activity" } p { "Contributions and uploaded storage over time." } }
                    AdminTimeframe { key: "{model.days}", days: model.days }
                }
                if model.activity.is_empty() {
                    div { class: "identity-empty", h3 { "No activity yet" } p { "Activity for this timeframe will appear here." } }
                } else {
                    div { class: "identity-admin-totals",
                        div { class: "identity-admin-total", span { "Contributions" } strong { "{contributions}" } }
                        div { class: "identity-admin-total", span { "Storage uploaded" } strong { {format_bytes(bytes)} } }
                        div { class: "identity-admin-total", span { "Active days" } strong { "{active_days}" } }
                    }
                    div { class: "identity-admin-chart-scroll",
                        svg { view_box: "0 0 800 260", role: "img", "aria-label": "Daily contributions from {first_day} to {last_day}", class: "identity-admin-chart",
                            for (value, line_y, label_y) in ticks {
                                line { x1: PLOT_LEFT, x2: PLOT_RIGHT, y1: line_y, y2: line_y, class: "identity-admin-grid" }
                                text { x: PLOT_LEFT - 8.0, y: label_y, text_anchor: "end", class: "identity-admin-axis", "{value}" }
                            }
                            for column in day_columns {
                                rect { x: column.x, y: column.y, width: column.width, height: column.height, class: "identity-admin-bar" }
                            }
                            text { x: PLOT_LEFT, y: 254.0, class: "identity-admin-axis", "{first_day}" }
                            text { x: PLOT_RIGHT, y: 254.0, text_anchor: "end", class: "identity-admin-axis", "{last_day}" }
                        }
                    }
                    details { class: "identity-admin-daily-details",
                        summary { "Daily figures" }
                        div { class: "identity-table-wrap", table { class: "identity-table",
                            caption { class: "identity-sr-only", "Daily and cumulative activity" }
                            thead { tr { th { "Date" } th { class: "identity-num", "Contributions" } th { class: "identity-num", "Total" } th { class: "identity-num", "Storage" } th { class: "identity-num", "Storage total" } } }
                            tbody { for point in table_rows { tr {
                                td { "{point.date}" } td { class: "identity-num", "{point.contributions}" } td { class: "identity-num", "{point.cumulative_contributions}" }
                                td { class: "identity-num", {format_bytes(point.bytes)} } td { class: "identity-num", {format_bytes(point.cumulative_bytes)} }
                            } } }
                        } }
                        if model.activity.len() > TABLE_DAYS {
                            p { class: "identity-muted", "Latest {TABLE_DAYS} of {model.activity.len()} days. The chart shows every day." }
                        }
                    }
                }
            }
            section { class: "identity-admin-panel", aria_labelledby: "admin-accounts-title",
                header { class: "identity-admin-panel-header",
                    div { h2 { id: "admin-accounts-title", "All accounts" } p { {count_label(model.accounts.len(), "account", "accounts")} } }
                }
                div { class: "identity-admin-warning", "Banning an account signs it out, revokes its tokens and stops its CI runs. Its repositories are kept." }
                div { class: "identity-table-wrap", table { class: "identity-table identity-account-table",
                    thead { tr { th { "Account" } th { "Role" } th { "Status" } th { class: "identity-num", "Storage" } th { "Activity" } th { "Joined" } th { class: "identity-table-action", "Action" } } }
                    tbody { for account in &model.accounts { tr {
                        td { NavLink { href: format!("/{}", crate::webui::encode_component(&account.username)), class: "identity-account-name", "{account.username}" }
                            p { "{account.email}" }
                        }
                        td { if account.admin { "Admin" } else { "Member" } }
                        td {
                            span { class: if account.banned { "identity-status identity-status-danger" } else { "identity-status identity-status-success" }, if account.banned { "Banned" } else { "Active" } }
                            if account.must_change_password { p { "Password change required" } }
                        }
                        td { class: "identity-num", {format_bytes(account.bytes)} p { {count_label(account.files.max(0) as usize, "file", "files")} } }
                        td { {count_label(account.contributions.max(0) as usize, "contribution", "contributions")} p { {count_label(account.commits.max(0) as usize, "commit", "commits")} " · {account.pushes} push workflow runs" } }
                        td { "{account.created}" }
                        td { class: "identity-table-action",
                            if account.id == model.current_user_id {
                                span { class: "identity-muted", "You" }
                            } else if account.banned {
                                form { method: "post", action: format!("/admin/accounts/{}/ban", account.id),
                                    {csrf(token)}
                                    input { r#type: "hidden", name: "banned", value: "false" }
                                    input { r#type: "hidden", name: "days", value: model.days.to_string() }
                                    button { r#type: "submit", class: "identity-button", "Unban" }
                                }
                            } else {
                                BanAccountAction { account: account.clone(), token: token.to_owned(), days: model.days }
                            }
                        }
                    } } }
                } }
            }
            if model.manage_access {
                section { class: "identity-admin-panel identity-access-section", {access_forms(&model.requests, token, &model.values, "/admin/access")} }
            }
        }
    }
}
