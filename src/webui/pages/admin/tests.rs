//! Admin page contracts: the timeframe control, the overview, the users page, site settings and the chart.
use super::*;
use crate::webui::components::charts::{columns, normalize_days, timeframe_links, DailyActivity};
use crate::webui::pages::fields::FormValues;
use crate::webui::pages::settings::settings_tab;
use dioxus::prelude::*;

fn html(element: Element) -> String {
    dioxus_ssr::render_element(element)
}

#[test]
fn timeframe_choices_are_plain_links_with_the_current_one_marked() {
    let output = html(timeframe_links("/admin", 30));
    for days in [7, 30, 90, 180] {
        assert!(output.contains(&format!("href=\"/admin?days={days}\"")));
    }
    assert!(!output.contains("href=\"/admin?days=365\""));
    assert_eq!(output.matches("aria-current=\"page\"").count(), 1);
    assert!(!output.contains("<form"));
}

fn account(id: i64, username: &str, admin: bool, banned: bool) -> AccountMetadata {
    AccountMetadata {
        id,
        username: username.into(),
        email: format!("{username}@example.test"),
        admin,
        banned,
        must_change_password: false,
        two_factor: false,
        created: "today".into(),
    }
}

fn day(date: &str, commits: i64, pulls_merged: i64, storage_bytes: i64) -> (DailyActivity, i64) {
    let point = DailyActivity {
        date: date.into(),
        commits,
        pulls_merged,
        ..Default::default()
    };
    (point, storage_bytes)
}

fn overview(days_seen: Vec<(DailyActivity, i64)>, days: i32) -> Admin {
    let (activity, storage_bytes) = days_seen.into_iter().unzip();
    Admin {
        pending_requests: None,
        activity,
        storage_bytes,
        days,
        version: "0.1.2".into(),
    }
}

#[test]
fn overview_charts_each_kind_of_activity_without_counters() {
    let activity = vec![day("2026-10-01", 4, 1, 2048), day("2026-10-02", 1, 0, 0)];
    let output = html(admin_page(overview(activity, 7)));
    for text in [
        ">Commits pushed</h3>",
        ">Pull requests opened</h3>",
        ">Pull requests merged</h3>",
        ">Issues opened</h3>",
        ">Issues closed</h3>",
        ">Workflow runs</h3>",
        ">Storage added</h3>",
        // Window totals, the peak and the dates at the ends.
        "<span>5 commits</span>",
        "<span>1 pull request</span>",
        "<span>2.0 KiB</span>",
        ">Oct 1</span>",
        ">Oct 2</span>",
        // Each column names its day and value on hover.
        "<title>Oct 1: 4 commits</title>",
        "<title>Oct 2: 1 commit</title>",
        "aria-label=\"Commits pushed, Oct 1 to Oct 2: 5 commits\"",
        "runs Codeyard 0.1.2.",
    ] {
        assert!(output.contains(text), "{text} missing in {output}");
    }
    // A quiet series says so instead of drawing empty columns.
    assert_eq!(output.matches("None in this period").count(), 4);
    assert_eq!(output.matches("column-chart-wide").count(), 1);
    // Counts with their own page and every action stay off the overview.
    for text in [
        "Recent signups",
        "Banned",
        "Storage used",
        "<form",
        "<table",
    ] {
        assert!(!output.contains(text), "{text} in {output}");
    }
    assert!(output.contains("class=\"identity-settings-nav\""));
}

#[test]
fn waiting_signup_requests_are_the_only_notice() {
    let mut model = overview(vec![day("2026-10-01", 0, 0, 0)], 30);
    assert!(!html(admin_page(model.clone())).contains("identity-admin-attention"));
    model.pending_requests = Some(0);
    assert!(!html(admin_page(model.clone())).contains("identity-admin-attention"));
    model.pending_requests = Some(2);
    let output = html(admin_page(model));
    assert!(
        output.contains("2 sign-up requests are waiting for a decision."),
        "{output}"
    );
    let notice = output.find("identity-admin-attention").unwrap();
    assert!(notice < output.find("admin-activity-title").unwrap());
}

fn users_model(
    accounts: Vec<AccountMetadata>,
    search: &str,
    page: usize,
    pages: usize,
) -> AdminUsers {
    AdminUsers {
        managed_accounts: true,
        current_user_id: 1,
        accounts,
        search: search.into(),
        role: String::new(),
        counts: [30, 2, 1],
        page,
        pages,
        total: 30,
        requests: vec![],
        requests_count: 0,
        requests_page: 1,
    }
}

#[test]
fn users_page_actions_confirm_bans_and_role_changes_in_dialogs() {
    let model = users_model(
        vec![
            account(42, "bob", false, false),
            account(43, "carol", true, false),
        ],
        "",
        1,
        1,
    );
    let output = html(admin_users_page(model, "csrf-marker", None, false));
    // The member gets a ban dialog and a make-admin dialog; the administrator gets remove-admin.
    assert!(output.contains("id=\"ban-account-42\""));
    assert!(output.contains("action=\"/admin/accounts/42/ban\""));
    assert!(output.contains("name=\"confirm_delete\" value=\"yes\""));
    assert!(output.contains("id=\"role-account-42\""));
    assert!(output.contains("action=\"/admin/accounts/42/role\""));
    assert!(output.contains("Make administrator"));
    assert!(output.contains("Remove administrator"));
    assert!(output.contains("action=\"/admin/accounts/43/role\""));
    assert!(output.contains("name=\"admin\" value=\"false\""));
    assert!(output.contains("name=\"csrf_token\" value=\"csrf-marker\""));
}

#[test]
fn users_page_keeps_search_filter_and_page_on_row_actions_and_links_the_new_user_form() {
    let mut model = users_model(vec![account(42, "bob", true, true)], "bo", 2, 3);
    model.role = "banned".into();
    let output = html(admin_users_page(model, "csrf-marker", None, false));
    assert!(output.contains("name=\"q\" value=\"bo\""));
    assert!(output.contains("name=\"role\" value=\"banned\""));
    assert!(output.contains("name=\"page\" value=\"2\""));
    assert!(output.contains("href=\"/admin/users?q=bo&#38;role=banned&#38;page=3\""));
    // The filters keep the search and start again at the first page.
    assert!(output.contains("href=\"/admin/users?q=bo\""));
    assert!(output.contains("href=\"/admin/users?q=bo&#38;role=admins\""));
    assert!(output.contains("Administrators<span class=\"identity-admin-filter-count\">2</span>"));
    assert_eq!(
        output.matches("aria-current=\"page\"").count(),
        3,
        "{output}"
    );
    // A banned account is restored by a form that needs no confirmation.
    assert!(output.contains("name=\"banned\" value=\"false\""));
    assert!(!output.contains("id=\"ban-account-42\""));
    // The row menu is a dropdown panel, and new accounts have their own page.
    assert!(output.contains("native-menu-panel identity-row-menu-panel"));
    assert!(output.contains("href=\"/admin/users/new\""));
    assert!(!output.contains("name=\"is_admin\""));
    assert!(output.contains("Page 2 of 3"));
}

#[test]
fn new_user_page_keeps_what_was_typed() {
    let values = FormValues {
        username: "dave".into(),
        ..FormValues::default()
    };
    let output = html(admin_new_user_page(
        values,
        "csrf-marker",
        Some("Password is too short."),
        true,
    ));
    assert!(output.contains("action=\"/admin/users\""));
    assert!(output.contains("value=\"dave\""));
    assert!(output.contains("name=\"is_admin\""));
    assert!(output.contains("Password is too short."));
    assert!(output.contains("<h1>New user</h1>"));
}

#[test]
fn users_page_lists_pending_requests_with_approve_and_reject() {
    let mut model = users_model(vec![account(42, "bob", false, false)], "bo", 2, 3);
    model.requests = vec![Signup {
        id: 8,
        username: "carol".into(),
        email: "carol@example.test".into(),
        created: "2026-10-02".into(),
    }];
    model.requests_count = 61;
    model.requests_page = 2;
    let output = html(admin_users_page(model, "csrf-marker", None, false));
    assert!(output.contains("carol@example.test"));
    assert!(output.contains("action=\"/admin/users/requests/8/resolve\""));
    assert!(output.contains("name=\"action\" value=\"approve\""));
    assert!(output.contains("name=\"action\" value=\"reject\""));
    assert!(output.contains("61 requests waiting for a decision."));
    assert!(output.contains("Page 2 of 3"));
    // The request pager keeps the search and the account page.
    assert!(output.contains("Previous requests"));
    assert!(output.contains("Next requests"));
    assert!(output.contains("request_page=3"));
}

#[test]
fn users_page_without_requests_leaves_the_request_list_out() {
    let output = html(admin_users_page(
        users_model(vec![], "", 1, 1),
        "csrf-marker",
        None,
        false,
    ));
    assert!(!output.contains("Pending requests"));
    assert!(!output.contains("Next requests"));
    assert!(output.contains("No accounts"));
}

#[test]
fn public_users_page_does_not_offer_managed_account_creation() {
    let mut model = users_model(vec![], "", 1, 1);
    model.managed_accounts = false;
    let output = html(admin_users_page(model, "csrf", None, false));
    assert!(!output.contains("method=\"post\" action=\"/admin/users\""));
    assert!(!output.contains("/admin/users/new"));
    assert!(!output.contains("Pending requests"));
}

#[test]
fn users_page_without_matches_says_so() {
    let model = users_model(vec![], "zzz", 1, 1);
    let output = html(admin_users_page(model, "csrf-marker", None, false));
    assert!(output.contains("No accounts match"));
    assert!(!output.contains("Page 1 of"));
}

#[test]
fn unknown_tabs_and_timeframes_are_bounded() {
    assert_eq!(settings_tab(Some("unknown")), "profile");
    assert_eq!(settings_tab(Some("password")), "password");
    assert_eq!(normalize_days(Some("2147483647")), 30);
    assert_eq!(normalize_days(Some("7")), 7);
    assert!(columns(&[0]).iter().all(|column| column.y.is_finite()));
    assert!(columns(&[0, 0]).iter().all(|column| column.height == 0.0));
}

#[test]
fn activity_columns_scale_to_the_busiest_day() {
    let day = columns(&[0, 5, 10]);
    assert_eq!(day.len(), 3);
    assert_eq!(day[0].height, 0.0);
    assert_eq!(day[1].height, 50.0);
    assert_eq!(day[2].height, 100.0);
    assert_eq!(day[2].y, 0.0);
    assert!(day[0].x < day[1].x && day[1].x < day[2].x);
    assert!(columns(&[]).is_empty());
}

/// Whether the `<option>` with this value is the chosen one.
fn chosen(output: &str, value: &str) -> bool {
    let start = output
        .find(&format!("<option value=\"{value}\""))
        .expect(value);
    output[start..][..output[start..].find('>').unwrap()].contains("selected")
}

#[test]
fn site_settings_change_the_signup_mode_and_the_ai_crawler_policy() {
    let output = html(admin_settings_page(
        AdminSettings {
            private: true,
            ai_crawlers: "block".into(),
        },
        "csrf-marker",
        None,
        false,
    ));
    // Each setting is its own form with its own Save button.
    assert_eq!(output.matches("action=\"/admin/settings\"").count(), 2);
    assert_eq!(
        output
            .matches("name=\"csrf_token\" value=\"csrf-marker\"")
            .count(),
        2
    );
    assert!(output.contains("name=\"node_mode\""));
    assert!(output.contains("name=\"ai_crawlers\""));
    assert!(chosen(&output, "private") && !chosen(&output, "public"));
    assert!(chosen(&output, "block"));
    let public = html(admin_settings_page(
        AdminSettings {
            private: false,
            ai_crawlers: "allow".into(),
        },
        "csrf-marker",
        None,
        false,
    ));
    assert!(chosen(&public, "public") && !chosen(&public, "private"));
    assert!(chosen(&public, "allow"));
}

fn audit_model(entries: Vec<AuditEntry>, page: usize, pages: usize) -> AdminAudit {
    AdminAudit {
        entries,
        action: "ban".into(),
        actor: String::new(),
        page,
        pages,
        total: 120,
    }
}

fn audit_entry(id: i64, actor: &str) -> AuditEntry {
    AuditEntry {
        id,
        actor: actor.into(),
        action: "user.ban".into(),
        target: "bob".into(),
        org: String::new(),
        details: r#"{"banned":true}"#.into(),
        time: "2026-10-02T09:30:00+00:00".into(),
    }
}

#[test]
fn audit_log_reads_as_sentences_with_filters_and_pager_links_that_keep_them() {
    let mut model = audit_model(vec![audit_entry(2, "alice"), audit_entry(1, "")], 2, 3);
    model.action = "user.ban".into();
    let output = html(admin_audit_page(model));
    assert!(output.contains("Audit log"));
    assert!(output.contains("120 changes match."));
    assert!(output.contains("method=\"get\" action=\"/admin/audit\""));
    assert!(output.contains("name=\"action\""));
    assert!(output.contains("name=\"actor\""));
    assert!(chosen(&output, "user.ban"), "{output}");
    assert!(output.contains(">alice</a> banned <a"), "{output}");
    assert!(output.contains("href=\"/bob\""));
    assert!(output.contains("A removed account banned"));
    assert!(output.contains("title=\"Oct 2, 2026, 09:30 UTC\""));
    assert!(output.contains("href=\"/admin/audit?action=user.ban&#38;page=3\""));
    assert!(output.contains("Page 2 of 3"));
    assert!(!output.contains("<form method=\"post\""));
}

#[test]
fn audit_sentences_cover_every_recorded_change() {
    let cases = [
        (
            "user.create",
            "dave",
            r#"{"is_admin":true}"#,
            "created the account <a",
        ),
        ("user.ban", "dave", r#"{"banned":false}"#, "unbanned <a"),
        (
            "user.admin_role",
            "dave",
            r#"{"is_admin":false}"#,
            "removed the administrator role from <a",
        ),
        (
            "user.two_factor_reset",
            "dave",
            "",
            "turned off two-factor for <a",
        ),
        (
            "signup.approve",
            "dave",
            "",
            "approved the sign-up request from <a",
        ),
        (
            "signup.reject",
            "",
            r#"{"username":"eve"}"#,
            "rejected the sign-up request from eve",
        ),
        (
            "node.node_mode",
            "",
            r#"{"node_mode":"private"}"#,
            "made new accounts need approval",
        ),
        (
            "node.ai_crawlers",
            "",
            r#"{"ai_crawlers":"allow"}"#,
            "allowed AI crawlers",
        ),
        (
            "node.email_settings",
            "",
            r#"{"password_changed":true}"#,
            "changed the mail server and its password",
        ),
        (
            "node.status",
            "node-b",
            r#"{"status":"revoked"}"#,
            "set node <strong>node-b</strong> to revoked",
        ),
        (
            "org.governance.recover_owner",
            "dave",
            "",
            "an owner of <strong>acme</strong>",
        ),
        (
            "user.ban",
            "",
            r#"{"banned":true}"#,
            "banned a removed account",
        ),
    ];
    for (action, target, details, expected) in cases {
        let entry = AuditEntry {
            action: action.into(),
            target: target.into(),
            org: "acme".into(),
            details: details.into(),
            ..audit_entry(1, "alice")
        };
        let output = html(admin_audit_page(audit_model(vec![entry], 1, 1)));
        assert!(output.contains(expected), "{action}: {output}");
    }
}

#[test]
fn audit_log_without_events_says_so_and_the_first_page_has_no_previous_link() {
    let empty = html(admin_audit_page(audit_model(vec![], 1, 1)));
    assert!(empty.contains("No changes"));
    assert!(!empty.contains("Page 1 of"));
    let first = html(admin_audit_page(audit_model(
        vec![audit_entry(1, "bob")],
        1,
        3,
    )));
    assert!(first.contains("Next"));
    assert!(!first.contains("Previous"));
}
