//! Original identity view contracts, grouped by responsibility.
use super::*;

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
        ProfileActivity::default(),
        "token",
        false,
        false,
        false,
        1,
        2,
        None,
    ));
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
        ProfileActivity::default(),
        "token",
        false,
        true,
        true,
        0,
        0,
        None,
    ));
    assert!(output.contains("action=\"/alice/_forms/follow\""));
    assert!(output.contains("value=\"unfollow\""));
    assert!(output.contains("name=\"csrf_token\""));
    let own = html(profile_page(
        profile,
        vec![],
        ProfileActivity::default(),
        "token",
        true,
        true,
        false,
        0,
        0,
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
        ProfileActivity::default(),
        "profile-csrf",
        true,
        true,
        false,
        1,
        2,
        Some("<b>follow error</b>"),
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
    assert!(output.contains("href=\"#profile-repositories\""));
    assert!(output.contains("id=\"profile-repositories\""));
    assert!(output.contains("href=\"#profile-about\""));
    assert!(output.contains("id=\"profile-about\""));
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
        ProfileActivity::default(),
        "csrf-static-css",
        false,
        true,
        false,
        0,
        0,
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
    assert_eq!(css, include_str!("../profile.css"));
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
        ProfileActivity::default(),
        "profile-csrf",
        false,
        false,
        false,
        0,
        0,
        None,
    ));
    assert!(output.contains("src=\"/avatars/alice\""));
    assert!(output.contains("width=\"96\""));
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
        "token",
        false,
        false,
        false,
        0,
        0,
        None,
    ));
    assert!(output.contains("Recent activity"));
    assert!(output.contains("Pushed 3 commits to main"));
    assert!(output.contains("href=\"/alice/pub/commits?ref=main\""));
    assert!(output.contains("href=\"/alice?page=2#profile-activity\""));
}

#[test]
fn profile_without_activity_says_so() {
    let output = html(profile_page(
        Profile {
            username: "alice".into(),
            ..Profile::default()
        },
        vec![],
        ProfileActivity::default(),
        "token",
        false,
        false,
        false,
        0,
        0,
        None,
    ));
    assert!(output.contains("No recent activity."));
    assert!(!output.contains("Show more"));
}
