use super::*;
use crate::webui::pages::users::Repository;
use dioxus::prelude::VirtualDom;
use std::collections::HashMap;

fn team() -> Team {
    Team {
        id: 9,
        name: "Writers".into(),
        slug: "writers".into(),
        description: "Writes the docs".into(),
        privacy: "visible".into(),
        member_count: 2,
    }
}

fn sample_page(draft: HashMap<String, String>, repositories: Vec<RepositoryChoice>) -> Page {
    Page {
        csrf_token: "csrf-fixture".into(),
        organizations: vec![],
        selected: Some(Organization {
            id: 3,
            name: "native_org".into(),
            display_name: Some("Native org".into()),
            description: String::new(),
            suspended: false,
            avatar_url: None,
        }),
        members: vec![],
        teams: vec![team()],
        repositories,
        more_repositories: false,
        selected_team: Some(TeamDetail {
            team: team(),
            slug: "writers".into(),
            description: String::new(),
            members: vec![],
            repositories: vec![RepositoryGrant {
                repo_id: 7,
                owner: "native_org".into(),
                name: "widgets".into(),
                permission: 3,
            }],
            more_members: false,
            more_repositories: false,
            members_after: 0,
            repositories_after: 0,
            members_cursor: 0,
            repositories_cursor: 0,
        }),
        can_recover: false,
        ownerless_organizations: vec![],
        owned_repositories: 0,
        invitations: vec![],
        pending_invitations: vec![],
        invitations_next: None,
        pending_invitations_next: None,
        ownerless_next: None,
        notice: None,
        can_manage: true,
        is_owner: true,
        viewer_id: 0,
        manage_access: false,
        more_organizations: false,
        more_members: false,
        more_teams: false,
        after: 0,
        members_after: 0,
        teams_after: 0,
        members_cursor: 0,
        teams_cursor: 0,
        tab: "teams".into(),
        webhooks: None,
        message: None,
        draft,
    }
}

fn draft(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn html(page: Page) -> String {
    let mut dom = VirtualDom::new_with_props(render, page);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

/// Every rendered `<select>` whose opening tag names `name`, in document order.
fn selects<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let marker = format!("name=\"{name}\"");
    html.match_indices("<select")
        .map(|(start, _)| {
            let end = start + html[start..].find("</select>").expect("select closes");
            &html[start..end]
        })
        .filter(|block| {
            let open = &block[..block.find('>').expect("select tag closes")];
            open.contains(&marker)
        })
        .collect()
}

/// The `(value, label, selected)` of each option in a rendered select.
fn choices(block: &str) -> Vec<(String, String, bool)> {
    block
        .split("<option")
        .skip(1)
        .map(|part| {
            let close = part.find('>').expect("option tag closes");
            let open = &part[..close];
            let value = open
                .split("value=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .expect("option has a value");
            let label = part[close + 1..]
                .split("</option>")
                .next()
                .unwrap_or_default();
            (
                value.to_owned(),
                label.to_owned(),
                open.contains("selected"),
            )
        })
        .collect()
}

fn assert_labels(block: &str, expected: &[(&str, &str)]) {
    let actual = choices(block);
    assert_eq!(actual.len(), expected.len(), "{block}");
    for ((value, label, _), (want_value, want_label)) in actual.iter().zip(expected) {
        assert_eq!(value.as_str(), *want_value, "{block}");
        assert!(label.contains(*want_label), "{label} is not {want_label}");
    }
}

fn selected(block: &str, value: &str) -> bool {
    choices(block)
        .iter()
        .any(|(option, _, selected)| option.as_str() == value && *selected)
}

#[test]
fn the_new_and_edited_team_descriptions_have_their_own_ids() {
    // Each description is a separate draft, so the two forms must not share an id or a name.
    let page = html(sample_page(HashMap::new(), Vec::new()));
    assert!(page.contains("id=\"team-description\""), "{page}");
    assert!(page.contains("id=\"team-edit-description\""), "{page}");
}

#[test]
fn fixed_choices_are_selects_with_human_labels() {
    // With a repository the grant form renders, so it has its own access select.
    let repositories = vec![RepositoryChoice {
        repo_id: 12,
        owner: "native_org".into(),
        name: "widgets".into(),
    }];
    let page = html(sample_page(HashMap::new(), repositories));
    let privacy = selects(&page, "privacy");
    assert_eq!(privacy.len(), 2, "the create and edit team forms");
    for block in &privacy {
        assert_labels(
            block,
            &[
                ("visible", "Visible to organization members"),
                ("secret", "Only team members"),
            ],
        );
    }
    assert_labels(
        team_role_select(&page),
        &[("member", "Member"), ("maintainer", "Maintainer")],
    );
    let access = selects(&page, "permission");
    assert_eq!(access.len(), 2, "one grant row and the grant form");
    for block in &access {
        assert_labels(
            block,
            &[
                ("1", "Read"),
                ("2", "Triage"),
                ("3", "Write"),
                ("4", "Maintain"),
                ("5", "Admin"),
            ],
        );
    }
    // A grant row starts at the level it already has, and the edit form at the stored privacy.
    assert!(selected(access[0], "3"));
    assert!(selected(privacy[1], "visible"));
}

fn public_profile(
    manage: bool,
    member_count: Option<i64>,
    repositories: Vec<Repository>,
) -> OrganizationProfile {
    OrganizationProfile {
        organization: Organization {
            id: 3,
            name: "acme".into(),
            display_name: None,
            description: "Widgets & tools".into(),
            suspended: false,
            avatar_url: None,
        },
        repository_count: repositories.len() as i64,
        repositories,
        member_count,
        team_count: member_count.map(|_| 2),
        manage,
        tab: "repositories".into(),
        people: Vec::new(),
        teams: Vec::new(),
        more: false,
        after: 0,
    }
}

fn listed(name: &str) -> Repository {
    Repository {
        name: name.into(),
        description: String::new(),
        private: false,
        archived: false,
        stars: 0,
        forks: 0,
        updated: "Oct 2, 2026".into(),
    }
}

#[test]
fn public_organization_page_links_management_only_for_members() {
    let profile_html = |page: OrganizationProfile| {
        let mut dom = VirtualDom::new_with_props(profile_page, page);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    };
    let owner = profile_html(public_profile(true, Some(3), vec![listed("widgets")]));
    assert!(owner.contains("@acme"));
    assert!(owner.contains("Widgets &#38; tools"));
    assert!(owner.contains("href=\"/acme/widgets\""));
    assert!(owner.contains("href=\"/settings/organizations/3\""));
    assert!(owner.contains("Manage organization"));
    assert!(owner.contains("1 repository · 3 members"));

    // An owner of an empty organization gets the new-repository page for this organization.
    let owner_empty = profile_html(public_profile(true, Some(3), vec![]));
    assert!(owner_empty.contains("This organization has no repositories yet."));
    assert!(owner_empty.contains("href=\"/new?owner_org_id=3\""));
    assert!(!owner_empty.contains("No repositories visible to you."));

    // A signed-in reader who is not a manager sees the repositories and no management link.
    let reader = profile_html(public_profile(false, None, vec![]));
    assert!(reader.contains("No public repositories yet."));
    assert!(!reader.contains("No repositories visible to you."));
    assert!(!reader.contains("/new?owner_org_id=3"));
    assert!(!reader.contains("Manage organization"));
    assert!(!reader.contains("/settings/organizations/3"));
    assert!(reader.contains("0 repositories · Public organization"));
    assert!(!reader.contains(" members"));

    // Anonymous visitors get the public wording, and the same two-part meta line without a count.
    let visitor = profile_html(public_profile(false, None, vec![]));
    assert!(visitor.contains("@acme"));
    assert!(visitor.contains("0 repositories · Public organization"));
    assert!(visitor.contains("No public repositories yet."));
    // The empty repository state is a centred block with the repository icon.
    assert!(visitor.contains("identity-profile-empty"));
    assert!(visitor.contains("<svg"));
    assert!(!visitor.contains("No repositories visible to you."));
    assert!(!visitor.contains("Manage organization"));
    assert!(!visitor.contains("/settings/organizations/3"));
}

#[test]
fn public_organization_tabs_follow_the_viewer() {
    let profile_html = |page: OrganizationProfile| {
        let mut dom = VirtualDom::new_with_props(profile_page, page);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    };

    // Owners and members get People and Teams after Repositories, with counts where cheap.
    let owner = profile_html(public_profile(true, Some(3), vec![listed("widgets")]));
    let repositories_tab = opening_tag(&owner, "href=\"/acme?tab=repositories\"");
    assert!(repositories_tab.contains("aria-current=\"page\""));
    assert!(!opening_tag(&owner, "href=\"/acme?tab=people\"").contains("aria-current"));
    assert!(owner.contains("href=\"/acme?tab=people\""));
    assert!(owner.contains("href=\"/acme?tab=teams\""));
    assert!(owner.contains("count-badge\">3</span>"));
    // The Teams count is the number of teams the owner can see, and the tab headings repeat nothing.
    let teams_tab = owner
        .split("href=\"/acme?tab=teams\"")
        .nth(1)
        .unwrap()
        .split("</a>")
        .next()
        .unwrap();
    assert!(teams_tab.contains("count-badge\">2</span>"));
    assert!(!owner.contains(">Repositories</h2>"));

    let member = profile_html(public_profile(false, Some(3), vec![]));
    assert!(member.contains("href=\"/acme?tab=people\""));
    assert!(member.contains("href=\"/acme?tab=teams\""));
    assert!(!member.contains("Manage organization"));

    // Visitors and signed-in non-members get no People or Teams tab at all.
    let visitor = profile_html(public_profile(false, None, vec![]));
    assert!(visitor.contains("href=\"/acme?tab=repositories\""));
    assert!(!visitor.contains("?tab=people"));
    assert!(!visitor.contains("?tab=teams"));
    let outsider = profile_html(public_profile(false, None, vec![]));
    assert!(!outsider.contains("?tab=people"));
    assert!(!outsider.contains("?tab=teams"));

    // The People tab lists members with links, roles and a cursor for the next page.
    let mut people = public_profile(false, Some(2), vec![]);
    people.tab = "people".into();
    people.people = vec![
        Member {
            user_id: 7,
            username: "ada".into(),
            role: "owner".into(),
        },
        Member {
            user_id: 8,
            username: "grace".into(),
            role: "member".into(),
        },
    ];
    people.more = true;
    people.after = 8;
    let people = profile_html(people);
    assert!(opening_tag(&people, "href=\"/acme?tab=people\"").contains("aria-current=\"page\""));
    assert!(people.contains("href=\"/ada\""));
    assert!(people.contains("href=\"/grace\""));
    assert!(people.contains("class=\"org-label\">owner</span>"));
    assert!(people.contains("href=\"/acme?tab=people&#38;after=8\""));
    assert!(!people.contains(">People</h2>"));
    assert!(!people.contains("No repositories visible to you."));

    // The Teams tab links each team to its page and says so when there are none.
    let mut teams = public_profile(false, Some(2), vec![]);
    teams.tab = "teams".into();
    teams.teams = vec![Team {
        id: 5,
        name: "Reviewers".into(),
        slug: "reviewers".into(),
        description: "Reviews code.".into(),
        privacy: "visible".into(),
        member_count: 2,
    }];
    let teams_html = profile_html(teams.clone());
    assert!(opening_tag(&teams_html, "href=\"/acme?tab=teams\"").contains("aria-current=\"page\""));
    assert!(teams_html.contains("href=\"/orgs/acme/teams/reviewers\""));
    assert!(teams_html.contains("Reviews code."));
    assert!(!teams_html.contains("No teams yet."));
    assert!(teams_html.contains("Visible to organization members"));
    assert!(!teams_html.contains(">Teams</h2>"));
    teams.teams.clear();
    // An empty Teams tab uses the same centred empty state as repositories.
    let empty = profile_html(teams);
    assert!(empty.contains("No teams yet."));
    assert!(empty.contains("identity-profile-empty"));
}

#[test]
fn grant_form_lists_repositories_by_name_and_posts_their_ids() {
    let repositories = vec![
        RepositoryChoice {
            repo_id: 12,
            owner: "native_org".into(),
            name: "widgets".into(),
        },
        RepositoryChoice {
            repo_id: 15,
            owner: "native_org".into(),
            name: "docs".into(),
        },
    ];
    let page = html(sample_page(HashMap::new(), repositories));
    let repo = selects(&page, "repo_id");
    assert_eq!(
        repo.len(),
        1,
        "the grant form is the only repository select"
    );
    assert_labels(
        repo[0],
        &[
            ("", "Choose a repository"),
            ("12", "native_org/widgets"),
            ("15", "native_org/docs"),
        ],
    );
    // An organization without repositories gets a note instead of an empty select.
    let empty = html(sample_page(HashMap::new(), vec![]));
    assert!(selects(&empty, "repo_id").is_empty());
    assert!(empty.contains("No repositories are available to grant."));
}

#[test]
fn rejected_submission_reselects_its_choices() {
    let repositories = vec![RepositoryChoice {
        repo_id: 15,
        owner: "native_org".into(),
        name: "docs".into(),
    }];
    let created = html(sample_page(
        draft(&[("action", "team"), ("privacy", "secret")]),
        vec![],
    ));
    assert!(selected(selects(&created, "privacy")[0], "secret"));
    // The team edit form keeps the team's stored privacy.
    assert!(selected(selects(&created, "privacy")[1], "visible"));

    let added = html(sample_page(
        draft(&[("action", "team-member"), ("role", "maintainer")]),
        vec![],
    ));
    assert!(selected(team_role_select(&added), "maintainer"));

    let granted = html(sample_page(
        draft(&[
            ("action", "grant"),
            ("team_id", "9"),
            ("repo_id", "15"),
            ("permission", "4"),
        ]),
        repositories,
    ));
    assert!(selected(selects(&granted, "repo_id")[0], "15"));
    let access = selects(&granted, "permission");
    // The row for repository 7 keeps its own level; the grant form shows the draft.
    assert!(selected(access[0], "3"));
    assert!(selected(access[1], "4"));
}

/// The team's role select. Member roles also use `role`, so it is found by its options.
fn team_role_select(html: &str) -> &str {
    selects(html, "role")
        .into_iter()
        .find(|block| block.contains("value=\"maintainer\""))
        .expect("the team role select")
}

/// Whether a visible (not hidden) `<input>` named `name` is rendered.
fn has_visible_input(html: &str, name: &str) -> bool {
    let marker = format!("name=\"{name}\"");
    html.split("<input")
        .skip(1)
        .map(|tag| tag.split('>').next().unwrap_or_default())
        .any(|tag| tag.contains(&marker) && !tag.contains("type=\"hidden\""))
}

fn repositories(count: i64) -> Vec<RepositoryChoice> {
    (1..=count)
        .map(|repo_id| RepositoryChoice {
            repo_id,
            owner: "native_org".into(),
            name: format!("repo_{repo_id:03}"),
        })
        .collect()
}

#[test]
fn grant_form_takes_an_id_only_past_one_page() {
    let limit_note = "enter the repository ID.";
    // Under the limit, and at it with nothing after it, the grant form keeps the select.
    let under = html(sample_page(HashMap::new(), repositories(99)));
    assert_eq!(selects(&under, "repo_id").len(), 1);
    assert!(!under.contains(limit_note));
    let full = html(sample_page(
        HashMap::new(),
        repositories(REPOSITORY_CHOICE_LIMIT),
    ));
    assert_eq!(selects(&full, "repo_id").len(), 1);
    assert!(!full.contains(limit_note));
    // A page with more repositories after it takes the ID and says why.
    let mut past = sample_page(HashMap::new(), repositories(REPOSITORY_CHOICE_LIMIT));
    past.more_repositories = true;
    let past = html(past);
    assert!(selects(&past, "repo_id").is_empty());
    assert!(has_visible_input(&past, "repo_id"));
    assert!(
        past.contains("This organization has more than 100 repositories; enter the repository ID.")
    );
}

#[test]
fn member_roles_are_selects_that_keep_a_rejected_choice() {
    let members = vec![
        Member {
            user_id: 5,
            username: "alice".into(),
            role: "owner".into(),
        },
        Member {
            user_id: 6,
            username: "bob".into(),
            role: "member".into(),
        },
    ];
    let with_members = |draft: HashMap<String, String>| {
        let mut page = sample_page(draft, vec![]);
        // The team panel has its own role select; this test reads the member rows only.
        page.selected_team = None;
        page.tab = "members".into();
        page.members = members.clone();
        html(page)
    };
    let page = with_members(HashMap::new());
    let roles = selects(&page, "role");
    assert_eq!(roles.len(), 3, "two member rows and the add-member form");
    for block in &roles {
        assert_labels(
            block,
            &[("member", "Member"), ("admin", "Admin"), ("owner", "Owner")],
        );
    }
    // The add-member form comes first, then one row per member in list order.
    assert!(selected(roles[1], "owner"));
    assert!(selected(roles[2], "member"));
    // A rejected change keeps the role it was submitted with, on its own row only.
    let page = with_members(draft(&[
        ("action", "member-role"),
        ("user_id", "6"),
        ("role", "admin"),
    ]));
    let roles = selects(&page, "role");
    assert!(selected(roles[1], "owner"));
    assert!(selected(roles[2], "admin"));
    // A value that is not a role is not offered, so the row shows its stored role.
    let page = with_members(draft(&[
        ("action", "member-role"),
        ("user_id", "6"),
        ("role", "invented"),
    ]));
    assert!(selected(selects(&page, "role")[2], "member"));
}

/// The organizations index, with the next-page and recovery state under test.
fn organization_index(more_organizations: bool, can_recover: bool) -> String {
    let mut page = sample_page(HashMap::new(), vec![]);
    page.selected = None;
    page.selected_team = None;
    page.after = 25;
    page.more_organizations = more_organizations;
    page.can_recover = can_recover;
    if can_recover {
        page.ownerless_organizations = vec![Organization {
            id: 9,
            name: "lost_org".into(),
            display_name: None,
            description: String::new(),
            suspended: true,
            avatar_url: None,
        }];
    }
    html(page)
}

#[test]
fn organization_index_counts_in_one_wording() {
    let output = organization_index(false, false);
    assert!(output.contains("0 organizations"));
    assert!(!output.contains("Shown"));
    assert!(!output.contains("Next organizations"));
}

#[test]
fn organization_index_links_onward_only_when_another_page_exists() {
    assert!(!organization_index(false, false).contains(">Next page<"));
    assert!(organization_index(true, false).contains(">Next page<"));
}

#[test]
fn member_and_team_lists_link_onward_only_when_another_page_exists() {
    let fits = html(sample_page(HashMap::new(), vec![]));
    assert!(!fits.contains(">Next members<"));
    assert!(!fits.contains(">Next teams<"));
    let mut more = sample_page(HashMap::new(), vec![]);
    more.more_members = true;
    more.more_teams = true;
    let teams = html(more.clone());
    assert!(teams.contains(">Next teams<"));
    assert!(!teams.contains(">Next members<"));
    more.tab = "members".into();
    let members = html(more);
    assert!(members.contains(">Next members<"));
    assert!(!members.contains(">Next teams<"));
}

#[test]
fn team_member_and_repository_lists_link_onward_only_when_another_page_exists() {
    let fits = html(sample_page(HashMap::new(), vec![]));
    assert!(!fits.contains(">Next team members<"));
    assert!(!fits.contains(">Next team repositories<"));
    let mut more = sample_page(HashMap::new(), vec![]);
    let detail = more
        .selected_team
        .as_mut()
        .expect("sample page selects a team");
    detail.more_members = true;
    detail.more_repositories = true;
    let output = html(more);
    assert!(output.contains(">Next team members<"));
    assert!(output.contains(">Next team repositories<"));
}

#[test]
fn recovery_is_a_visible_danger_card_for_site_administrators_only() {
    let admin = organization_index(false, true);
    assert!(admin.contains("Recover an organization without an owner"));
    assert!(admin.contains("org-danger"));
    assert!(admin.contains("btn btn-destructive"));
    assert!(admin.contains("action=\"/admin/organizations/recovery\""));
    assert!(admin.contains("name=\"org_id\""));
    assert!(admin.contains("value=\"9\""));
    assert!(admin.contains("name=\"username\""));
    assert!(admin.contains("name=\"csrf_token\""));
    assert!(!admin.contains("name=\"user_id\""));

    let member = organization_index(false, false);
    assert!(!member.contains("Recover an organization"));
    assert!(!member.contains("/admin/organizations/recovery"));
}

#[test]
fn recovery_panel_is_hidden_when_no_organization_is_ownerless() {
    let mut page = sample_page(HashMap::new(), vec![]);
    page.selected = None;
    page.selected_team = None;
    page.can_recover = true;
    let output = html(page);
    assert!(!output.contains("Recover an organization"));
    assert!(!output.contains("/admin/organizations/recovery"));
}

fn team_page_model(
    show_repositories: bool,
    members: Vec<Member>,
    repositories: Vec<RepositoryGrant>,
    more: bool,
) -> TeamPage {
    TeamPage {
        organization: Organization {
            id: 3,
            name: "acme".into(),
            display_name: Some("Acme Labs".into()),
            description: String::new(),
            suspended: false,
            avatar_url: None,
        },
        team: team(),
        description: "Ships docs and copy.".into(),
        member_count: 1,
        repository_count: 1,
        manage: true,
        show_repositories,
        members,
        repositories,
        more,
        after: 42,
    }
}

fn team_page_html(page: TeamPage) -> String {
    let mut dom = VirtualDom::new_with_props(team_page, page);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

/// The opening tag that contains `marker`, so a test can read the attributes of one element.
fn opening_tag<'a>(html: &'a str, marker: &str) -> &'a str {
    let at = html.find(marker).expect("marker is rendered");
    let start = html[..at].rfind('<').expect("marker is inside a tag");
    let end = at + html[at..].find('>').expect("tag closes");
    &html[start..end]
}

#[test]
fn team_cards_show_the_team_description() {
    let mut page = sample_page(HashMap::new(), vec![]);
    page.tab = "teams".into();
    let output = html(page);
    assert!(output.contains("class=\"org-row-description\""));
    assert!(output.contains("Writes the docs"));
}

#[test]
fn organization_sections_follow_the_url_without_scripts() {
    let tabs = [
        ("overview", "organization-overview"),
        ("members", "organization-members"),
        ("teams", "organization-teams"),
    ];
    for (tab, _) in tabs {
        let mut page = sample_page(HashMap::new(), vec![]);
        page.tab = tab.into();
        let output = html(page);
        for (name, section) in tabs {
            let link = opening_tag(
                &output,
                &format!("href=\"/settings/organizations/3?tab={name}\""),
            );
            assert_eq!(
                link.contains("aria-current=\"page\""),
                name == tab,
                "{link}"
            );
            let marker = format!("id=\"{section}\"");
            assert_eq!(output.contains(&marker), name == tab, "{tab}: {section}");
            if name == tab {
                assert!(!opening_tag(&output, &marker).contains("hidden"));
            }
        }
    }
}

#[test]
fn public_team_page_shows_header_members_and_manage_link() {
    let member = Member {
        user_id: 5,
        username: "ada".into(),
        role: "maintainer".into(),
    };
    let output = team_page_html(team_page_model(false, vec![member], vec![], false));
    assert!(output.contains("Acme Labs"));
    assert!(output.contains("Writers"));
    assert!(output.contains(">People</h2>"));
    // The team name is the only heading; the organization is the breadcrumb above it.
    assert_eq!(output.matches("<h1").count(), 1);
    let heading = output
        .split("<h1")
        .nth(1)
        .unwrap()
        .split("</h1>")
        .next()
        .unwrap();
    assert!(heading.contains("Writers"));
    assert!(!heading.contains("Acme Labs"));
    assert!(output.contains("aria-label=\"Organization\""));
    assert!(output.contains("Ships docs and copy."));
    assert!(output.contains("href=\"/acme\""));
    assert!(output.contains("href=\"/orgs/acme/teams/writers?tab=repositories\""));
    assert!(output.contains("href=\"/ada\""));
    assert!(output.contains("maintainer"));
    assert!(output.contains("href=\"/settings/organizations/3?tab=teams"));
    assert!(output.contains("team=9\""));
    assert!(output.contains("Manage team"));
    // Only the active tab carries aria-current, and a short page has no Next link. The inline
    // stylesheets also name aria-current, so count inside the tab list only.
    let tabs = output
        .split_once("class=\"org-team-tabs\"")
        .unwrap()
        .1
        .split_once("</nav>")
        .unwrap()
        .0;
    assert_eq!(tabs.matches("aria-current=\"page\"").count(), 1);
    assert!(!output.contains("class=\"org-pagination\""));
    assert!(!output.contains("href=\"/acme/tools\""));
}

#[test]
fn public_team_page_repositories_tab_lists_grants_and_links_onward_when_full() {
    let grant = RepositoryGrant {
        repo_id: 7,
        owner: "acme".into(),
        name: "tools".into(),
        permission: 3,
    };
    let output = team_page_html(team_page_model(true, vec![], vec![grant], true));
    assert!(output.contains("href=\"/acme/tools\""));
    assert!(output.contains("Write"));
    assert!(output.contains("href=\"/orgs/acme/teams/writers?tab=members\""));
    assert!(output.contains("class=\"org-pagination\""));
    assert!(output.contains("after=42"));
    assert!(!output.contains("No members to show"));
}

#[test]
fn public_team_page_empty_states_and_hidden_manage_link() {
    let mut page = team_page_model(false, vec![], vec![], false);
    page.manage = false;
    let members = team_page_html(page.clone());
    assert!(members.contains("No members to show"));
    assert!(!members.contains("Manage team"));
    page.show_repositories = true;
    let repositories = team_page_html(page);
    assert!(repositories.contains("No repositories to show"));
    assert!(!repositories.contains("No members to show"));
}

/// A plain member's view of the selected organization, with a team open.
fn member_page() -> Page {
    let mut page = sample_page(HashMap::new(), vec![]);
    page.tab = "members".into();
    page.can_manage = false;
    page.is_owner = false;
    page.members = vec![
        Member {
            user_id: 5,
            username: "alice".into(),
            role: "owner".into(),
        },
        Member {
            user_id: 6,
            username: "bob".into(),
            role: "member".into(),
        },
    ];
    page
}

#[test]
fn members_get_a_read_only_organization() {
    let mut page = member_page();
    page.tab = "overview".into();
    let mut output = html(page.clone());
    page.tab = "members".into();
    output.push_str(&html(page.clone()));
    page.tab = "teams".into();
    output.push_str(&html(page));
    assert!(output.contains("Organization profile"));
    assert!(output.contains("No description yet."));
    assert!(output.contains("href=\"/alice\""));
    assert!(output.contains("href=\"/orgs/native_org/teams/writers\""));
    assert!(output.contains("native_org/widgets"));
    // Team rows open the panel read-only, so they say View rather than Manage.
    assert!(output.contains(">View</a>"));
    assert!(!output.contains(">Manage</a>"));
    for hidden in [
        "Save profile",
        "Organization picture",
        "Upload a new picture",
        "Send invitation",
        "Create a team",
        "Save team",
        "Add team member",
        "Grant repository",
        "Delete team",
        "value=\"remove-member\"",
        "value=\"member-role\"",
        "value=\"remove-team-member\"",
        "value=\"grant\"",
        "value=\"revoke\"",
        "name=\"role\"",
        "name=\"permission\"",
        "name=\"team_id\"",
    ] {
        assert!(!output.contains(hidden), "a member must not see {hidden}");
    }
}

#[test]
fn owners_get_inline_member_controls_and_a_danger_card_for_delete() {
    let mut page = member_page();
    page.can_manage = true;
    page.is_owner = true;
    let mut output = html(page.clone());
    page.tab = "overview".into();
    output.push_str(&html(page.clone()));
    page.tab = "teams".into();
    output.push_str(&html(page));
    for shown in [
        "Save profile",
        "Organization picture",
        "Send invitation",
        "Create a team",
        "Edit team",
        "Add a team member",
        "Grant repository access",
        "Delete team and its grants",
        "btn btn-destructive",
        "Update",
        "Remove",
    ] {
        assert!(output.contains(shown), "an owner must see {shown}");
    }
    // Both members get the role and remove forms: the owner row too.
    assert_eq!(output.matches("value=\"remove-member\"").count(), 2);
    assert_eq!(output.matches("value=\"member-role\"").count(), 2);
    assert!(output.contains(">Manage</a>"));
    // Team rows say "Manage" for owners, and the grant and team forms take no raw IDs.
    assert!(!output.contains("Revoke by ID"));
    assert!(!output.contains("Team ID"));
}

#[test]
fn owners_confirm_each_removal_by_username_and_see_each_role_once() {
    let mut page = member_page();
    page.can_manage = true;
    page.is_owner = true;
    page.viewer_id = 5;
    let output = html(page);
    // Every removable row asks for its own username before removal.
    let removals = output.matches("value=\"remove-member\"").count()
        + output.matches("value=\"remove-team-member\"").count();
    assert!(removals >= 2);
    // The organization delete confirmation belongs to Overview, not Members.
    assert_eq!(output.matches("name=\"confirm\"").count(), removals);
    let mut overview = member_page();
    overview.can_manage = true;
    overview.is_owner = true;
    overview.tab = "overview".into();
    assert_eq!(html(overview).matches("name=\"confirm\"").count(), 1);
    assert!(output.contains("Type alice to confirm"));
    assert!(output.contains("Type bob to confirm"));
    // A manageable row shows its role in the select, not in a second badge.
    assert!(!output.contains("class=\"org-label\">owner<"));
    assert!(!output.contains("class=\"org-label\">member<"));
}

#[test]
fn recovery_card_asks_for_the_organization_name_as_confirmation() {
    let output = organization_index(false, true);
    assert!(output.contains("Type the organization name to confirm"));
    assert!(output.contains("Restore owner"));
}

#[test]
fn only_owners_see_the_delete_card() {
    let mut owner_page = sample_page(HashMap::new(), vec![]);
    owner_page.tab = "overview".into();
    let owner = html(owner_page);
    assert!(owner.contains("Delete this organization"));
    assert!(owner.contains("value=\"delete-organization\""));
    let mut page = sample_page(HashMap::new(), vec![]);
    page.tab = "overview".into();
    page.is_owner = false;
    let admin = html(page);
    assert!(!admin.contains("Delete this organization"));
    assert!(!admin.contains("value=\"delete-organization\""));
}

#[test]
fn organization_delete_is_a_centred_dialog_that_names_its_repositories() {
    let mut page = sample_page(HashMap::new(), vec![]);
    page.tab = "overview".into();
    page.owned_repositories = 2;
    let output = html(page);
    // The button opens the dialog by its fragment. Cancel reloads the page, which closes it.
    assert!(output.contains("href=\"#organization-delete-dialog\""));
    assert!(output.contains("id=\"organization-delete-dialog\""));
    assert!(output.contains("aria-label=\"Delete this organization\""));
    assert!(
        output.contains("It also permanently deletes its 2 repositories and everything in them.")
    );
    // The typed name is still the confirmation.
    assert!(output.contains("Type the organization name to confirm"));
    assert!(output.contains(">Cancel</a>"));
}

#[test]
fn a_refused_organization_delete_shows_its_message_by_the_button() {
    let draft = HashMap::from([
        ("action".to_owned(), "delete-organization".to_owned()),
        ("confirm".to_owned(), "wrong".to_owned()),
    ]);
    let mut page = sample_page(draft, vec![]);
    page.tab = "overview".into();
    page.message = Some("Type the organization name to confirm deletion.".into());
    let output = html(page);
    assert!(output.contains("Type the organization name to confirm deletion."));
    assert!(output.contains("value=\"wrong\""));
}

#[test]
fn team_delete_is_a_centred_dialog_that_asks_for_the_team_id() {
    let output = html(sample_page(HashMap::new(), vec![]));
    assert!(output.contains("href=\"#team-delete-dialog\""));
    assert!(output.contains("id=\"team-delete-dialog\""));
    assert!(output.contains("aria-label=\"Delete team Writers\""));
    assert!(output.contains("Type the team ID to confirm"));
    assert!(output.contains("value=\"team-delete\""));
    assert!(output.contains("Delete team and its grants"));
}

#[test]
fn team_member_removal_asks_for_the_username_in_a_centred_dialog() {
    let mut page = sample_page(HashMap::new(), vec![]);
    page.selected_team.as_mut().unwrap().members = vec![Member {
        user_id: 6,
        username: "bob".into(),
        role: "member".into(),
    }];
    let output = html(page);
    assert!(output.contains("href=\"#remove-team-member-6\""));
    assert!(output.contains("id=\"remove-team-member-6\""));
    assert!(output.contains("aria-label=\"Remove bob from the team\""));
    assert!(output.contains("Type bob to confirm"));
    assert!(output.contains("value=\"remove-team-member\""));
}

#[test]
fn member_removal_asks_for_the_username_in_a_centred_dialog() {
    let mut page = member_page();
    page.can_manage = true;
    page.is_owner = true;
    page.viewer_id = 5;
    let output = html(page);
    assert!(output.contains("href=\"#remove-member-6\""));
    assert!(output.contains("id=\"remove-member-6\""));
    assert!(output.contains("aria-label=\"Remove bob from the organization\""));
    assert!(output.contains("Type bob to confirm"));
}

#[test]
fn admins_change_plain_members_but_not_owners() {
    let mut page = member_page();
    page.can_manage = true;
    page.is_owner = false;
    let output = html(page);
    // The DAO refuses an admin's change to an owner, so the owner row shows no controls.
    assert_eq!(output.matches("value=\"remove-member\"").count(), 1);
    assert_eq!(output.matches("value=\"member-role\"").count(), 1);
    assert!(output.contains("Send invitation"));
}

#[test]
fn organization_pages_use_no_disclosure_widgets() {
    let mut admin = sample_page(HashMap::new(), vec![]);
    admin.can_recover = true;
    for output in [
        html(admin),
        html(member_page()),
        organization_index(false, true),
    ] {
        // The only menu is the account settings section picker a phone shows in place of the tab column.
        let output = output.replace("<details class=\"identity-settings-picker\"", "<picker");
        assert!(
            !output.contains("<details"),
            "{}",
            &output[..output.len().min(200)]
        );
    }
}

#[test]
fn organization_pages_share_the_account_settings_layout() {
    let mut page = sample_page(HashMap::new(), vec![]);
    page.selected = None;
    page.selected_team = None;
    let output = html(page.clone());
    assert!(output.contains("identity-settings-layout"));
    assert!(output.contains("aria-label=\"Account settings\""));
    assert!(output.contains("Organizations, current page"));
    assert!(!output.contains("org-breadcrumb"));
    // The Administration section appears only to people who may manage access.
    assert!(!output.contains("Account access"));
    page.manage_access = true;
    assert!(html(page).contains("Account access"));
}

#[test]
fn organization_settings_leave_the_personal_shell() {
    let output = html(sample_page(HashMap::new(), vec![]));
    // The organization's name, its own tabs and a way out, with no personal navigation.
    assert!(output.contains("Native org settings"));
    // The shared stylesheet mentions the personal navigation, so check the rendered elements.
    assert!(!output.contains(">Account settings</h1>"));
    assert!(!output.contains("aria-label=\"Account settings\""));
    assert!(!output.contains("class=\"identity-settings-nav\""));
    assert!(!output.contains(">Personal<"));
    assert!(output.contains("← Organizations"));
    assert!(output.contains("href=\"/settings/organizations\""));
    assert!(output.contains("View profile"));
    assert!(output.contains("href=\"/native_org\""));
    assert!(output.contains("aria-label=\"Organization sections\""));
    assert!(output.contains("?tab=members"));
    assert!(output.contains("?tab=teams"));
}

#[test]
fn team_rows_show_member_count_and_privacy_not_ids() {
    let output = html(sample_page(HashMap::new(), vec![]));
    assert!(output.contains("2 members"));
    assert!(output.contains("Visible to organization members"));
    assert!(!output.contains("Team #9"));
}

#[test]
fn organization_pictures_use_the_upload_or_the_generated_avatar() {
    let generated = html(sample_page(HashMap::new(), vec![]));
    assert!(generated.contains("src=\"/avatars/native_org\""));
    let mut uploaded = sample_page(HashMap::new(), vec![]);
    uploaded.selected.as_mut().expect("selected").avatar_url =
        Some("/api/files/abc/download".into());
    let uploaded = html(uploaded);
    assert!(uploaded.contains("src=\"/api/files/abc/download\""));
    assert!(!uploaded.contains("src=\"/avatars/native_org\""));
}

#[test]
fn pending_invitations_show_cancel_to_owners_and_accept_or_decline_to_invitees() {
    let mut page = sample_page(HashMap::new(), vec![]);
    page.can_manage = true;
    page.tab = "members".into();
    page.pending_invitations = vec![super::page::PendingInvitation {
        id: 1,
        user_id: 44,
        username: "dana".into(),
        role: "admin".into(),
    }];
    let owner_view = html(page.clone());
    assert!(owner_view.contains("Pending invitations"));
    assert!(owner_view.contains("Invited as admin"));
    assert!(owner_view.contains("id=\"cancel-invitation-44\""));
    assert!(owner_view.contains("value=\"cancel-invitation\""));
    assert!(owner_view.contains("Invite a member"));

    // Members who cannot manage see no pending list, and no way to cancel.
    page.can_manage = false;
    let plain_view = html(page.clone());
    assert!(!plain_view.contains("Pending invitations"));
    assert!(!plain_view.contains("cancel-invitation"));

    // The organizations list offers accept and decline for each invitation.
    let mut list = sample_page(HashMap::new(), vec![]);
    list.selected = None;
    list.invitations = vec![Invitation {
        id: 1,
        org_id: 7,
        name: "acme".into(),
        display_name: Some("Acme".into()),
        role: "member".into(),
    }];
    let invitee_view = html(list);
    assert!(invitee_view.contains("Invitations"));
    assert!(invitee_view.contains("action=\"/settings/organizations/invitations/7/accept\""));
    assert!(invitee_view.contains("action=\"/settings/organizations/invitations/7/decline\""));
    assert!(invitee_view.contains("joins as member"));
}
