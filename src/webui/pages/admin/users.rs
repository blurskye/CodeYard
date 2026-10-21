//! Administrator users page: pending signup requests, account creation, and the searchable
//! account list with its actions.
use super::nav::{admin_header, Section};
use super::SIGNUP_REQUESTS_PER_PAGE;
use crate::webui::components::{compact_date, confirm_dialog, count_label};
use crate::webui::navigation::NavLink;
use crate::webui::pages::fields::{csrf, managed_account_form, notice, FormValues, SUBMIT};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Signup {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub created: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AccountMetadata {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub admin: bool,
    pub banned: bool,
    pub must_change_password: bool,
    pub two_factor: bool,
    pub created: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdminUsers {
    /// A private node: it creates accounts for people and reviews their signup requests.
    pub managed_accounts: bool,
    pub current_user_id: i64,
    /// The rows on this page.
    pub accounts: Vec<AccountMetadata>,
    /// The search text, trimmed. Empty lists every account.
    pub search: String,
    pub page: usize,
    pub pages: usize,
    /// Accounts that match the search, on every page.
    pub total: usize,
    pub values: FormValues,
    /// The requests on this page. Empty on a public node.
    pub requests: Vec<Signup>,
    /// Pending requests on every page.
    pub requests_count: usize,
    pub requests_page: usize,
}

/// The users page address for a search, an account page and a request page. Pages after the
/// first keep their number.
fn list_url(search: &str, page: usize, requests_page: usize) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if !search.is_empty() {
        query.append_pair("q", search);
    }
    if page > 1 {
        query.append_pair("page", &page.to_string());
    }
    if requests_page > 1 {
        query.append_pair("request_page", &requests_page.to_string());
    }
    let query = query.finish();
    if query.is_empty() {
        "/admin/users".to_owned()
    } else {
        format!("/admin/users?{query}")
    }
}

/// The hidden fields that keep a row action on the same search and page.
fn listing_fields(search: &str, page: usize) -> Element {
    rsx! {
        input { r#type: "hidden", name: "q", value: search.to_owned() }
        input { r#type: "hidden", name: "page", value: page.to_string() }
    }
}

/// The per-row menu. Each choice is a link to a confirmation dialog, or a form that needs
/// no confirmation, so the menu works before scripts load.
#[component]
fn AccountActions(account: AccountMetadata, token: String, search: String, page: usize) -> Element {
    let id = account.id;
    let menu_label = format!("Actions for {}", account.username);
    let ban_id = format!("ban-account-{id}");
    let role_id = format!("role-account-{id}");
    let reset_id = format!("two-factor-account-{id}");
    let cancel = list_url(&search, page, 1);
    rsx! {
        details { class: "native-menu identity-row-menu",
            summary { class: "identity-row-menu-button", "aria-label": menu_label.clone(), title: menu_label,
                svg { "aria-hidden": "true", width: "16", height: "16", view_box: "0 0 24 24", fill: "currentColor",
                    circle { cx: "5", cy: "12", r: "2" }
                    circle { cx: "12", cy: "12", r: "2" }
                    circle { cx: "19", cy: "12", r: "2" }
                }
            }
            div { class: "identity-row-menu-panel",
                a { href: format!("#{role_id}"), title: if account.admin { "Remove the administrator role" } else { "Make this account an administrator" },
                    if account.admin { "Remove administrator" } else { "Make administrator" }
                }
                if account.banned {
                    form { method: "post", action: format!("/admin/accounts/{id}/ban"),
                        {csrf(&token)}
                        {listing_fields(&search, page)}
                        input { r#type: "hidden", name: "banned", value: "false" }
                        button { r#type: "submit", class: "identity-row-menu-action", title: "Restore this account's access", "Unban" }
                    }
                } else {
                    a { href: format!("#{ban_id}"), class: "identity-row-menu-danger", title: "Sign this account out and stop its access", "Ban account" }
                }
                if account.two_factor {
                    a { href: format!("#{reset_id}"), class: "identity-row-menu-danger", title: "Stop asking for authenticator codes, for an account that has lost its device", "Turn off two-factor" }
                }
            }
        }
        if account.two_factor {
            {confirm_dialog(Some(&reset_id), &format!("Turn off two-factor for {}", account.username), &cancel, &format!("/admin/accounts/{id}/two-factor/reset"), rsx! {
                {csrf(&token)}
                {listing_fields(&search, page)}
                p { class: "text-sm", "{account.username} will sign in with a password alone, and its recovery codes stop working. Use this only to recover a locked-out account." }
            }, "Turn off two-factor")}
        }
        if !account.banned {
        {confirm_dialog(Some(&ban_id), &format!("Ban {}", account.username), &cancel, &format!("/admin/accounts/{id}/ban"), rsx! {
            {csrf(&token)}
            {listing_fields(&search, page)}
            input { r#type: "hidden", name: "banned", value: "true" }
            input { r#type: "hidden", name: "confirm_delete", value: "yes" }
            p { class: "text-sm", "Banning {account.username} signs the account out, revokes its tokens and stops its CI runs. Its repositories are kept, and unbanning restores access." }
        }, "Ban account")}
        }
        {confirm_dialog(Some(&role_id), if account.admin { "Remove administrator" } else { "Make administrator" }, &cancel, &format!("/admin/accounts/{id}/role"), rsx! {
            {csrf(&token)}
            {listing_fields(&search, page)}
            input { r#type: "hidden", name: "admin", value: if account.admin { "false" } else { "true" } }
            p { class: "text-sm",
                if account.admin {
                    "{account.username} will lose the administrator role and its access to this page. The account itself is kept."
                } else {
                    "{account.username} will be able to manage every account and setting on this node."
                }
            }
        }, if account.admin { "Remove administrator" } else { "Make administrator" })}
    }
}

pub fn admin_users_page(
    model: AdminUsers,
    token: &str,
    message: Option<&str>,
    error: bool,
) -> Element {
    let search = model.search.clone();
    let page = model.page;
    let requests_page = model.requests_page;
    let request_pages = model
        .requests_count
        .div_ceil(SIGNUP_REQUESTS_PER_PAGE)
        .max(1);
    let previous = (page > 1).then(|| list_url(&search, page - 1, requests_page));
    let next = (page < model.pages).then(|| list_url(&search, page + 1, requests_page));
    let previous_requests = (requests_page > 1).then(|| list_url(&search, page, requests_page - 1));
    let next_requests =
        (requests_page < request_pages).then(|| list_url(&search, page, requests_page + 1));
    rsx! {
        div { class: "identity-settings identity-admin page-shell",
            {admin_header(Section::Users, "Users", "Find accounts, review signup requests, ban accounts and grant the administrator role.")}
            {notice(message, error)}
            if model.managed_accounts {
                section { class: "identity-admin-panel", aria_labelledby: "admin-requests-title",
                    header { class: "identity-admin-panel-header",
                        div { h2 { id: "admin-requests-title", "Pending requests" } p { {count_label(model.requests_count, "request", "requests")} " waiting for a decision." } }
                    }
                    if model.requests.is_empty() {
                        div { class: "identity-empty", h3 { "No pending requests" } p { "New requests will appear here." } }
                    } else {
                        div { class: "identity-row-list",
                            for request in model.requests.iter() {
                                article { key: "{request.id}", class: "identity-row",
                                    div { class: "identity-row-main",
                                        h3 { "{request.username}" }
                                        p { "{request.email} · Requested {compact_date(&request.created)}" }
                                    }
                                    form { method: "post", action: format!("/admin/users/requests/{}/resolve", request.id), class: "identity-row-actions",
                                        {csrf(token)}
                                        {listing_fields(&search, page)}
                                        button { r#type: "submit", name: "action", value: "approve", class: SUBMIT, "Approve" }
                                        button { r#type: "submit", name: "action", value: "reject", class: "btn", "Reject" }
                                    }
                                }
                            }
                        }
                    }
                    if request_pages > 1 {
                        nav { class: "identity-admin-pager", aria_label: "Signup request pages",
                            if let Some(href) = previous_requests { NavLink { href, class: "btn", "Previous requests" } }
                            span { class: "identity-muted", "Page {requests_page} of {request_pages}" }
                            if let Some(href) = next_requests { NavLink { href, class: "btn", "Next requests" } }
                        }
                    }
                }
                section { class: "identity-admin-panel", aria_labelledby: "admin-create-title",
                    header { class: "identity-admin-panel-header",
                        div { h2 { id: "admin-create-title", "Create user" } p { "The new account signs in with the temporary password and must change it." } }
                    }
                    {managed_account_form(token, &model.values, "/admin/users")}
                }
            }
            section { class: "identity-admin-panel", aria_labelledby: "admin-accounts-title",
                header { class: "identity-admin-panel-header",
                    div { h2 { id: "admin-accounts-title", "Accounts" } p { {count_label(model.total, "account", "accounts")} } }
                }
                form { method: "get", action: "/admin/users", role: "search", class: "identity-admin-search",
                    label { r#for: "admin-user-search", class: "identity-sr-only", "Search accounts" }
                    input { id: "admin-user-search", name: "q", r#type: "search", value: search.clone(), placeholder: "Search by username or email", class: "identity-admin-search-input" }
                    button { r#type: "submit", class: "btn", "Search" }
                }
                if model.accounts.is_empty() {
                    div { class: "identity-empty", h3 { "No accounts match" } p { "Try a different search." } }
                } else {
                    div { class: "identity-table-wrap", table { class: "identity-table identity-account-table",
                        thead { tr { th { "Account" } th { "Role" } th { "Status" } th { "Joined" } th { class: "identity-table-action", "Action" } } }
                        tbody { for account in model.accounts.clone() { tr {
                            td { NavLink { href: format!("/{}", crate::webui::encode_component(&account.username)), class: "identity-account-name", "{account.username}" }
                                p { "{account.email}" }
                            }
                            td { "data-label": "Role", if account.admin { "Administrator" } else { "Member" } }
                            td { "data-label": "Status",
                                span { class: if account.banned { "identity-status identity-status-danger" } else { "identity-status identity-status-success" }, if account.banned { "Banned" } else { "Active" } }
                                if account.must_change_password { p { "Password change required" } }
                            }
                            td { "data-label": "Joined", {compact_date(&account.created)} }
                            td { class: "identity-table-action",
                                if account.id == model.current_user_id {
                                    span { class: "identity-muted", "You" }
                                } else {
                                    AccountActions { account: account.clone(), token: token.to_owned(), search: search.clone(), page }
                                }
                            }
                        } } }
                    } }
                }
                if model.pages > 1 {
                    nav { class: "identity-admin-pager", aria_label: "Account pages",
                        if let Some(href) = previous { NavLink { href, class: "btn", "Previous" } }
                        span { class: "identity-muted", "Page {page} of {model.pages}" }
                        if let Some(href) = next { NavLink { href, class: "btn", "Next" } }
                    }
                }
            }
        }
    }
}
