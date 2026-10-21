use super::*;

#[test]
fn expired_badge_counts_are_omitted_instead_of_showing_zero() {
    let model = SearchModel {
        tab: "repos".into(),
        counts_unavailable: vec!["issues".into()],
        ..Default::default()
    };
    assert_eq!(search_tab_count(&model, "issues"), None);
    assert_eq!(search_tab_count(&model, "repos"), Some("0".into()));
}

/// The markup from `marker` through the first `close` after it.
fn element_at<'a>(html: &'a str, marker: &str, close: &str) -> &'a str {
    let at = html
        .find(marker)
        .unwrap_or_else(|| panic!("missing {marker}"));
    let end = html[at..]
        .find(close)
        .map_or(html.len(), |end| at + end + close.len());
    &html[at..end]
}

#[test]
fn urls_encode_segments_and_preserve_search_queries() {
    assert_eq!(repository_url("a/b", "x?#"), "/a%2Fb/x%3F%23");
    assert_eq!(
        search_tab_url("rust & wasm+#", "issues"),
        "/search?q=rust%20%26%20wasm%2B%23&type=issues"
    );
}

#[test]
fn hydration_page_models_round_trip_only_presentation_data() {
    let page = Page::Home(HomeModel {
        username: Some("alice".into()),
        ..Default::default()
    });
    let json = serde_json::to_string(&page).unwrap();
    assert_eq!(serde_json::from_str::<Page>(&json).unwrap(), page);
    for private_field in [
        "password_hash",
        "email",
        "disk_path",
        "is_admin",
        "two_factor_enabled",
    ] {
        assert!(!json.contains(private_field));
    }
}

#[test]
fn hydrated_name_feedback_rejects_bad_names_without_becoming_server_authority() {
    for name in [
        "", ".", "..", "../demo", "a/b", "a\\b", "bad name", "demo.git",
    ] {
        assert!(!valid_repository_name(name));
    }
    assert!(!valid_repository_name(&"a".repeat(101)));
    assert!(valid_repository_name("demo-repo_1.0"));
    assert!(valid_repository_name("répo"));
}

#[test]
fn explore_with_no_match_offers_a_clear_search_link_that_keeps_the_scope() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(explore(ExploreModel {
            sort: "stars".into(),
            query: "python".into(),
            language: String::new(),
            topic: String::new(),
            kind: String::new(),
            languages: vec![],
            page: 3,
            has_older: false,
            repositories: vec![],
        }))
    });
    assert!(html.contains("No repositories match"));
    assert!(html.contains("python"));
    assert!(html.contains("Clear search"));
    assert!(html.contains("sort=stars"));
    assert!(!html.contains("page=3"));
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(explore(ExploreModel {
            sort: "recent".into(),
            query: String::new(),
            language: String::new(),
            topic: String::new(),
            kind: String::new(),
            languages: vec![],
            page: 1,
            has_older: false,
            repositories: vec![],
        }))
    });
    assert!(!html.contains("Clear search"));
}

#[test]
fn search_renders_escaped_content_real_links_and_total_counts() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(search(SearchModel {
            query: "<script>alert(1)</script>&".into(),
            tab: "issues".into(),
            issues: vec![IssueCard {
                owner: "alice".into(),
                repo: "demo".into(),
                number: 4,
                title: "<img src=x onerror=alert(1)>".into(),
                ..Default::default()
            }],
            counts: SearchCounts {
                issues: 1,
                ..Default::default()
            },
            ..Default::default()
        }))
    });
    assert!(html.contains("&#60;script&#62;"));
    assert!(html.contains("&#60;img"));
    assert!(!html.contains("<script>"));
    assert!(!html.contains("<img src=x"));
    assert!(html.contains("/alice/demo/issues/4"));
    assert!(html.contains("Issues"));
    assert!(html.contains(">1</span>"));
    assert!(!html.contains("loaded"));
}

#[test]
fn count_badges_show_the_cap_as_more_than_the_cap() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(search(SearchModel {
            query: "needle".into(),
            tab: "repos".into(),
            counts: SearchCounts {
                repositories: SEARCH_COUNT_CAP,
                issues: SEARCH_COUNT_CAP - 1,
                ..Default::default()
            },
            ..Default::default()
        }))
    });
    assert!(html.contains(">1000+</span>"), "{html}");
    assert!(html.contains(">999</span>"), "{html}");
}

#[test]
fn code_hit_links_encode_each_path_segment() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(search(SearchModel {
            query: "needle".into(),
            tab: "code".into(),
            code: vec![CodeHit {
                owner: "alice".into(),
                repo: "demo".into(),
                branch: "main".into(),
                path: "src/a#b?c%d.rs".into(),
                line_number: 3,
                line: "needle".into(),
            }],
            ..Default::default()
        }))
    });
    assert!(
        html.contains(
            "/alice/demo/blob/refs/heads/main/src/a%23b%3Fc%25d.rs?view=code&#38;line=3#L3"
        ),
        "{html}"
    );
}

#[test]
fn issue_and_pull_cards_show_state_icon_number_author_and_excerpt() {
    let issue = IssueCard {
        owner: "alice".into(),
        repo: "demo".into(),
        number: 1,
        title: "Crash when saving an empty file".into(),
        state: "open".into(),
        author: "bob".into(),
        excerpt: "I reproduced the crash".into(),
        created_at: "2026-10-18T09:00:00".into(),
        updated_at: "2026-10-18".into(),
    };
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(search(SearchModel {
            query: "crash".into(),
            tab: "issues".into(),
            issues: vec![issue.clone()],
            counts: SearchCounts {
                issues: 1,
                ..Default::default()
            },
            ..Default::default()
        }))
    });
    assert!(html.contains("Crash when saving an empty file"), "{html}");
    assert!(html.contains("alice/demo #1 opened by bob"), "{html}");
    assert!(html.contains("I reproduced the crash"), "{html}");
    assert!(html.contains("collaboration-icon-open"), "{html}");
    assert!(html.contains("collaboration-state-open"), "{html}");
    assert!(html.contains("/alice/demo/issues/1"), "{html}");
    // A merged pull request has its own icon and state colour.
    let pull = IssueCard {
        state: "merged".into(),
        author: String::new(),
        excerpt: String::new(),
        ..issue
    };
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(search(SearchModel {
            query: "crash".into(),
            tab: "pulls".into(),
            pulls: vec![pull.clone()],
            counts: SearchCounts {
                pulls: 1,
                ..Default::default()
            },
            ..Default::default()
        }))
    });
    assert!(html.contains("collaboration-icon-merged"), "{html}");
    assert!(html.contains("collaboration-state-merged"), "{html}");
    assert!(html.contains("/alice/demo/pulls/1"), "{html}");
    assert!(!html.contains("opened by"), "{html}");
}

#[test]
fn code_matches_are_marked_and_the_line_stays_escaped() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(search(SearchModel {
            query: "old".into(),
            tab: "code".into(),
            highlight: "old".into(),
            code: vec![CodeHit {
                owner: "alice".into(),
                repo: "demo".into(),
                branch: "main".into(),
                path: "old.rs".into(),
                line_number: 1,
                line: "fn old() { <b>OLD</b> }".into(),
            }],
            ..Default::default()
        }))
    });
    assert!(
        html.contains("<mark class=\"search-match\">old</mark>"),
        "{html}"
    );
    assert!(
        html.contains("<mark class=\"search-match\">OLD</mark>"),
        "{html}"
    );
    assert!(html.contains("&#60;b&#62;"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
}

#[test]
fn highlight_parts_split_at_each_match_ignoring_ascii_case() {
    let parts = |line: &str, needle: &str| -> Vec<(String, bool)> { highlight_parts(line, needle) };
    let owned = |items: &[(&str, bool)]| -> Vec<(String, bool)> {
        items
            .iter()
            .map(|(text, matched)| ((*text).to_owned(), *matched))
            .collect()
    };
    assert_eq!(
        parts("fn Old() old", "old"),
        owned(&[("fn ", false), ("Old", true), ("() ", false), ("old", true)])
    );
    // Non-ASCII text stays whole, and the match after it is still found.
    assert_eq!(
        parts("é old", "OLD"),
        owned(&[("é ", false), ("old", true)])
    );
    assert_eq!(
        parts("nothing here", "zzz"),
        owned(&[("nothing here", false)])
    );
    assert_eq!(parts("unchanged", ""), owned(&[("unchanged", false)]));
}

#[test]
fn people_results_use_the_avatar_component() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(search(SearchModel {
            query: "alice".into(),
            tab: "users".into(),
            users: vec![UserCard {
                username: "alice".into(),
                display_name: Some("Alice".into()),
                ..Default::default()
            }],
            ..Default::default()
        }))
    });
    assert!(html.contains("src=\"/avatars/alice\""), "{html}");
    assert!(html.contains("class=\"avatar\""), "{html}");
    assert!(!html.contains(">@</div>"), "{html}");
}

#[test]
fn stars_rows_carry_one_unstar_form_that_returns_to_the_list() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(stars(StarsModel {
            sort: "stars".into(),
            query: String::new(),
            page: 2,
            has_older: false,
            repositories: vec![RepositoryCard {
                owner: "bob".into(),
                name: "tool".into(),
                stars: 3,
                ..Default::default()
            }],
            csrf_token: "stars-token".into(),
        }))
    });
    assert_eq!(
        html.matches("action=\"/bob/tool/_forms/unstar\"").count(),
        1,
        "{html}"
    );
    let action = html.find("action=\"/bob/tool/_forms/unstar\"").unwrap();
    let open = html[..action].rfind("<form").unwrap();
    let form = element_at(&html[open..], "<form", "</form>");
    assert!(form.contains("method=\"post\""), "{form}");
    assert!(form.contains("name=\"csrf_token\""), "{form}");
    assert!(form.contains("value=\"stars-token\""), "{form}");
    assert!(form.contains("name=\"return_to\""), "{form}");
    assert!(form.contains("/stars?sort=stars"), "{form}");
    assert!(form.contains("aria-label=\"Unstar bob/tool\""), "{form}");
    // The filter bar above the list is the one /stars shares with the profile tab.
    assert!(html.contains("id=\"stars-filters\""), "{html}");
    assert!(html.contains("action=\"/stars\""), "{html}");
}

#[test]
fn empty_repository_search_names_the_query_and_suggests_a_broader_term() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(search(SearchModel {
            query: "zzz".into(),
            tab: "repos".into(),
            ..Default::default()
        }))
    });
    assert!(html.contains("No repositories found for"));
    assert!(html.contains("zzz"));
    assert!(html.contains("Try a broader search term."));
}

#[test]
fn creation_models_round_trip_and_missing_fields_remain_personal() {
    let page = Page::NewRepository {
        owner: "alice".into(),
        csrf_token: "fixture-csrf".into(),
        choices: RepositoryCreationChoices {
            organizations: vec![RepositoryCreationOrganization {
                id: 4,
                slug: "native_org".into(),
            }],
            after: 2,
            limit: 1,
            next_after: Some(4),
        },
        form: NewRepositoryForm {
            owner_org_id: "4".into(),
            is_private: true,
            ..Default::default()
        },
        error: None,
    };
    let json = serde_json::to_string(&page).unwrap();
    assert_eq!(serde_json::from_str::<Page>(&json).unwrap(), page);
    for field in [
        "billing_email",
        "storage_operation",
        "storage_actor_id",
        "disk_path",
        "governance_suspended",
    ] {
        assert!(!json.contains(field));
    }
    let legacy = r#"{"NewRepository":{"owner":"alice","csrf_token":"fixture-csrf","form":{"name":"demo","description":"draft","is_private":false},"error":null}}"#;
    match serde_json::from_str::<Page>(legacy).unwrap() {
        Page::NewRepository { choices, form, .. } => {
            assert_eq!(choices, RepositoryCreationChoices::default());
            assert!(form.owner_org_id.is_empty());
        }
        _ => panic!("legacy creation model"),
    }
}

#[test]
fn a_refused_name_is_marked_beside_its_field_and_keeps_the_draft() {
    let html = dioxus_ssr::render_element(new_repository(
        "alice".into(),
        "csrf-test".into(),
        RepositoryCreationChoices::default(),
        NewRepositoryForm {
            name: String::new(),
            description: "kept draft".into(),
            is_private: false,
            owner_org_id: String::new(),
            ..Default::default()
        },
        Some(FormError {
            field: Some("name".into()),
            message: "Repository name must be 1–100 bytes.".into(),
        }),
    ));
    assert!(html.contains("novalidate"));
    assert!(!html.contains(" required="));
    assert!(html.contains("aria-required=\"true\""));
    assert!(!html.contains("role=\"alert\""));
    assert!(html.contains("id=\"repo-name-error\""));
    assert!(html.contains("Repository name must be 1–100 bytes."));
    assert!(html.contains("aria-invalid=\"true\""));
    assert!(html.contains("aria-describedby=\"repo-name-error\""));
    assert!(!html.contains("repo-name-help"));
    assert!(html.contains("kept draft"));
}

#[test]
fn creation_choice_renderer_escapes_slugs_and_preserves_native_selection_and_cursor() {
    let html = dioxus_ssr::render_element(new_repository(
        "alice".into(),
        "fixture-csrf".into(),
        RepositoryCreationChoices {
            organizations: vec![RepositoryCreationOrganization {
                id: 4,
                slug: "<script>org</script>".into(),
            }],
            after: 2,
            limit: 1,
            next_after: Some(4),
        },
        NewRepositoryForm {
            owner_org_id: "4".into(),
            ..Default::default()
        },
        None,
    ));
    assert!(!html.contains("<script>org</script>"));
    assert!(html.contains("&#60;script&#62;org&#60;/script&#62;"));
    assert!(!html.contains("Selected organization unavailable"));
    assert!(html.contains("formaction=\"/new?org_after=4&#38;org_limit=1\""));
    assert!(html.contains("name=\"intent\" value=\"page-owners\""));
    let first_submit = html
        .split("<button")
        .skip(1)
        .find(|button| {
            button
                .split('>')
                .next()
                .unwrap()
                .contains("type=\"submit\"")
        })
        .unwrap();
    assert!(!first_submit
        .split('>')
        .next()
        .unwrap()
        .contains("page-owners"));
    assert!(first_submit.contains("Create repository"));
    assert!(html.contains("/new?org_limit=1"));
    assert!(html.contains("action=\"/new?org_after=2&#38;org_limit=1\""));
    let option = html
        .split("<option")
        .find(|option| option.split('>').next().unwrap().contains("value=\"4\""))
        .unwrap();
    assert!(option.split('>').next().unwrap().contains("selected"));
}

#[test]
fn creation_form_preserves_values_csrf_and_visibility_without_inert_controls() {
    let html = dioxus_ssr::render_element(new_repository(
        "alice".into(),
        "csrf-test".into(),
        RepositoryCreationChoices::default(),
        NewRepositoryForm {
            name: "bad/name\"".into(),
            description: "<script>".into(),
            is_private: true,
            owner_org_id: "99".into(),
            ..Default::default()
        },
        Some(FormError {
            field: None,
            message: "Repository already exists".into(),
        }),
    ));
    assert!(html.contains("method=\"post\""));
    assert!(html.contains("action=\"/new\""));
    assert!(html.contains("name=\"owner_org_id\""));
    assert!(html.contains("Selected organization unavailable on this page"));
    assert!(html.contains("Only organizations where you are an owner or admin are listed."));
    assert!(!html.contains("governance"));
    assert!(html.contains("Cancel"));
    assert!(html.contains("value=\"99\""));
    assert!(html.contains("name=\"csrf_token\""));
    assert!(html.contains("csrf-test"));
    assert!(html.contains("bad/name&quot;") || html.contains("bad/name&#34;"));
    assert!(html.contains("&#60;script&#62;"));
    let private_input = html
        .split("<input")
        .find(|input| {
            input
                .split('>')
                .next()
                .is_some_and(|attributes| attributes.contains("value=\"private\""))
        })
        .unwrap();
    assert!(private_input.split('>').next().unwrap().contains("checked"));
    assert!(html.contains("Repository already exists"));
    assert!(!html.contains("disabled"));
}

#[test]
fn creation_form_offers_initial_files_and_keeps_their_drafts() {
    let html = dioxus_ssr::render_element(new_repository(
        "alice".into(),
        "csrf-test".into(),
        RepositoryCreationChoices::default(),
        NewRepositoryForm {
            readme: true,
            gitignore: "Rust".into(),
            license: "MIT".into(),
            ..Default::default()
        },
        None,
    ));
    // The opening tag of the element that holds `marker`.
    let opening_tag = |marker: &str| {
        let at = html.find(marker).unwrap();
        let start = html[..at].rfind('<').unwrap();
        html[start..].split('>').next().unwrap().to_owned()
    };
    assert!(html.contains("Add a README file"));
    assert!(opening_tag("name=\"readme\"").contains("checked"));
    assert!(opening_tag("value=\"Rust\"").contains("selected"));
    assert!(opening_tag("value=\"MIT\"").contains("selected"));
    assert!(!opening_tag("value=\"Node\"").contains("selected"));
    assert!(html.contains("No .gitignore") && html.contains("No license"));
    for name in GITIGNORE_CHOICES.iter().chain(&LICENSE_CHOICES) {
        assert!(html.contains(&format!("value=\"{name}\"")), "{name}");
    }
}

#[test]
fn visibility_choices_are_icon_cards_with_one_line_explanations() {
    let html = dioxus_ssr::render_element(new_repository(
        "alice".into(),
        "csrf-test".into(),
        RepositoryCreationChoices::default(),
        NewRepositoryForm::default(),
        None,
    ));
    let visibility = html
        .split("collaboration-option-group")
        .nth(1)
        .unwrap()
        .split("</fieldset>")
        .next()
        .unwrap();
    assert_eq!(visibility.matches("<svg").count(), 2);
    assert!(visibility
        .contains("Anyone who can reach this server can see it. You choose who can commit."));
    assert!(visibility.contains("You choose who can see and commit to this repository."));
}

#[test]
fn explore_keeps_its_filters_and_pages_through_the_results() {
    let page = |page: usize, has_older: bool| {
        crate::webui::tests::in_scope(|| {
            dioxus_ssr::render_element(explore(ExploreModel {
                sort: "stars".into(),
                query: "Rust & Git".into(),
                language: String::new(),
                topic: String::new(),
                kind: String::new(),
                languages: vec![],
                page,
                has_older,
                repositories: vec![RepositoryCard {
                    owner: "alice".into(),
                    name: "demo".into(),
                    description: Some("Rust & Git".into()),
                    ..Default::default()
                }],
            }))
        })
    };
    let second = page(2, true);
    assert!(second.contains("method=\"get\""));
    assert!(second.contains("action=\"/explore\""));
    assert!(second.contains("Rust &#38; Git"));
    assert!(second.contains("Repositories matching “Rust &#38; Git”"));
    assert!(second.contains("Showing 1 repository"));
    assert!(!second.contains("Public repositories"));
    assert!(second.contains("/alice/demo"));
    // The page size is fixed, so no size control is offered. Newer leads to page 1, which has
    // no page number, and Older leads to page 3.
    assert!(!second.contains("name=\"limit\""));
    assert!(!second.contains("Show more repositories"));
    assert!(second.contains("Newer"));
    assert!(second.contains("Older"));
    assert!(second.contains("page=3"));
    assert!(!second.contains("page=1"));
    let first = page(1, false);
    assert!(!first.contains("Newer"));
    assert!(!first.contains("Older"));
}

#[test]
fn explore_topic_filter_is_a_removable_chip_and_cards_link_their_topics() {
    let html = crate::webui::tests::in_scope(|| {
        dioxus_ssr::render_element(explore(ExploreModel {
            sort: "stars".into(),
            query: "Rust".into(),
            language: String::new(),
            topic: "git".into(),
            kind: String::new(),
            languages: vec![],
            page: 1,
            has_older: false,
            repositories: vec![RepositoryCard {
                owner: "alice".into(),
                name: "demo".into(),
                topics: vec!["forge".into()],
                ..Default::default()
            }],
        }))
    });
    // The search form carries the topic, and the chip's remove link drops only the topic.
    assert!(html.contains("name=\"topic\" value=\"git\""));
    assert!(html.contains("aria-label=\"Topic filter\""));
    assert!(html.contains("aria-label=\"Remove topic git\""));
    assert!(html.contains("q=Rust"));
    assert!(!html.contains("topic=git"));
    // Each card links its topics to their Explore list.
    assert!(html.contains("href=\"/explore?topic=forge\""));
}

#[test]
fn demo_routes_have_real_bidirectional_links_without_developer_scaffolding() {
    let html = dioxus_ssr::render_element(demo(false));
    assert!(html.contains("/demo/no-nav"));
    assert!(html.contains("Codeyard interface"));
    assert!(!html.contains("transition library"));
    let html = dioxus_ssr::render_element(demo(true));
    assert!(html.contains("href=\"/demo\""));
    assert!(html.contains("Focused view"));
}

#[test]
fn home_has_real_content_before_any_client_runtime() {
    let html = dioxus_ssr::render_element(home(HomeModel::default()));
    assert!(html.contains("Git hosting, written in Rust."));
    assert!(html.contains("src=\"/assets/logo.webp\""));
    assert!(html.contains("width=\"512\""));
    assert!(html.contains("height=\"512\""));
    assert!(html.contains("href=\"/register\""));
    assert!(!html.contains("Loading"));
    let html = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        repositories: vec![RepositoryCard {
            owner: "alice".into(),
            name: "private".into(),
            is_private: true,
            ..Default::default()
        }],
        ..Default::default()
    }));
    assert!(html.contains("Your repositories"));
    assert!(html.contains("/alice/private"));
    assert!(html.contains("Private"));
}

#[test]
fn dashboard_places_all_owned_repositories_before_secondary_trending() {
    let repositories = (1..=12)
        .map(|number| RepositoryCard {
            id: i64::from(number),
            owner: "alice".into(),
            name: format!("owned-{number}"),
            description: Some(format!("Project {number}")),
            topics: Vec::new(),
            is_private: number == 1,
            stars: number,
            forks: 2,
            default_branch: "main".into(),
            primary_language: None,
            updated_at: "2026-10-02".into(),
        })
        .collect();
    let html = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        repositories,
        trending: vec![RepositoryCard {
            owner: "bob".into(),
            name: "popular".into(),
            stars: 20,
            ..Default::default()
        }],
        ..Default::default()
    }));
    let primary_heading = html.find("id=\"dashboard-repositories-title\"").unwrap();
    let first_owned = html.find("href=\"/alice/owned-1\"").unwrap();
    let last_owned = html.find("href=\"/alice/owned-12\"").unwrap();
    let sidebar = html.find("<aside").unwrap();
    let trending = html.find("href=\"/bob/popular\"").unwrap();
    assert!(primary_heading < first_owned);
    assert!(first_owned < last_owned);
    assert!(last_owned < sidebar);
    assert!(sidebar < trending);
    assert!(html[..sidebar].contains("<h1"));
    assert!(html[..sidebar].contains("href=\"/new\""));
    assert!(html.contains("aria-label=\"12 repositories\""));
    assert_eq!(
        html.matches("class=\"discovery-repository-row\"").count(),
        12
    );
    for number in 1..=12 {
        assert!(html.contains(&format!("href=\"/alice/owned-{number}\"")));
        assert!(html.contains(&format!("Project {number}")));
    }
    assert!(html.contains("Private"));
    assert!(html.contains("Public"));
    assert!(html.contains("Branch main"));
    assert!(html.contains("Updated Oct 2, 2026"));
    assert!(html.contains("2 forks"));
    assert!(html[sidebar..].contains("href=\"/explore?sort=stars\""));
    assert!(html.contains("Most starred of the 100 newest public repositories."));
    for rejected_panel in ["Welcome back", "Personal account", "recent activity"] {
        assert!(!html.contains(rejected_panel));
    }
}

#[test]
fn trending_is_not_hidden_below_the_side_panel_width() {
    // Below 960px the trending panel stacks under the repository list instead of disappearing.
    let css = include_str!("../../input.css");
    assert!(
        !css.contains(".discovery-dashboard .discovery-dashboard-trending {\n\tdisplay: none;"),
        "trending is hidden at every width"
    );
}

#[test]
fn dashboard_empty_states_offer_real_actions_without_fabricated_rows() {
    let html = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        ..Default::default()
    }));
    assert!(html.contains("Your repositories"));
    assert!(html.contains("aria-label=\"0 repositories\""));
    assert!(html.contains("No repositories yet"));
    assert!(html.contains("href=\"/new\""));
    assert!(html.contains("Create a repository"));
    assert!(html.contains("href=\"/explore\""));
    assert!(!html.contains("class=\"discovery-repository-row\""));
    assert!(!html.contains("class=\"discovery-trending-list\""));
    assert!(!html.contains("Loading"));
}

#[test]
fn dashboard_without_trending_drops_the_column_and_counts_link_to_their_lists() {
    let html = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        assigned_issues: 2,
        open_pull_requests: 1,
        ..Default::default()
    }));
    // Nothing is trending, so there is no Trending column, and the work takes the full width.
    assert!(html.contains("discovery-dashboard-solo"), "{html}");
    assert!(!html.contains("<aside"), "{html}");
    assert!(!html.contains("Trending"), "{html}");
    assert!(
        !html.contains("Nothing trending from other people yet."),
        "{html}"
    );
    // The two counts open the matching filtered lists.
    assert!(html.contains("href=\"/issues?tab=assigned"), "{html}");
    assert!(html.contains("href=\"/pulls?tab=involved"), "{html}");
    let with_trending = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        trending: vec![RepositoryCard {
            owner: "bob".into(),
            name: "popular".into(),
            ..Default::default()
        }],
        ..Default::default()
    }));
    assert!(!with_trending.contains("discovery-dashboard-solo"));
    assert!(with_trending.contains("<aside"));
}

#[test]
fn dashboard_keeps_owned_empty_state_primary_when_trending_has_data() {
    let html = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        trending: vec![RepositoryCard {
            owner: "bob".into(),
            name: "popular".into(),
            ..Default::default()
        }],
        ..Default::default()
    }));
    let empty = html.find("No repositories yet").unwrap();
    let trending = html.find("href=\"/bob/popular\"").unwrap();
    assert!(empty < trending);
    assert!(html.contains("href=\"/new\""));
    assert!(!html.contains("Nothing trending from other people yet."));
    assert!(!html.contains("class=\"discovery-repository-row\""));
}

#[test]
fn dashboard_shares_collaborator_repositories_with_a_role_only_when_there_are_some() {
    let html = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        ..Default::default()
    }));
    assert!(!html.contains("Shared with you"));
    let html = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        shared: vec![SharedRepositoryCard {
            repository: RepositoryCard {
                owner: "bob".into(),
                name: "tool".into(),
                description: Some("Shared tooling".into()),
                ..Default::default()
            },
            role: "Write".into(),
        }],
        ..Default::default()
    }));
    let shared = html.find("Shared with you").unwrap();
    assert!(html[shared..].contains("href=\"/bob/tool\""));
    assert!(html[shared..].contains("Shared tooling"));
    assert!(html[shared..].contains("Write"));
    assert!(html[..shared].contains("No repositories yet"));
}

#[test]
fn dashboard_escapes_repository_fields_and_encodes_links_in_both_columns() {
    let malicious = RepositoryCard {
        owner: "a/b".into(),
        name: "x\"<script>".into(),
        description: Some("<img src=x onerror=alert(1)>".into()),
        default_branch: "<script>alert(1)</script>".into(),
        updated_at: "2026-10-02\" onmouseover=\"alert(1)".into(),
        ..Default::default()
    };
    let html = dioxus_ssr::render_element(home(HomeModel {
        username: Some("alice".into()),
        repositories: vec![malicious.clone()],
        trending: vec![malicious],
        ..Default::default()
    }));
    assert_eq!(html.matches("href=\"/a%2Fb/x%22%3Cscript%3E\"").count(), 2);
    assert!(html.contains("&#60;script&#62;"));
    assert!(html.contains("&#60;img"));
    assert!(html.contains("&#34;") || html.contains("&quot;"));
    assert!(!html.contains("<script>"));
    assert!(!html.contains("<img src=x"));
    assert!(!html.contains(" onmouseover=\"alert(1)\""));
}
