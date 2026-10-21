//! Admin page contracts: the timeframe control, the overview, the users page, site settings and the chart.
use super::page::{activity_totals, columns, date_tick_indices, timeframe_control};
use super::*;
use crate::webui::pages::fields::FormValues;
use crate::webui::pages::settings::settings_tab;
use dioxus::prelude::*;

fn html(element: Element) -> String {
    dioxus_ssr::render_element(element)
}

#[test]
fn timeframe_choices_are_plain_links_with_the_current_one_marked() {
    let output = html(timeframe_control(30));
    for days in [7, 30, 90, 180] {
        assert!(output.contains(&format!("href=\"/admin?days={days}\"")));
    }
    assert!(!output.contains("href=\"/admin?days=365\""));
    assert_eq!(output.matches("aria-current=\"page\"").count(), 1);
    assert!(!output.contains("<form"));
}

#[test]
fn chart_labels_the_first_last_and_evenly_spaced_days() {
    assert_eq!(date_tick_indices(0), Vec::<usize>::new());
    assert_eq!(date_tick_indices(1), vec![0]);
    assert_eq!(date_tick_indices(2), vec![0, 1]);
    assert_eq!(date_tick_indices(180), vec![0, 44, 89, 134, 179]);
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

fn overview(activity: Vec<Activity>, days: i32) -> Admin {
    Admin {
        accounts: vec![],
        users: 3,
        banned: 1,
        repositories: 7,
        organizations: 2,
        pending_requests: None,
        storage_bytes: 0,
        activity,
        days,
        status: SystemStatus {
            version: "0.1.2".into(),
            database: "SQLite".into(),
            private: false,
            nodes: None,
            queued_runs: 1,
        },
    }
}

#[test]
fn overview_shows_totals_and_system_status_without_forms() {
    let mut model = overview(vec![], 30);
    model.accounts = vec![Account {
        id: 42,
        username: "bob".into(),
        created: "today".into(),
    }];
    model.status.nodes = Some(3);
    let output = html(admin_page(model));
    for text in [
        ">Repositories<",
        ">Organizations<",
        "SQLite",
        "0.1.2",
        "Open",
        "3 cluster nodes",
        "1 queued run",
        "bob",
    ] {
        assert!(output.contains(text), "{text} missing in {output}");
    }
    // The overview only reads. Every action lives on its own page.
    assert!(!output.contains("<form"));
    assert!(!output.contains(">Pending requests<"));
    assert!(!output.contains("/admin/accounts/42/ban"));
    assert!(!output.contains("ai_crawlers"));
}

#[test]
fn overview_links_pending_requests_to_the_users_page() {
    let mut model = overview(vec![], 30);
    model.pending_requests = Some(2);
    model.status.private = true;
    let output = html(admin_page(model));
    assert!(output.contains(">Pending requests<"));
    assert!(output.contains("href=\"/admin/users\""));
    assert!(output.contains("Review requests"));
    assert!(output.contains("By approval"));
    assert!(output.contains("Single node"));
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
        page,
        pages,
        total: 30,
        values: FormValues::default(),
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
fn users_page_keeps_search_and_page_on_row_actions_and_offers_the_create_form() {
    let model = users_model(vec![account(42, "bob", false, true)], "bo", 2, 3);
    let output = html(admin_users_page(model, "csrf-marker", None, false));
    assert!(output.contains("name=\"q\" value=\"bo\""));
    assert!(output.contains("name=\"page\" value=\"2\""));
    assert!(output.contains("/admin/users?q=bo"));
    assert!(output.contains("page=3"));
    assert!(output.contains("href=\"/admin/users?q=bo\""));
    // A banned account is restored by a form that needs no confirmation.
    assert!(output.contains("name=\"banned\" value=\"false\""));
    assert!(!output.contains("id=\"ban-account-42\""));
    assert!(output.contains("action=\"/admin/users\""));
    assert!(output.contains("name=\"is_admin\""));
    assert!(output.contains("Page 2 of 3"));
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
fn users_page_without_requests_says_so() {
    let output = html(admin_users_page(
        users_model(vec![], "", 1, 1),
        "csrf-marker",
        None,
        false,
    ));
    assert!(output.contains("No pending requests"));
    assert!(!output.contains("Next requests"));
}

#[test]
fn public_users_page_does_not_offer_managed_account_creation() {
    let mut model = users_model(vec![], "", 1, 1);
    model.managed_accounts = false;
    let output = html(admin_users_page(model, "csrf", None, false));
    assert!(!output.contains("method=\"post\" action=\"/admin/users\""));
    assert!(!output.contains("Create user"));
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
    assert_eq!(normalize_days(Some("2147483647")), 180);
    assert_eq!(normalize_days(Some("7")), 7);
    assert!(columns(&[0]).iter().all(|column| column.y.is_finite()));
    assert!(columns(&[0, 0]).iter().all(|column| column.height == 0.0));
}

#[test]
fn activity_columns_scale_to_the_busiest_day() {
    let day = columns(&[0, 5, 10]);
    assert_eq!(day.len(), 3);
    assert_eq!(day[0].height, 0.0);
    assert_eq!(day[1].height, 79.0);
    assert_eq!(day[2].height, 158.0);
    assert_eq!(day[2].y, 12.0);
    assert!(day[0].x < day[1].x && day[1].x < day[2].x);
    assert!(columns(&[]).is_empty());
}

#[test]
fn activity_totals_cover_the_whole_window() {
    let point = |date: &str, contributions, bytes| Activity {
        date: date.into(),
        contributions,
        cumulative_contributions: contributions,
        bytes,
        cumulative_bytes: bytes,
    };
    let activity = [
        point("2026-10-01", 2, 100),
        point("2026-10-02", 0, 0),
        point("2026-10-03", 3, 50),
    ];
    assert_eq!(activity_totals(&activity), (5, 150, 2));
    assert_eq!(activity_totals(&[]), (0, 0, 0));
}

#[test]
fn admin_overview_draws_labelled_columns_with_a_visible_data_table() {
    let activity = vec![
        Activity {
            date: "2026-10-01".into(),
            contributions: 4,
            cumulative_contributions: 4,
            bytes: 2048,
            cumulative_bytes: 2048,
        },
        Activity {
            date: "2026-10-02".into(),
            contributions: 1,
            cumulative_contributions: 5,
            bytes: 0,
            cumulative_bytes: 2048,
        },
    ];
    let model = overview(activity, 7);
    let output = html(admin_page(model));
    assert!(output.contains("class=\"identity-admin-bar\""));
    assert!(output.contains("2026-10-01"));
    assert!(output.contains("2026-10-02"));
    assert!(output.contains(">Storage uploaded<"));
    assert!(output.contains("2.0 KiB"));
    assert!(!output.contains("2048 bytes"));
    // The chart and totals come first; the daily table sits in a disclosure that starts closed.
    let chart = output.find("class=\"identity-admin-chart\"").unwrap();
    let daily = output
        .find("<details class=\"identity-admin-daily-details\"><summary>Daily figures</summary>")
        .unwrap();
    let row = output.find("<td>2026-10-02</td>").unwrap();
    assert!(chart < daily && daily < row);
    assert!(!output.contains("<details open"));
    assert!(!output.contains("Site administration"));
    // One header pattern: the title and the section tabs.
    assert!(output.contains("class=\"admin-tabs\""));
    assert!(output.contains("href=\"/admin/nodes\""));
    assert!(output.contains("href=\"/admin/settings\""));
}

#[test]
fn the_daily_table_lists_only_the_newest_thirty_days() {
    let activity: Vec<Activity> = (1..=35)
        .map(|day| Activity {
            date: format!("day-{day:02}"),
            contributions: 1,
            cumulative_contributions: day,
            bytes: 1024,
            cumulative_bytes: 1024 * day,
        })
        .collect();
    let model = overview(activity, 180);
    let output = html(admin_page(model));
    assert!(output.contains("<td>day-35</td>"));
    assert!(output.contains("<td>day-06</td>"));
    assert!(!output.contains("<td>day-05</td>"));
    assert!(output.contains("Latest 30 of 35 days."));
}

#[test]
fn site_settings_offer_the_ai_crawler_policy_and_show_the_signup_mode() {
    let output = html(admin_settings_page(
        AdminSettings {
            private: true,
            ai_crawlers: "block".into(),
        },
        "csrf-marker",
        None,
        false,
    ));
    assert!(output.contains("action=\"/admin/settings\""));
    assert!(output.contains("name=\"ai_crawlers\""));
    assert!(output.contains("Block (default)"));
    assert!(output.contains("name=\"csrf_token\" value=\"csrf-marker\""));
    assert!(output.contains("approves each request"));
    let public = html(admin_settings_page(
        AdminSettings {
            private: false,
            ai_crawlers: "allow".into(),
        },
        "csrf-marker",
        None,
        false,
    ));
    assert!(public.contains("anyone can register"));
    assert!(!public.contains("approves each request"));
}

#[test]
fn status_and_signups_come_before_the_activity_chart() {
    let output = html(admin_page(overview(vec![], 7)));
    let status = output.find("admin-status-title").unwrap();
    let signups = output.find("admin-signups-title").unwrap();
    let activity = output.find("admin-activity-title").unwrap();
    assert!(status < signups && signups < activity, "{output}");
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
        target: "user 7".into(),
        details: r#"{"banned":true}"#.into(),
        time: "2026-10-02 09:30 UTC".into(),
    }
}

#[test]
fn audit_log_lists_events_with_filters_and_pager_links_that_keep_them() {
    let model = audit_model(vec![audit_entry(2, "alice"), audit_entry(1, "")], 2, 3);
    let output = html(admin_audit_page(model));
    assert!(output.contains("Audit log"));
    assert!(output.contains("120 events"));
    assert!(output.contains("method=\"get\" action=\"/admin/audit\""));
    assert!(output.contains("name=\"action\""));
    assert!(output.contains("name=\"actor\""));
    assert!(output.contains("alice"));
    assert!(output.contains("Removed account"));
    assert!(output.contains("user 7"));
    assert!(output.contains("href=\"/admin/audit?action=ban\""));
    assert!(output.contains("page=3"));
    assert!(output.contains("Page 2 of 3"));
    assert!(!output.contains("<form method=\"post\""));
}

#[test]
fn audit_log_without_events_says_so_and_the_first_page_has_no_previous_link() {
    let empty = html(admin_audit_page(audit_model(vec![], 1, 1)));
    assert!(empty.contains("No events"));
    assert!(!empty.contains("Page 1 of"));
    let first = html(admin_audit_page(audit_model(
        vec![audit_entry(1, "bob")],
        1,
        3,
    )));
    assert!(first.contains("Next"));
    assert!(!first.contains("Previous"));
}
