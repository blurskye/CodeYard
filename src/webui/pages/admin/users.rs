//! Administrator users page: pending signup requests, account creation, and the searchable
//! account list with its actions.
use super::nav::{admin_layout, Section};
use super::SIGNUP_REQUESTS_PER_PAGE;
use crate::webui::components::{compact_date, confirm_dialog, count_label};
use crate::webui::navigation::NavLink;
use crate::webui::pages::fields::{csrf, managed_account_form, notice, FormValues};
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
    /// The role filter: `admins`, `banned`, or empty for every account.
    pub role: String,
    /// Accounts that match the search: all of them, the administrators and the banned ones.
    pub counts: [usize; 3],
    pub page: usize,
    pub pages: usize,
    /// Accounts that match the search and the filter, on every page.
    pub total: usize,
    /// The requests on this page. Empty on a public node.
    pub requests: Vec<Signup>,
    /// Pending requests on every page.
    pub requests_count: usize,
    pub requests_page: usize,
}

/// Where the users page is: the search, the role filter, the account page and the request page.
#[derive(Clone, PartialEq)]
struct Listing {
    search: String,
    role: String,
    page: usize,
    requests_page: usize,
}

impl Listing {
    /// The users page address. Pages after the first keep their number.
    fn url(&self) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if !self.search.is_empty() {
            query.append_pair("q", &self.search);
        }
        if !self.role.is_empty() {
            query.append_pair("role", &self.role);
        }
        if self.page > 1 {
            query.append_pair("page", &self.page.to_string());
        }
        if self.requests_page > 1 {
            query.append_pair("request_page", &self.requests_page.to_string());
        }
        let query = query.finish();
        if query.is_empty() {
            "/admin/users".to_owned()
        } else {
            format!("/admin/users?{query}")
        }
    }

    fn with(&self, change: impl FnOnce(&mut Listing)) -> String {
        let mut listing = self.clone();
        change(&mut listing);
        listing.url()
    }

    /// The hidden fields that bring a row action back to the same list.
    fn fields(&self) -> Element {
        rsx! {
            input { r#type: "hidden", name: "q", value: self.search.clone() }
            input { r#type: "hidden", name: "role", value: self.role.clone() }
            input { r#type: "hidden", name: "page", value: self.page.to_string() }
        }
    }
}

/// The per-row menu. Each choice is a link to a confirmation dialog, or a form that needs
/// no confirmation, so the menu works before scripts load.
#[component]
fn AccountActions(account: AccountMetadata, token: String, listing: Listing) -> Element {
    let id = account.id;
    let menu_label = format!("Actions for {}", account.username);
    let ban_id = format!("ban-account-{id}");
    let role_id = format!("role-account-{id}");
    let reset_id = format!("two-factor-account-{id}");
    let cancel = listing.url();
    let role_action = if account.admin {
        "Remove administrator"
    } else {
        "Make administrator"
    };
    rsx! {
        details { class: "native-menu identity-row-menu",
            summary { class: "identity-row-menu-button", "aria-label": menu_label.clone(), title: menu_label,
                svg { "aria-hidden": "true", width: "16", height: "16", view_box: "0 0 24 24", fill: "currentColor",
                    circle { cx: "5", cy: "12", r: "2" }
                    circle { cx: "12", cy: "12", r: "2" }
                    circle { cx: "19", cy: "12", r: "2" }
                }
            }
            div { class: "native-menu-panel identity-row-menu-panel",
                div { class: "site-menu-links",
                    a { class: "site-menu-link", href: format!("#{role_id}"), "{role_action}" }
                    if account.banned {
                        form { method: "post", action: format!("/admin/accounts/{id}/ban"),
                            {csrf(&token)}
                            {listing.fields()}
                            input { r#type: "hidden", name: "banned", value: "false" }
                            button { r#type: "submit", class: "site-menu-link", "Unban" }
                        }
                    }
                    if !account.banned || account.two_factor {
                        hr { class: "site-menu-divider" }
                    }
                    if account.two_factor {
                        a { class: "site-menu-link identity-menu-danger", href: format!("#{reset_id}"), "Turn off two-factor" }
                    }
                    if !account.banned {
                        a { class: "site-menu-link identity-menu-danger", href: format!("#{ban_id}"), "Ban account" }
                    }
                }
            }
        }
        if account.two_factor {
            {confirm_dialog(Some(&reset_id), &format!("Turn off two-factor for {}", account.username), &cancel, &format!("/admin/accounts/{id}/two-factor/reset"), rsx! {
                {csrf(&token)}
                {listing.fields()}
                p { class: "text-sm", "{account.username} will sign in with a password alone, and its recovery codes stop working. Use this only to recover a locked-out account." }
            }, "Turn off two-factor")}
        }
        if !account.banned {
            {confirm_dialog(Some(&ban_id), &format!("Ban {}", account.username), &cancel, &format!("/admin/accounts/{id}/ban"), rsx! {
                {csrf(&token)}
                {listing.fields()}
                input { r#type: "hidden", name: "banned", value: "true" }
                input { r#type: "hidden", name: "confirm_delete", value: "yes" }
                p { class: "text-sm", "Banning {account.username} signs the account out, revokes its tokens and stops its CI runs. Its repositories are kept, and unbanning restores access." }
            }, "Ban account")}
        }
        {confirm_dialog(Some(&role_id), role_action, &cancel, &format!("/admin/accounts/{id}/role"), rsx! {
            {csrf(&token)}
            {listing.fields()}
            input { r#type: "hidden", name: "admin", value: if account.admin { "false" } else { "true" } }
            p { class: "text-sm",
                if account.admin {
                    "{account.username} will lose the administrator role and its access to this page. The account itself is kept."
                } else {
                    "{account.username} will be able to manage every account and setting on this node."
                }
            }
        }, role_action)}
    }
}

pub fn admin_users_page(
    model: AdminUsers,
    token: &str,
    message: Option<&str>,
    error: bool,
) -> Element {
    let listing = Listing {
        search: model.search.clone(),
        role: model.role.clone(),
        page: model.page,
        requests_page: model.requests_page,
    };
    let page = model.page;
    let requests_page = model.requests_page;
    let request_pages = model
        .requests_count
        .div_ceil(SIGNUP_REQUESTS_PER_PAGE)
        .max(1);
    let previous = (page > 1).then(|| listing.with(|l| l.page -= 1));
    let next = (page < model.pages).then(|| listing.with(|l| l.page += 1));
    let previous_requests = (requests_page > 1).then(|| listing.with(|l| l.requests_page -= 1));
    let next_requests =
        (requests_page < request_pages).then(|| listing.with(|l| l.requests_page += 1));
    let filters = [
        ("", "All"),
        ("admins", "Administrators"),
        ("banned", "Banned"),
    ]
    .into_iter()
    .zip(model.counts)
    .map(|((role, label), count)| {
        let href = listing.with(|l| {
            l.role = role.to_owned();
            l.page = 1;
        });
        (role, label, count, href)
    })
    .collect::<Vec<_>>();
    let empty = if !model.search.is_empty() {
        ("No accounts match", "Try a different search.")
    } else if model.role == "banned" {
        ("No banned accounts", "Banned accounts will appear here.")
    } else {
        ("No accounts", "Accounts will appear here.")
    };
    admin_layout(
        Section::Users,
        "Users",
        "Every account on this installation. Open a row's menu to change its role or ban it.",
        rsx! {
            {notice(message, error)}
            if model.managed_accounts && model.requests_count > 0 {
                section { class: "identity-admin-panel", aria_labelledby: "admin-requests-title",
                    header { class: "identity-admin-panel-header",
                        div { h2 { id: "admin-requests-title", "Pending requests" } p { {count_label(model.requests_count, "request", "requests")} " waiting for a decision." } }
                    }
                    div { class: "identity-row-list",
                        for request in model.requests.iter() {
                            article { key: "{request.id}", class: "identity-row",
                                div { class: "identity-row-main",
                                    h3 { "{request.username}" }
                                    p { "{request.email} · Requested {compact_date(&request.created)}" }
                                }
                                form { method: "post", action: format!("/admin/users/requests/{}/resolve", request.id), class: "identity-row-actions",
                                    {csrf(token)}
                                    {listing.fields()}
                                    button { r#type: "submit", name: "action", value: "approve", class: "btn btn-sm btn-primary", "Approve" }
                                    button { r#type: "submit", name: "action", value: "reject", class: "btn btn-sm", "Reject" }
                                }
                            }
                        }
                    }
                    if request_pages > 1 {
                        nav { class: "identity-admin-pager", aria_label: "Signup request pages",
                            if let Some(href) = previous_requests { NavLink { href, class: "btn btn-sm", "Previous requests" } }
                            span { class: "identity-muted", "Page {requests_page} of {request_pages}" }
                            if let Some(href) = next_requests { NavLink { href, class: "btn btn-sm", "Next requests" } }
                        }
                    }
                }
            }
            section { class: "identity-admin-panel", aria_label: "Accounts",
                div { class: "identity-admin-toolbar",
                    form { method: "get", action: "/admin/users", role: "search", class: "identity-admin-search",
                        label { r#for: "admin-user-search", class: "identity-sr-only", "Search accounts" }
                        if !model.role.is_empty() {
                            input { r#type: "hidden", name: "role", value: model.role.clone() }
                        }
                        input { id: "admin-user-search", name: "q", r#type: "search", value: model.search.clone(), placeholder: "Search by username or email", class: "identity-admin-search-input" }
                        button { r#type: "submit", class: "btn", "Search" }
                    }
                    if model.managed_accounts {
                        NavLink { href: "/admin/users/new", class: "btn btn-primary", "New user" }
                    }
                }
                div { class: "identity-admin-listbox",
                    nav { class: "identity-admin-filters", aria_label: "Show accounts",
                        for (role, label, count, href) in filters {
                            NavLink { key: "{role}", href, class: "identity-admin-filter", aria_current: (model.role == role).then(|| "page".to_owned()),
                                "{label}"
                                span { class: "identity-admin-filter-count", "{count}" }
                            }
                        }
                    }
                    if model.accounts.is_empty() {
                        div { class: "identity-empty", h3 { "{empty.0}" } p { "{empty.1}" } }
                    } else {
                        table { class: "identity-table identity-account-table",
                            thead { tr { th { "Account" } th { "Role" } th { "Status" } th { "Joined" } th { class: "identity-table-action", span { class: "identity-sr-only", "Actions" } } } }
                            tbody { for account in model.accounts.clone() { tr { key: "{account.id}",
                                td { NavLink { href: format!("/{}", crate::webui::encode_component(&account.username)), class: "identity-account-name", "{account.username}" }
                                    if !account.email.is_empty() { p { "{account.email}" } }
                                }
                                td { "data-label": "Role", if account.admin { "Administrator" } else { "Member" } }
                                td { "data-label": "Status",
                                    span { class: if account.banned { "identity-status identity-status-danger" } else { "identity-status identity-status-success" }, if account.banned { "Banned" } else { "Active" } }
                                    if account.two_factor { p { "Two-factor on" } }
                                    if account.must_change_password { p { "Must change password" } }
                                }
                                td { "data-label": "Joined", {compact_date(&account.created)} }
                                td { class: "identity-table-action",
                                    if account.id == model.current_user_id {
                                        span { class: "identity-muted", "You" }
                                    } else {
                                        AccountActions { account: account.clone(), token: token.to_owned(), listing: listing.clone() }
                                    }
                                }
                            } } }
                        }
                    }
                }
                if model.pages > 1 {
                    nav { class: "identity-admin-pager", aria_label: "Account pages",
                        if let Some(href) = previous { NavLink { href, class: "btn btn-sm", "Previous" } }
                        span { class: "identity-muted", "Page {page} of {model.pages}" }
                        if let Some(href) = next { NavLink { href, class: "btn btn-sm", "Next" } }
                    }
                }
            }
        },
    )
}

/// The form that creates an account on a private node. A refused form comes back here with
/// what was typed.
pub fn admin_new_user_page(
    values: FormValues,
    token: &str,
    message: Option<&str>,
    error: bool,
) -> Element {
    admin_layout(
        Section::Users,
        "New user",
        "Create an account for someone. They sign in with the temporary password and then choose their own.",
        rsx! {
            {notice(message, error)}
            section { class: "identity-admin-panel", aria_label: "New user",
                {managed_account_form(token, &values, "/admin/users")}
            }
        },
    )
}
