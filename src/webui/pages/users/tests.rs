//! Profile page contracts: safe website links, and the repository and activity lists.
use super::page::{profile_repository_rows, safe_website};
use super::*;
use dioxus::prelude::Element;

fn html(element: Element) -> String {
    dioxus_ssr::render_element(element)
}

#[test]
fn public_profile_never_links_executable_websites() {
    for website in [
        "javascript:alert(1)",
        "data:text/html,evil",
        "//evil.example",
        "https://good.example\\evil",
        "https://user@evil.example",
    ] {
        assert!(safe_website(website).is_none());
    }
    assert_eq!(
        safe_website("https://example.org/path?q=1"),
        Some("https://example.org/path?q=1")
    );
    let output = html(profile_page(
        Profile {
            username: "alice".into(),
            bio: "<img src=x onerror=evil>".into(),
            website: "javascript:alert(1)".into(),
            ..Profile::default()
        },
        vec![],
        0,
        ProfileActivity::default(),
        ProfileStars::default(),
        "token",
        false,
        false,
        false,
        1,
        2,
        None,
        Some(2),
    ));
    assert!(output.contains("href=\"/alice?repo_page=2#profile-repositories\""));
    assert!(!output.contains("href=\"javascript:"));
    assert!(!output.contains("<img src=x"));
    assert!(!output.contains("action=\"/alice/_forms/follow\""));
    assert!(output.contains("Sign in to follow"));
}

#[test]
fn native_follow_forms_carry_explicit_action_and_csrf() {
    let profile = Profile {
        username: "alice".into(),
        ..Profile::default()
    };
    let output = html(profile_page(
        profile.clone(),
        vec![],
        0,
        ProfileActivity::default(),
        ProfileStars::default(),
        "token",
        false,
        true,
        true,
        0,
        0,
        None,
        None,
    ));
    assert!(output.contains("action=\"/alice/_forms/follow\""));
    assert!(output.contains("value=\"unfollow\""));
    assert!(output.contains("name=\"csrf_token\""));
    let own = html(profile_page(
        profile,
        vec![],
        0,
        ProfileActivity::default(),
        ProfileStars::default(),
        "token",
        true,
        true,
        false,
        0,
        0,
        None,
        None,
    ));
    assert!(!own.contains("action=\"/alice/_forms/follow\""));
}

#[test]
fn profile_repository_filters_match_only_supplied_names_descriptions_and_types() {
    let repositories = vec![
        Repository {
            name: "alpha".into(),
            description: "A Rust library".into(),
            private: false,
            archived: false,
            stars: 2,
            forks: 4,
            updated: "Oct 1, 2026".into(),
        },
        Repository {
            name: "beta".into(),
            description: "Internal RUST tools".into(),
            private: true,
            archived: true,
            stars: 8,
            forks: 1,
            updated: "Sep 2, 2026".into(),
        },
        Repository {
            name: "gamma".into(),
            description: "Documentation".into(),
            private: false,
            archived: true,
            stars: 8,
            forks: 2,
            updated: "Aug 3, 2026".into(),
        },
    ];
    let names = |query: &str, kind: &str, sort: &str| -> Vec<String> {
        profile_repository_rows(&repositories, query, kind, sort)
            .into_iter()
            .map(|repo| repo.name.clone())
            .collect()
    };
    assert_eq!(names("", "all", "name"), ["alpha", "beta", "gamma"]);
    assert_eq!(names("  rust  ", "all", "name"), ["alpha", "beta"]);
    assert_eq!(names("ALPHA", "all", "name"), ["alpha"]);
    assert_eq!(names("rust", "public", "name"), ["alpha"]);
    assert_eq!(names("", "private", "name"), ["beta"]);
    assert_eq!(names("", "archived", "name"), ["beta", "gamma"]);
    assert_eq!(names("", "active", "name"), ["alpha"]);
    assert_eq!(names("", "all", "stars"), ["beta", "gamma", "alpha"]);
    assert_eq!(names("", "all", "forks"), ["alpha", "gamma", "beta"]);
    assert!(names("no match", "all", "name").is_empty());
    // Filtering/sorting neither mutates nor supplements the authorized input.
    assert_eq!(
        repositories
            .iter()
            .map(|repo| repo.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta", "gamma"]
    );
    assert!(profile_repository_rows(&[], "", "all", "stars").is_empty());
}

#[test]
fn profile_initial_html_has_real_rows_metadata_links_and_safe_personal_details() {
    let output = html(profile_page(
        Profile {
            username: "alice".into(),
            display_name: "Alice & Bob".into(),
            bio: "<script>not markup</script>".into(),
            location: "Berlin".into(),
            website: "https://example.org".into(),
            joined: "Oct 1, 2026".into(),
            ..Profile::default()
        },
        vec![Repository {
            name: "notes & tools".into(),
            description: "<img src=x onerror=evil>".into(),
            private: true,
            archived: true,
            stars: 7,
            forks: 3,
            updated: "Oct 2, 2026".into(),
        }],
        1,
        ProfileActivity::default(),
        ProfileStars::default(),
        "profile-csrf",
        true,
        true,
        false,
        1,
        2,
        Some("<b>follow error</b>"),
        None,
    ));
    assert!(output.contains("Alice &#38; Bob"));
    assert!(output.contains("&#60;script&#62;not markup&#60;/script&#62;"));
    assert!(output.contains("&#60;img src=x onerror=evil&#62;"));
    assert!(output.contains("&#60;b&#62;follow error&#60;/b&#62;"));
    assert!(!output.contains("<script>"));
    assert!(!output.contains("<img src=x"));
    assert!(output.contains("href=\"/alice/notes%20%26%20tools\""));
    assert!(output.contains("href=\"/settings\""));
    assert!(output.contains("href=\"/new\""));
    assert!(output.contains("href=\"/alice?tab=stars\""));
    assert!(output.contains("id=\"profile-repositories\""));
    assert!(!output.contains("profile-about"));
    assert!(output.contains("class=\"identity-profile-sidebar\""));
    assert!(output.contains("href=\"https://example.org\""));
    assert!(output.contains("rel=\"noopener noreferrer\""));
    assert!(output.contains("aria-label=\"7 stars\""));
    assert!(output.contains("aria-label=\"3 forks\""));
    assert!(output.contains("Updated Oct 2, 2026"));
    assert!(output.contains("Joined Oct 1, 2026"));
    assert!(output.contains("Private"));
    assert!(output.contains("Archived"));
    assert!(output.contains(" follower"));
    assert!(!output.contains("<option value=\"language\""));
    assert!(!output.contains("role=\"tab\"")); // Section links, not pretend routed tabs.
}

#[test]
fn profile_prerendered_static_css_has_no_hydration_markers() {
    let mut renderer = dioxus_ssr::Renderer::new();
    renderer.pre_render = true;
    let output = renderer.render_element(profile_page(
        Profile {
            username: "alice".into(),
            ..Profile::default()
        },
        vec![Repository {
            name: "real-repository".into(),
            description: String::new(),
            private: false,
            archived: false,
            stars: 0,
            forks: 0,
            updated: "Oct 2, 2026".into(),
        }],
        1,
        ProfileActivity::default(),
        ProfileStars::default(),
        "csrf-static-css",
        false,
        true,
        false,
        0,
        0,
        None,
        None,
    ));
    let css = output
        .split_once("<style")
        .unwrap()
        .1
        .split_once('>')
        .unwrap()
        .1
        .split_once("</style>")
        .unwrap()
        .0;
    assert_eq!(css, include_str!("../account.css"));
    assert!(!css.contains("<!--"));
    assert!(output.contains("href=\"/alice/real-repository\""));
    assert!(output.contains("action=\"/alice/_forms/follow\""));
    assert!(output.contains("name=\"csrf_token\""));
    assert!(output.contains("value=\"csrf-static-css\""));
    assert!(output.contains("value=\"follow\""));
    assert!(output.contains("disabled")); // No inert enabled filter during SSR/no-JS fallback.
}

#[test]
fn profile_masthead_shows_the_account_picture_at_96_pixels() {
    let output = html(profile_page(
        Profile {
            username: "alice".into(),
            ..Profile::default()
        },
        vec![],
        0,
        ProfileActivity::default(),
        ProfileStars::default(),
        "profile-csrf",
        false,
        false,
        false,
        0,
        0,
        None,
        None,
    ));
    assert!(output.contains("src=\"/avatars/alice\""));
    assert!(output.contains("width=\"256\""));
    assert!(!output.contains("identity-profile-avatar"));
}

#[test]
fn profile_lists_recent_activity_with_a_show_more_link() {
    let output = html(profile_page(
        Profile {
            username: "alice".into(),
            ..Profile::default()
        },
        vec![],
        0,
        ProfileActivity {
            items: vec![ProfileActivityItem {
                kind: "push".into(),
                created_at: "2026-10-16T10:00:00+00:00".into(),
                repository: "alice/pub".into(),
                url: "/alice/pub/commits?ref=main".into(),
                number: None,
                title: None,
                branch: Some("main".into()),
                commits: Some(3),
            }],
            next_page: Some(2),
        },
        ProfileStars::default(),
        "token",
        false,
        false,
        false,
        0,
        0,
        None,
        None,
    ));
    assert!(output.contains("Recent activity"));
    assert!(output.contains("Pushed 3 commits to main"));
    assert!(output.contains("href=\"/alice/pub/commits?ref=main\""));
    assert!(output.contains("href=\"/alice?page=2#profile-activity\""));
}

#[test]
fn profile_feed_leaves_out_pushes_without_commits() {
    let push = |commits: i32| ProfileActivityItem {
        kind: "push".into(),
        created_at: "2026-10-16T10:00:00+00:00".into(),
        repository: "alice/pub".into(),
        url: "/alice/pub/commits?ref=main".into(),
        number: None,
        title: None,
        branch: Some("main".into()),
        commits: Some(commits),
    };
    let page = |items: Vec<ProfileActivityItem>| {
        html(profile_page(
            Profile {
                username: "alice".into(),
                ..Profile::default()
            },
            vec![],
            0,
            ProfileActivity {
                items,
                next_page: None,
            },
            ProfileStars::default(),
            "token",
            false,
            false,
            false,
            0,
            0,
            None,
            None,
        ))
    };
    let output = page(vec![push(0), push(2)]);
    assert!(output.contains("Pushed 2 commits to main"));
    assert!(!output.contains("Pushed 0 commits"));
    let only_empty = page(vec![push(0)]);
    assert!(only_empty.contains("No recent activity."));
}

#[test]
fn profile_without_activity_says_so() {
    let output = html(profile_page(
        Profile {
            username: "alice".into(),
            ..Profile::default()
        },
        vec![],
        0,
        ProfileActivity::default(),
        ProfileStars::default(),
        "token",
        false,
        false,
        false,
        0,
        0,
        None,
        None,
    ));
    assert!(output.contains("No recent activity."));
    assert!(!output.contains("Show more"));
}

#[test]
fn profile_groups_activity_by_day_with_links_to_each_repository() {
    let issue = |created_at: &str| ProfileActivityItem {
        kind: "issue_opened".into(),
        created_at: created_at.into(),
        repository: "alice/one".into(),
        url: "/alice/one/issues/1".into(),
        number: Some(1),
        title: Some("Bug".into()),
        branch: None,
        commits: None,
    };
    let output = html(profile_page(
        Profile {
            username: "alice".into(),
            ..Profile::default()
        },
        vec![],
        0,
        ProfileActivity {
            items: vec![
                issue("2026-10-16T10:00:00+00:00"),
                issue("2026-10-16T09:00:00+00:00"),
                issue("2026-10-15T08:00:00+00:00"),
            ],
            next_page: None,
        },
        ProfileStars::default(),
        "token",
        false,
        false,
        false,
        0,
        0,
        None,
        None,
    ));
    // One heading per day, newest first, and the repository is a link of its own.
    assert_eq!(
        output
            .matches("class=\"identity-profile-activity-date\"")
            .count(),
        2
    );
    let first = output.find("Oct 16, 2026").expect("first day heading");
    let second = output.find("Oct 15, 2026").expect("second day heading");
    assert!(first < second);
    assert_eq!(output.matches("href=\"/alice/one\"").count(), 3);
    assert!(output.contains("Opened issue #1: Bug"));
}

#[test]
fn profile_stars_tab_counts_on_the_tab_and_lists_its_page_only_when_open() {
    use crate::webui::pages::discovery::RepositoryCard;
    let starred = RepositoryCard {
        owner: "bob".into(),
        name: "rustacean".into(),
        description: Some("Rust tools".into()),
        topics: vec!["rust".into()],
        is_private: false,
        stars: 4,
        forks: 1,
        default_branch: "main".into(),
        updated_at: "2026-10-16".into(),
        primary_language: None,
    };
    let page = |stars: ProfileStars| {
        html(profile_page(
            Profile {
                username: "alice".into(),
                ..Profile::default()
            },
            vec![],
            0,
            ProfileActivity::default(),
            stars,
            "token",
            false,
            false,
            false,
            0,
            0,
            None,
            None,
        ))
    };
    let closed = page(ProfileStars {
        count: 3,
        ..ProfileStars::default()
    });
    assert!(closed.contains("aria-label=\"Stars (3)\""));
    assert!(closed.contains("href=\"/alice?tab=stars\""));
    assert!(closed.contains("Recent activity"));
    assert!(!closed.contains("id=\"profile-stars\""));

    let open = page(ProfileStars {
        count: 3,
        open: true,
        page: 2,
        has_older: true,
        repositories: vec![starred],
        sort: "recent".into(),
        query: String::new(),
    });
    assert!(open.contains("id=\"profile-stars\""));
    assert!(open.contains("href=\"/bob/rustacean\""));
    assert!(open.contains("4 stars"));
    // The Stars tab replaces the repositories and the activity, and pages through its own list.
    assert!(!open.contains("Recent activity"));
    assert!(!open.contains("id=\"profile-repositories\""));
    assert!(open.contains("Newer"));
    assert!(open.contains("Older"));
    assert!(open.contains("page=3"));
}

#[test]
fn profile_stars_tab_has_the_stars_filter_bar_and_keeps_its_filters_in_pages() {
    let stars = ProfileStars {
        count: 60,
        open: true,
        page: 2,
        has_older: true,
        repositories: Vec::new(),
        sort: "name".into(),
        query: "tool".into(),
    };
    let page = html(profile_page(
        Profile {
            username: "alice".into(),
            ..Profile::default()
        },
        vec![],
        0,
        ProfileActivity::default(),
        stars,
        "token",
        false,
        false,
        false,
        0,
        0,
        None,
        None,
    ));
    // The bar is the one /stars uses, submitted back to the profile's Stars tab.
    assert!(page.contains("id=\"profile-stars-filters\""), "{page}");
    assert!(page.contains("action=\"/alice\""), "{page}");
    assert!(page.contains("name=\"tab\""), "{page}");
    assert!(page.contains("value=\"tool\""), "{page}");
    // The older page keeps the order and the text it was reached with.
    let hrefs: Vec<&str> = page
        .split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .collect();
    let older = hrefs
        .iter()
        .find(|href| href.starts_with("/alice?tab=stars") && href.contains("page=3"))
        .unwrap_or_else(|| panic!("no older link in {page}"));
    assert!(
        older.contains("sort=name") && older.contains("q=tool"),
        "{older}"
    );
    assert!(
        page.contains("No starred repositories match “tool”."),
        "{page}"
    );
}

#[test]
fn profile_stars_tab_says_whose_empty_list_it_is() {
    let empty = |own: bool| {
        html(profile_page(
            Profile {
                username: "alice".into(),
                ..Profile::default()
            },
            vec![],
            0,
            ProfileActivity::default(),
            ProfileStars {
                open: true,
                page: 1,
                ..ProfileStars::default()
            },
            "token",
            own,
            false,
            false,
            0,
            0,
            None,
            None,
        ))
    };
    assert!(empty(true).contains("You have not starred any repository you can read yet."));
    assert!(empty(false).contains("No starred repositories visible to you."));
}
