//! Admin page contracts: the timeframe control, the overview's account summary, the users page and the chart.
use super::page::{activity_totals, columns, date_tick_indices, timeframe_control};
use super::*;
use crate::webui::pages::fields::FormValues;
use crate::webui::pages::settings::settings_tab;
use dioxus::prelude::*;

fn html(element: Element) -> String {
    dioxus_ssr::render_element(element)
}

#[test]
fn signup_review_pages_link_both_directions_and_show_the_total() {
    let output = html(super::signup_pagination(2, 61, "/settings?tab=access"));
    assert!(output.contains("61 pending requests"));
    assert!(output.contains("Page 2 of 3"));
    assert!(output.contains("request_page=1"));
    assert!(output.contains("request_page=3"));
    assert!(output.contains("Previous requests"));
    assert!(output.contains("Next requests"));
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

#[test]
fn overview_links_to_the_users_page_instead_of_listing_every_action() {
    let model = Admin {
        current_user_id: 1,
        ai_crawlers: "block".into(),
        accounts: vec![Account {
            id: 42,
            username: "bob".into(),
            created: "today".into(),
        }],
        totals: (1, 0, 0),
        activity: vec![],
        days: 30,
        manage_access: false,
        requests: vec![],
        requests_count: 0,
        requests_page: 1,
        values: FormValues::default(),
    };
    let output = html(admin_page(model, "csrf-marker", None, false));
    assert!(output.contains("href=\"/admin/users\""));
    assert!(output.contains("Manage users"));
    assert!(!output.contains("/admin/accounts/42/ban"));
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
    assert!(output.contains("action=\"/admin/access/users\""));
    assert!(output.contains("name=\"is_admin\""));
    assert!(output.contains("Page 2 of 3"));
}

#[test]
fn public_users_page_does_not_offer_managed_account_creation() {
    let mut model = users_model(vec![], "", 1, 1);
    model.managed_accounts = false;
    let output = html(admin_users_page(model, "csrf", None, false));
    assert!(!output.contains("/admin/access/users"));
    assert!(!output.contains("Create user"));
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
    let model = Admin {
        current_user_id: 1,
        ai_crawlers: "block".into(),
        accounts: vec![],
        totals: (0, 0, 0),
        activity,
        days: 7,
        manage_access: false,
        requests: vec![],
        requests_count: 0,
        requests_page: 1,
        values: FormValues::default(),
    };
    let output = html(admin_page(model, "csrf-marker", None, false));
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
    // One header pattern: breadcrumb, title and the two page links.
    assert!(output.contains("class=\"admin-crumbs\""));
    assert!(output.contains(">Administration</a> / Overview"));
    assert!(output.contains("href=\"/admin/nodes\""));
    assert!(output.contains("href=\"/admin/storage\""));
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
    let model = Admin {
        current_user_id: 1,
        ai_crawlers: "block".into(),
        accounts: vec![],
        totals: (0, 0, 0),
        activity,
        days: 180,
        manage_access: false,
        requests: vec![],
        requests_count: 0,
        requests_page: 1,
        values: FormValues::default(),
    };
    let output = html(admin_page(model, "csrf-marker", None, false));
    assert!(output.contains("<td>day-35</td>"));
    assert!(output.contains("<td>day-06</td>"));
    assert!(!output.contains("<td>day-05</td>"));
    assert!(output.contains("Latest 30 of 35 days."));
}

#[test]
fn admin_overview_offers_the_ai_crawler_setting() {
    let model = Admin {
        current_user_id: 1,
        totals: (0, 0, 0),
        accounts: vec![],
        activity: vec![],
        days: 180,
        manage_access: false,
        requests: vec![],
        requests_count: 0,
        requests_page: 1,
        values: FormValues::default(),
        ai_crawlers: "block".into(),
    };
    let output = html(admin_page(model, "csrf-marker", None, false));
    assert!(output.contains("action=\"/admin/ai-crawlers\""));
    assert!(output.contains("name=\"ai_crawlers\""));
    assert!(output.contains("Block (default)"));
}

#[test]
fn accounts_and_requests_come_before_the_activity_chart() {
    let model = Admin {
        current_user_id: 1,
        ai_crawlers: "block".into(),
        accounts: vec![],
        totals: (0, 0, 0),
        activity: vec![],
        days: 7,
        manage_access: false,
        requests: vec![],
        requests_count: 0,
        requests_page: 1,
        values: FormValues::default(),
    };
    let output = html(admin_page(model, "csrf-marker", None, false));
    let signups = output.find("admin-signups-title").unwrap();
    let requests = output.find("admin-requests-title").unwrap();
    let accounts = output.find("admin-accounts-title").unwrap();
    let activity = output.find("admin-activity-title").unwrap();
    assert!(
        signups < requests && requests < accounts && accounts < activity,
        "{output}"
    );
}
