use super::fixture::{Fixture, INDEX};
use axum::http::StatusCode;
use codeyard::webui::{app::Page, pages::account};

pub async fn contracts(f: &Fixture) {
    let index = f.get(&f.owner, INDEX).await;
    assert_eq!(index.status, StatusCode::OK);
    index.secure();
    assert!(index
        .organization()
        .organizations
        .iter()
        .any(|org| org.id == f.org));
    let detail = f.get(&f.owner, &f.path()).await;
    assert_eq!(detail.status, StatusCode::OK);
    detail.secure();
    assert_eq!(detail.organization().selected.unwrap().id, f.org);
    let orgs_profile = f.get(&f.member, "/orgs").await;
    assert_eq!(orgs_profile.status, StatusCode::OK);
    assert!(
        matches!(orgs_profile.envelope().page, Page::Identity(account::Page::Profile { profile, .. }) if profile.username == "orgs")
    );
    let orgs_repo = f.get(&f.member, "/orgs/repo").await;
    assert_eq!(orgs_repo.status, StatusCode::OK);
    assert!(matches!(orgs_repo.envelope().page, Page::Repositories(_)));
    f.form(&f.member, "/orgs/repo/_forms/star", &[])
        .await
        .redirect("/orgs/repo");
    // Correct :id POST routing, distinct from existing repository roots.
    let created = f
        .form(
            &f.owner,
            INDEX,
            &[("action", "create"), ("slug", "created_native")],
        )
        .await;
    created.redirect(INDEX);
    let duplicate = f
        .form(
            &f.owner,
            INDEX,
            &[
                ("action", "create"),
                ("slug", "created_native"),
                ("description", "Keep this conflict draft"),
            ],
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(
        duplicate.organization().draft["description"],
        "Keep this conflict draft"
    );
    duplicate.secure();
    let bad_name = f
        .form(
            &f.owner,
            INDEX,
            &[("action", "create"), ("slug", "Bad Org!")],
        )
        .await;
    assert_eq!(bad_name.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        bad_name.organization().message.as_deref(),
        Some("Organization name must be 3-255 characters: letters, numbers, hyphens or underscores. Reserved names cannot be used.")
    );

    let missing = format!("{INDEX}/9223372036854775807");
    for method in ["GET", "POST"] {
        let (absent, outside) = if method == "GET" {
            (
                f.get(&f.outsider, &missing).await,
                f.get(&f.outsider, &f.path()).await,
            )
        } else {
            (
                f.form(&f.outsider, &missing, &[("action", "profile")])
                    .await,
                f.form(&f.outsider, &f.path(), &[("action", "profile")])
                    .await,
            )
        };
        assert_eq!(absent.status, StatusCode::NOT_FOUND);
        assert_eq!(outside.status, absent.status);
        assert_eq!(outside.error_message(), absent.error_message());
        outside.secure();
        absent.secure();
    }
    for id in ["garbage", "0", "-1", "9223372036854775808"] {
        assert_eq!(
            f.get(&f.owner, &format!("{INDEX}/{id}")).await.status,
            StatusCode::NOT_FOUND
        );
    }
    for (token, cross_site) in [
        ("", false),
        ("incorrect", false),
        (f.owner.csrf.as_str(), true),
    ] {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("action", "profile")
            .append_pair("csrf_token", token)
            .finish();
        let reply = f
            .request(
                &f.owner,
                "POST",
                &f.path(),
                body,
                "application/x-www-form-urlencoded",
                (true, cross_site),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        reply.secure();
    }
    // Extractor/body failures must also go through PageContext, not default text errors.
    for (content_type, body, status) in [
        (
            "application/json",
            "{}".to_owned(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "application/x-www-form-urlencoded",
            format!(
                "csrf_token={}&action=profile&description={}",
                f.owner.csrf,
                "x".repeat(33 * 1024)
            ),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let reply = f
            .request(
                &f.owner,
                "POST",
                &f.path(),
                body,
                content_type,
                (true, false),
            )
            .await;
        assert_eq!(reply.status, status);
        reply.secure();
        assert!(matches!(reply.envelope().page, Page::Error { .. }));
    }
}

/// ADDED, NOT RUN: detail visibility, scoped grants and native edit/delete PRG.
/// Only the open section's tab carries aria-current, whatever the URL says.
pub async fn section_tabs(f: &Fixture) {
    for query in [
        "",
        "?tab=overview",
        "?tab=members",
        "?tab=teams",
        "?tab=webhooks",
    ] {
        let reply = f
            .request(
                &f.owner,
                "GET",
                &format!("{}{query}", f.path()),
                String::new(),
                "application/x-www-form-urlencoded",
                (false, false),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        let open = query.strip_prefix("?tab=").unwrap_or("overview");
        let tabs = tab_links(&reply.body);
        // Owners also see the webhooks tab.
        assert_eq!(tabs.len(), 4, "{query}");
        for tab in ["overview", "members", "teams", "webhooks"] {
            let link = tabs
                .iter()
                .find(|link| link.contains(&format!("tab={tab}\"")))
                .unwrap_or_else(|| panic!("{tab} tab at {query}"));
            assert_eq!(
                link.contains("aria-current=\"page\""),
                tab == open,
                "{tab} tab at {query}"
            );
        }
    }
}

/// The links of the organization's section tabs, each from its `<a` to the next one.
fn tab_links(source: &str) -> Vec<&str> {
    let nav = source
        .split("aria-label=\"Organization sections\"")
        .nth(1)
        .expect("organization section tabs");
    nav.split("</nav>")
        .next()
        .unwrap()
        .split("<a ")
        .skip(1)
        .collect()
}

pub async fn teams(f: &Fixture) {
    use super::{fixture::applied, profile::html};
    use codeyard::backend::db::{self, models::AccessLevel, organizations::*};
    let pool = f.database.pool();
    for slug in ["api", "admin", "search"] {
        let reserved = f
            .form(&f.owner, INDEX, &[("action", "create"), ("slug", slug)])
            .await;
        assert_eq!(reserved.status, StatusCode::BAD_REQUEST);
        f.form(
            &f.owner,
            &f.path(),
            &[
                ("action", "team"),
                ("slug", slug),
                ("name", slug),
                ("privacy", "visible"),
            ],
        )
        .await
        .redirect(&f.path());
        let team = f
            .get(&f.owner, &f.path())
            .await
            .organization()
            .teams
            .into_iter()
            .find(|team| team.name == slug)
            .expect("reserved route names are valid organization-local teams");
        assert_eq!(
            f.get(&f.owner, &format!("{}?team={}", f.path(), team.id))
                .await
                .organization()
                .selected_team
                .unwrap()
                .slug,
            slug
        );
    }
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "team"),
            ("slug", "native_visible"),
            ("name", "Native team"),
            ("description", "Team description\nSecond line"),
            ("privacy", "visible"),
        ],
    )
    .await
    .redirect(&f.path());
    let visible = f
        .get(&f.owner, &f.path())
        .await
        .organization()
        .teams
        .into_iter()
        .find(|team| team.name == "Native team")
        .unwrap()
        .id;
    // A blank display name on create shows the team name instead.
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "team"),
            ("slug", "blank_display"),
            ("privacy", "visible"),
        ],
    )
    .await
    .redirect(&f.path());
    let blank = f
        .get(&f.owner, &f.path())
        .await
        .organization()
        .teams
        .into_iter()
        .find(|team| team.name == "blank_display")
        .expect("a blank display name shows the team name");
    assert_eq!(blank.privacy, "visible");
    // A bad Team name is refused with the rule it broke, and the draft is kept.
    let bad_name = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "team"),
                ("slug", "a b"),
                ("name", "Spaced"),
                ("privacy", "visible"),
            ],
        )
        .await;
    assert_eq!(bad_name.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        bad_name.organization().message.as_deref(),
        Some("Team name must be 3-255 characters: letters, numbers, hyphens or underscores.")
    );
    assert_eq!(bad_name.organization().draft["name"], "Spaced");
    let secret = f.team("native_secret", TeamPrivacy::Secret).await;
    let other = applied(
        create_organization(
            pool,
            f.outsider.id,
            &NewOrganization::new(
                "foreign_tenant",
                OrganizationProfile::new(None, None).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    let foreign_team = applied(
        create_organization_team(
            pool,
            f.outsider.id,
            other,
            &NewTeam::new(
                "foreign_team",
                TeamProfile::new("Foreign team", None, TeamPrivacy::Visible).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    )
    .id;
    for id in [secret, foreign_team, i64::MAX] {
        let denied = f.get(&f.member, &format!("{}?team={id}", f.path())).await;
        assert_eq!(denied.status, StatusCode::NOT_FOUND);
        denied.secure();
        assert_eq!(denied.error_message(), "Organization not found.");
    }
    assert_eq!(
        f.get(
            &f.member,
            &format!("{}?team={secret}&team_members_after=bad", f.path())
        )
        .await
        .status,
        StatusCode::NOT_FOUND,
        "hidden team refusal precedes detail cursor validation"
    );
    let secret_id = secret.to_string();
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "team-member"),
            ("team_id", &secret_id),
            ("username", "ordinary_member"),
            ("role", "member"),
        ],
    )
    .await
    .redirect(&f.path());
    assert_eq!(
        f.get(&f.member, &format!("{}?team={secret}", f.path()))
            .await
            .organization()
            .selected_team
            .unwrap()
            .team
            .id,
        secret
    );
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "remove-team-member"),
            ("team_id", &secret_id),
            ("user_id", &f.member.id.to_string()),
            ("confirm", "ordinary_member"),
        ],
    )
    .await
    .redirect(&f.path());
    assert_eq!(
        f.get(&f.member, &format!("{}?team={secret}", f.path()))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let team_id = visible.to_string();
    let path = format!("{}?members_after=0&teams_after={visible}&team={visible}&team_members_after=0&team_repositories_after=0", f.path());
    // Detail is independent of the list page: selected team is behind its cursor.
    let detail = f.get(&f.member, &path).await.organization();
    assert!(detail.teams.iter().all(|team| team.id != visible));
    let selected = detail.selected_team.as_ref().unwrap();
    assert_eq!(selected.slug, "native_visible");
    assert_eq!(selected.description, "Team description\nSecond line");
    // The URL names the section the page shows; an unknown name shows the overview.
    assert_eq!(
        f.get(&f.member, &format!("{path}&tab=teams"))
            .await
            .organization()
            .tab,
        "teams"
    );
    assert_eq!(
        f.get(&f.member, &format!("{}?tab=bogus", f.path()))
            .await
            .organization()
            .tab,
        "overview"
    );
    // A member reads the team without its management forms.
    let source = html(detail);
    assert!(!source.contains("value=\"Native team\""));
    assert!(!source.contains("value=\"team-edit\""));
    assert!(!source.contains("value=\"team-delete\""));
    let owner_detail = f.get(&f.owner, &path).await.organization();
    let source = html(owner_detail);
    assert!(source.contains("value=\"Native team\""));
    assert!(source.contains("value=\"team-edit\""));
    assert!(source.contains("value=\"team-delete\""));
    assert!(source.contains(&format!("value=\"{visible}\"")));
    for action in ["team-edit", "team-delete"] {
        let mut fields = vec![("action", action), ("team_id", team_id.as_str())];
        if action == "team-edit" {
            fields.extend([("name", "Denied edit"), ("privacy", "visible")]);
        } else {
            fields.push(("confirmation", team_id.as_str()));
        }
        assert_eq!(
            f.form(&f.member, &path, &fields).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.form(&f.outsider, &path, &fields).await.status,
            StatusCode::NOT_FOUND
        );
    }
    for target in [foreign_team.to_string(), i64::MAX.to_string()] {
        assert_eq!(
            f.form(
                &f.owner,
                &f.path(),
                &[
                    ("action", "team-delete"),
                    ("team_id", &target),
                    ("confirmation", &target)
                ]
            )
            .await
            .status,
            StatusCode::FORBIDDEN
        );
    }
    for privacy in ["unsupported", "closed"] {
        let bad = f
            .form(
                &f.owner,
                &path,
                &[
                    ("action", "team-edit"),
                    ("team_id", &team_id),
                    ("name", "Draft team"),
                    ("description", "\n</textarea><script>bad</script>"),
                    ("privacy", privacy),
                ],
            )
            .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
        assert_eq!(bad.organization().draft["name"], "Draft team");
        let source = html(bad.organization());
        assert!(source.contains("&lt;script&gt;bad&lt;/script&gt;"));
        assert!(!source.contains("<script>bad</script>"));
    }
    for id in ["0", "-1", "garbage", "9223372036854775808"] {
        assert_eq!(
            f.form(
                &f.owner,
                &path,
                &[
                    ("action", "team-delete"),
                    ("team_id", id),
                    ("confirmation", id)
                ]
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    let duplicate = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "team"),
                ("slug", "native_visible"),
                ("name", "Conflict draft"),
                ("description", "Preserved team draft"),
                ("privacy", "visible"),
            ],
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(
        duplicate.organization().draft["description"],
        "Preserved team draft"
    );
    let huge = "x".repeat(4097);
    let bad = f
        .form(
            &f.owner,
            &path,
            &[
                ("action", "team-edit"),
                ("team_id", &team_id),
                ("name", "Bounded name"),
                ("description", &huge),
                ("privacy", "visible"),
                ("password", "never-echo"),
            ],
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.organization().draft["name"], "Bounded name");
    assert!(!bad.organization().draft.contains_key("description"));
    assert!(!bad.body.contains("never-echo"));
    let invalid = f
        .form(
            &f.owner,
            &path,
            &[
                ("action", "team-edit"),
                ("team_id", &team_id),
                ("name", "Draft name"),
                ("privacy", "invalid"),
            ],
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.organization().draft["privacy"], "invalid");
    f.form(
        &f.owner,
        &path,
        &[
            ("action", "team-edit"),
            ("team_id", &team_id),
            ("name", "Renamed team"),
            ("description", "Updated\nMultiline"),
            ("privacy", "visible"),
        ],
    )
    .await
    .redirect(&path);
    let page = f.get(&f.owner, &path).await.organization();
    let detail = page.selected_team.as_ref().unwrap();
    assert_eq!(detail.description, "Updated\nMultiline");
    assert_eq!(detail.team.name, "Renamed team");
    let source = html(page);
    assert!(source.contains("value=\"Renamed team\""), "{source}");

    let private = f
        .database
        .organization_repository(f.org, "private_grant", true)
        .await;
    let public = f
        .database
        .organization_repository(f.org, "public_grant", false)
        .await;
    let foreign = f
        .database
        .organization_repository(other, "foreign_grant", false)
        .await;
    // Grant choices hold only what the viewer can read: an owner sees both repositories,
    // and a plain member sees the public one.
    let owner_choices: Vec<i64> = f
        .get(&f.owner, &path)
        .await
        .organization()
        .repositories
        .iter()
        .map(|repo| repo.repo_id)
        .collect();
    assert_eq!(owner_choices, vec![private, public]);
    let member_choices: Vec<i64> = f
        .get(&f.member, &path)
        .await
        .organization()
        .repositories
        .iter()
        .map(|repo| repo.repo_id)
        .collect();
    assert_eq!(member_choices, vec![public]);
    for repo in [private, public] {
        f.form(
            &f.owner,
            &path,
            &[
                ("action", "grant"),
                ("team_id", &team_id),
                ("repo_id", &repo.to_string()),
                ("permission", "3"),
            ],
        )
        .await
        .redirect(&path);
    }
    let denied = f
        .form(
            &f.owner,
            &path,
            &[
                ("action", "grant"),
                ("team_id", &team_id),
                ("repo_id", &foreign.to_string()),
                ("permission", "3"),
            ],
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    // A corrupt foreign grant exists only in this fixture; it must NEVER be projected.
    f.database.execute(&format!("INSERT INTO team_repo_access(team_id,repo_id,permission) VALUES({visible},{foreign},3)"), &[]).await;
    let response = f.get(&f.member, &path).await;
    assert!(!response.body.contains("private_grant"));
    assert!(!response.body.contains("foreign_grant"));
    let grants = response.organization().selected_team.unwrap().repositories;
    assert_eq!(grants[0].owner, "native_org");
    assert_eq!(grants[0].name, "public_grant");
    assert_eq!(
        grants.iter().map(|grant| grant.repo_id).collect::<Vec<_>>(),
        vec![public]
    );
    db::repositories::access::grant_repo_access(pool, f.member.id, private, AccessLevel::Read)
        .await
        .unwrap();
    assert_eq!(
        f.get(&f.member, &path)
            .await
            .organization()
            .selected_team
            .unwrap()
            .repositories
            .len(),
        2
    );
    db::repositories::access::revoke_repo_access(pool, f.member.id, private)
        .await
        .unwrap();
    f.form(
        &f.owner,
        &path,
        &[
            ("action", "team-member"),
            ("team_id", &team_id),
            ("username", "ordinary_member"),
            ("role", "maintainer"),
        ],
    )
    .await
    .redirect(&path);
    let selected = f
        .get(&f.member, &path)
        .await
        .organization()
        .selected_team
        .unwrap();
    assert!(selected
        .members
        .iter()
        .any(|member| member.user_id == f.member.id && member.role == "maintainer"));
    assert_eq!(
        selected.repositories.len(),
        2,
        "membership grants READ; foreign grant stays filtered"
    );
    let safe = serde_json::to_value(selected).unwrap();
    for key in ["org_id", "created_at", "billing_email", "disk_path"] {
        assert!(safe.get(key).is_none());
    }
    let refused = f
        .form(
            &f.owner,
            &path,
            &[
                ("action", "team-delete"),
                ("team_id", &team_id),
                ("confirmation", &secret.to_string()),
            ],
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        f.database
            .count_rows(
                "SELECT COUNT(*) FROM teams WHERE id=$1",
                team_id.parse().unwrap()
            )
            .await,
        1
    );
    f.form(
        &f.owner,
        &path,
        &[
            ("action", "team-delete"),
            ("team_id", &team_id),
            ("confirmation", &team_id),
        ],
    )
    .await
    .redirect(&format!(
        "{}?members_after=0&teams_after={visible}",
        f.path()
    ));
    for table in ["teams", "team_memberships", "team_repo_access"] {
        let key = if table == "teams" { "id" } else { "team_id" };
        assert_eq!(
            f.database
                .count_rows(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {key}=$1"),
                    visible
                )
                .await,
            0
        );
    }
    assert_eq!(f.get(&f.owner, &path).await.status, StatusCode::NOT_FOUND);
    // A full page of grantable repositories keeps the select. One more switches the grant
    // form to a typed ID, the only way to name repositories past the page.
    let listed = f
        .get(&f.owner, &f.path())
        .await
        .organization()
        .repositories
        .len();
    for index in listed..100 {
        f.database
            .organization_repository(f.org, &format!("bulk_{index:03}"), false)
            .await;
    }
    let full = f.get(&f.owner, &f.path()).await.organization();
    assert_eq!(full.repositories.len(), 100);
    assert!(!full.more_repositories);
    f.database
        .organization_repository(f.org, "bulk_over_limit", false)
        .await;
    let over = f.get(&f.owner, &f.path()).await.organization();
    assert_eq!(over.repositories.len(), 100);
    assert!(over.more_repositories);
}
