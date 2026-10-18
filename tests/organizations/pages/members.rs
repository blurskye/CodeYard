use super::fixture::{applied, Fixture, INDEX};
use axum::http::StatusCode;
use codeyard::backend::db::{models::OrgRole, organizations::*};

fn row_fields(source: &str, href: &str, action: &str) -> Vec<(String, String)> {
    let row = source
        .split("<li ")
        .find(|row| {
            let row = row.split("</li>").next().unwrap();
            row.contains(&format!("href=\"{href}\""))
                && row.contains(&format!("value=\"{action}\""))
        })
        .expect("named row with its own native action");
    let row = row.split("</li>").next().unwrap();
    assert!(row.contains("class=\"org-row-main\""));
    let form = row
        .split("<form")
        .skip(1)
        .find(|form| {
            form.split("</form>")
                .next()
                .unwrap()
                .contains(&format!("value=\"{action}\""))
        })
        .unwrap()
        .split("</form>")
        .next()
        .unwrap();
    let attribute = |input: &str, name: &str| {
        input
            .split_once(&format!("{name}=\""))
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    };
    let mut fields: Vec<_> = form
        .split("<input")
        .skip(1)
        .filter_map(|input| {
            let input = input.split('>').next().unwrap();
            let name = attribute(input, "name");
            // A typed confirmation is never prefilled; the test types it itself.
            if name == "confirm" {
                return None;
            }
            if name.ends_with("_id") {
                assert!(input.contains("type=\"hidden\""));
            }
            Some((name, attribute(input, "value")))
        })
        .collect();
    // A select posts its chosen option, or the first one when none is marked selected.
    fields.extend(form.split("<select").skip(1).map(|select| {
        let select = select.split("</select>").next().unwrap();
        let options: Vec<&str> = select.split("<option").skip(1).collect();
        let chosen = options
            .iter()
            .find(|option| option.split('>').next().unwrap().contains("selected"))
            .copied()
            .or(options.first().copied())
            .unwrap();
        (attribute(select, "name"), attribute(chosen, "value"))
    }));
    assert!(fields
        .iter()
        .any(|(name, value)| name == "csrf_token" && !value.is_empty()));
    fields
}

/// A removal row's fields with the removed account's username typed in its confirmation field.
fn confirmed(mut fields: Vec<(String, String)>, username: &str) -> Vec<(String, String)> {
    fields.push(("confirm".into(), username.into()));
    fields
}

async fn submit_row(f: &Fixture, path: &str, fields: &[(String, String)]) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().map(|(key, value)| (key, value)))
        .finish();
    f.request(
        &f.owner,
        "POST",
        path,
        body,
        "application/x-www-form-urlencoded",
        (false, false),
    )
    .await
    .redirect(path);
}

pub async fn banned_team_member_removal(f: &Fixture) {
    let team = f.team("banned_team", TeamPrivacy::Visible).await;
    let path = format!("{}?team={team}", f.path());
    f.form(
        &f.owner,
        &path,
        &[
            ("action", "team-member"),
            ("team_id", &team.to_string()),
            ("username", "ordinary_member"),
            ("role", "member"),
        ],
    )
    .await
    .redirect(&path);
    let listed = f.get(&f.owner, &path).await.organization();
    assert!(listed
        .selected_team
        .as_ref()
        .unwrap()
        .members
        .iter()
        .any(|member| member.user_id == f.member.id));
    f.database
        .execute(
            "UPDATE users SET is_banned=TRUE WHERE id=$1",
            &[f.member.id],
        )
        .await;
    let response = f.get(&f.owner, &path).await;
    let detail = response.organization().selected_team.unwrap();
    assert!(!detail
        .members
        .iter()
        .any(|member| member.user_id == f.member.id));
    // The banned row is hidden from the team, so the team offers no removal for it.
    let html = f
        .request(
            &f.owner,
            "GET",
            &path,
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(html.status, StatusCode::OK);
    assert!(!html.body.contains("value=\"remove-team-member\""));
    let membership_rows =
        format!("SELECT COUNT(*) FROM team_memberships WHERE team_id={team} AND user_id=$1");
    assert_eq!(
        f.database.count_rows(&membership_rows, f.member.id).await,
        1
    );
    // Removing the account from the organization also removes its team memberships.
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "remove-member"),
            ("user_id", &f.member.id.to_string()),
            ("confirm", "ordinary_member"),
        ],
    )
    .await
    .redirect(&f.path());
    assert_eq!(
        f.database.count_rows(&membership_rows, f.member.id).await,
        0
    );
}

pub async fn access_management(f: &Fixture) {
    let team = f.team("identified_team", TeamPrivacy::Visible).await;
    let path = format!("{}?team={team}", f.path());
    for username in ["orgs", "ordinary_member"] {
        f.form(
            &f.owner,
            &path,
            &[
                ("action", "team-member"),
                ("team_id", &team.to_string()),
                ("username", username),
                ("role", "member"),
            ],
        )
        .await
        .redirect(&path);
    }
    let mut repos = Vec::new();
    for name in ["first_grant", "second_grant"] {
        let repo = f.database.organization_repository(f.org, name, true).await;
        f.form(
            &f.owner,
            &path,
            &[
                ("action", "grant"),
                ("team_id", &team.to_string()),
                ("repo_id", &repo.to_string()),
                ("permission", "3"),
            ],
        )
        .await
        .redirect(&path);
        repos.push(repo);
    }
    let page = f.get(&f.owner, &path).await.organization();
    for members in [&page.members, &page.selected_team.as_ref().unwrap().members] {
        assert!(members
            .iter()
            .any(|member| member.user_id == f.owner.id && member.username == "orgs"));
        assert!(members
            .iter()
            .any(|member| member.user_id == f.member.id && member.username == "ordinary_member"));
    }
    for (grant, (id, name)) in page
        .selected_team
        .as_ref()
        .unwrap()
        .repositories
        .iter()
        .zip(repos.iter().zip(["first_grant", "second_grant"]))
    {
        assert_eq!(grant.repo_id, *id);
        assert_eq!(grant.owner, "native_org");
        assert_eq!(grant.name, name);
    }
    let response = f
        .request(
            &f.owner,
            "GET",
            &path,
            String::new(),
            "application/x-www-form-urlencoded",
            (false, false),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let source = &response.body;
    for (username, id) in [("orgs", f.owner.id), ("ordinary_member", f.member.id)] {
        for action in ["member-role", "remove-member", "remove-team-member"] {
            let fields = row_fields(source, &format!("/{username}"), action);
            assert!(fields.contains(&("user_id".into(), id.to_string())));
            if action == "remove-team-member" {
                assert!(fields.contains(&("team_id".into(), team.to_string())));
            }
        }
    }
    for (name, id) in ["first_grant", "second_grant"].into_iter().zip(&repos) {
        let fields = row_fields(source, &format!("/native_org/{name}"), "revoke");
        assert!(fields.contains(&("repo_id".into(), id.to_string())));
        assert!(fields.contains(&("team_id".into(), team.to_string())));
    }
    let invalid = f
        .form(
            &f.owner,
            &path,
            &[
                ("action", "member-role"),
                ("user_id", &f.member.id.to_string()),
                ("role", "invalid"),
            ],
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    let draft = super::profile::html(invalid.organization());
    // A value outside the select is not offered, so the row keeps the role it has.
    let role_of = |html: &str| {
        row_fields(html, "/ordinary_member", "member-role")
            .into_iter()
            .find(|(name, _)| name == "role")
    };
    assert_eq!(role_of(draft.as_str()), role_of(source.as_str()));
    assert!(row_fields(&draft, "/orgs", "member-role").contains(&("role".into(), "owner".into())));
    let mut grant = row_fields(source, "/native_org/second_grant", "grant");
    let access = grant
        .iter_mut()
        .find(|(name, _)| name == "permission")
        .unwrap();
    // The access select posts one of the five levels.
    assert!(["1", "2", "3", "4", "5"].contains(&access.1.as_str()));
    access.1 = "1".into();
    submit_row(f, &path, &grant).await;
    let grants = f
        .get(&f.owner, &path)
        .await
        .organization()
        .selected_team
        .unwrap()
        .repositories;
    assert_eq!(grants[0].permission, 3);
    assert_eq!(grants[1].permission, 1);
    let mut role = row_fields(source, "/ordinary_member", "member-role");
    role.iter_mut().find(|(name, _)| name == "role").unwrap().1 = "admin".into();
    submit_row(f, &path, &role).await;
    let members = f.get(&f.owner, &path).await.organization().members;
    assert!(members
        .iter()
        .any(|m| m.user_id == f.member.id && m.role == "admin"));
    assert!(members
        .iter()
        .any(|m| m.user_id == f.owner.id && m.role == "owner"));
    submit_row(
        f,
        &path,
        &confirmed(
            row_fields(source, "/ordinary_member", "remove-team-member"),
            "ordinary_member",
        ),
    )
    .await;
    submit_row(
        f,
        &path,
        &row_fields(source, "/native_org/first_grant", "revoke"),
    )
    .await;
    let detail = f
        .get(&f.owner, &path)
        .await
        .organization()
        .selected_team
        .unwrap();
    assert_eq!(detail.members.len(), 1);
    assert_eq!(detail.members[0].user_id, f.owner.id);
    assert_eq!(detail.repositories.len(), 1);
    assert_eq!(detail.repositories[0].repo_id, repos[1]);
    submit_row(
        f,
        &path,
        &confirmed(
            row_fields(source, "/ordinary_member", "remove-member"),
            "ordinary_member",
        ),
    )
    .await;
    let members = f.get(&f.owner, &path).await.organization().members;
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].user_id, f.owner.id);
}

pub async fn contracts(f: &Fixture) {
    let pool = f.database.pool();
    let target = Fixture::actor(pool, "inactive_cleanup_target").await;
    let user_id = target.id.to_string();
    let member_id = f.member.id.to_string();
    // Ordinary member/site admin outsider cannot probe target-account existence.
    for action in ["add-member", "team-member"] {
        for username in ["no_such_account", "orgs"] {
            let fields = if action == "add-member" {
                vec![
                    ("action", action),
                    ("username", username),
                    ("role", "member"),
                ]
            } else {
                vec![
                    ("action", action),
                    ("username", username),
                    ("role", "member"),
                    ("team_id", "1"),
                ]
            };
            let denied = f.form(&f.member, &f.path(), &fields).await;
            assert_eq!(denied.status, StatusCode::FORBIDDEN);
            assert_eq!(
                denied.organization().message.as_deref(),
                Some("This organization operation is not permitted.")
            );
            denied.secure();
            let outsider = f.form(&f.outsider, &f.path(), &fields).await;
            assert_eq!(outsider.status, StatusCode::NOT_FOUND);
        }
    }
    for username in ["no_such_account", "orgs"] {
        let bad_role = f
            .form(
                &f.member,
                &f.path(),
                &[
                    ("action", "add-member"),
                    ("username", username),
                    ("role", "invented"),
                ],
            )
            .await;
        assert_eq!(bad_role.status, StatusCode::BAD_REQUEST);
        let bad_id = f
            .form(
                &f.member,
                &f.path(),
                &[
                    ("action", "team-member"),
                    ("username", username),
                    ("role", "member"),
                    ("team_id", "9223372036854775808"),
                ],
            )
            .await;
        assert_eq!(bad_id.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        f.form(
            &f.member,
            &f.path(),
            &[
                ("action", "grant"),
                ("team_id", "1"),
                ("repo_id", "1"),
                ("permission", "6")
            ]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "add-member"),
            ("username", "inactive_cleanup_target"),
            ("role", "owner"),
        ],
    )
    .await
    .redirect(&f.path());
    // Add is not update: conflict cannot change existing authority.
    let conflict = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "add-member"),
                ("username", "inactive_cleanup_target"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(
        conflict.organization().draft["username"],
        "inactive_cleanup_target"
    );
    assert!(conflict
        .organization()
        .members
        .iter()
        .any(|member| member.user_id == target.id && member.role == "owner"));
    for role in ["owner", "admin"] {
        if role == "admin" {
            f.form(
                &f.owner,
                &f.path(),
                &[
                    ("action", "member-role"),
                    ("user_id", &user_id),
                    ("role", "admin"),
                ],
            )
            .await
            .redirect(&f.path());
        }
        f.database
            .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[target.id])
            .await;
        f.form(
            &f.owner,
            &f.path(),
            &[
                ("action", "member-role"),
                ("user_id", &user_id),
                ("role", "member"),
            ],
        )
        .await
        .redirect(&f.path());
        assert!(f
            .get(&f.owner, &f.path())
            .await
            .organization()
            .members
            .iter()
            .any(|member| member.user_id == target.id && member.role == "member"));
        f.database
            .execute("UPDATE users SET is_banned=FALSE WHERE id=$1", &[target.id])
            .await;
    }
    // Admins may add ordinary members but cannot assign ownership.
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "member-role"),
            ("user_id", &member_id),
            ("role", "admin"),
        ],
    )
    .await
    .redirect(&f.path());
    let page = f.get(&f.owner, &f.path()).await.organization();
    assert!(page
        .members
        .iter()
        .any(|member| member.user_id == f.member.id && member.role == "admin"));
    let source = super::profile::html(page);
    assert!(source.contains("ordinary_member: admin"), "{source}");
    let denied = f
        .form(
            &f.member,
            &f.path(),
            &[
                ("action", "member-role"),
                ("user_id", &user_id),
                ("role", "owner"),
            ],
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let owner_id = f.owner.id.to_string();
    let last_owner = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "member-role"),
                ("user_id", &owner_id),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(last_owner.status, StatusCode::CONFLICT);
    assert_eq!(
        last_owner.organization().message.as_deref(),
        Some("An organization needs at least one active owner.")
    );
    // The refused demotion re-selects the role it was submitted with.
    let refused = super::profile::html(last_owner.organization());
    assert!(
        row_fields(&refused, "/orgs", "member-role").contains(&("role".into(), "member".into()))
    );
    assert!(last_owner
        .organization()
        .members
        .iter()
        .any(|member| member.user_id == f.owner.id && member.role == "owner"));
    // Removing yourself needs your username typed; with it, the last owner is still kept.
    assert_eq!(
        f.form(
            &f.owner,
            &f.path(),
            &[("action", "remove-member"), ("user_id", &owner_id)]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let last_removal = f
        .form(
            &f.owner,
            &f.path(),
            &[
                ("action", "remove-member"),
                ("user_id", &owner_id),
                ("confirm", "orgs"),
            ],
        )
        .await;
    assert_eq!(last_removal.status, StatusCode::CONFLICT);
    assert_eq!(
        last_removal.organization().message.as_deref(),
        Some("An organization needs at least one active owner.")
    );

    // Team operations are delegated without a blanket ingress admin requirement.
    let team = f
        .team("members_team", TeamPrivacy::Visible)
        .await
        .to_string();
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "team-member"),
            ("team_id", &team),
            ("username", "ordinary_member"),
            ("role", "maintainer"),
        ],
    )
    .await
    .redirect(&f.path());
    let team_members = applied(
        list_organization_team_members(
            pool,
            f.owner.id,
            f.org,
            team.parse().unwrap(),
            OrganizationPage::new(0, 100).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(team_members
        .iter()
        .any(|member| member.user_id == f.member.id && member.role == "maintainer"));
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "remove-team-member"),
            ("team_id", &team),
            ("user_id", &member_id),
            ("confirm", "ordinary_member"),
        ],
    )
    .await
    .redirect(&f.path());

    // Suspended governance refuses coherently before resolving usernames or roles.
    f.database
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[f.owner.id])
        .await;
    for username in ["no_such_account", "orgs"] {
        let suspended = f
            .form(
                &f.member,
                &f.path(),
                &[
                    ("action", "add-member"),
                    ("username", username),
                    ("role", "invalid"),
                ],
            )
            .await;
        assert_eq!(suspended.status, StatusCode::CONFLICT);
        assert_eq!(
            suspended.organization().message.as_deref(),
            Some("Organization governance is suspended.")
        );
    }
    // Banned sessions are anonymous at the shared identity boundary.
    f.get(&f.owner, &f.path()).await.redirect("/login");
    f.database
        .execute(
            "UPDATE users SET is_banned=FALSE WHERE id=$1",
            &[f.owner.id],
        )
        .await;
    f.database
        .execute(
            "UPDATE users SET must_change_password=TRUE WHERE id=$1",
            &[f.owner.id],
        )
        .await;
    // A temporary password reaches only the password form under /settings.
    assert_eq!(
        f.get(&f.owner, &f.path()).await.status,
        StatusCode::FORBIDDEN
    );
    f.database
        .execute(
            "UPDATE users SET must_change_password=FALSE WHERE id=$1",
            &[f.owner.id],
        )
        .await;

    // DAO last-usable-owner guard stays intact; successful self-removal goes to
    // the index, not the now-inaccessible organization. Create a second owner.
    applied(
        update_organization_member_role(pool, f.owner.id, f.org, target.id, OrgRole::Owner)
            .await
            .unwrap(),
    );
    f.form(
        &f.owner,
        &f.path(),
        &[
            ("action", "remove-member"),
            ("user_id", &owner_id),
            ("confirm", "orgs"),
        ],
    )
    .await
    .redirect(INDEX);
    assert_eq!(
        f.get(&f.owner, &f.path()).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(f.get(&f.owner, INDEX).await.status, StatusCode::OK);
}

/// ADDED, NOT RUN: verified staff recovery stays separate from tenant authority.
pub async fn recovery(f: &Fixture) {
    use super::{database::Database, profile::html};
    const RECOVERY: &str = "/admin/organizations/recovery";
    let org = f.org.to_string();
    let fields = [
        ("action", "recover-owner"),
        ("org_id", org.as_str()),
        ("username", "ordinary_member"),
        ("confirm", "native_org"),
    ];
    let admin_index = f.get(&f.outsider, INDEX).await.organization();
    assert!(admin_index.can_recover);
    // No organization has lost its owner yet, so the picker has nothing to offer.
    assert!(admin_index.ownerless_organizations.is_empty());
    assert!(!html(admin_index).contains("/admin/organizations/recovery"));
    for actor in [&f.owner, &f.member] {
        let page = f.get(actor, INDEX).await.organization();
        assert!(!page.can_recover);
        assert!(!html(page).contains("/admin/organizations/recovery"));
        let denied = f.form(actor, RECOVERY, &fields).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
        denied.secure();
    }
    assert_eq!(
        f.get(&f.outsider, RECOVERY).await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let conflict = f.form(&f.outsider, RECOVERY, &fields).await;
    assert_eq!(
        conflict.status,
        StatusCode::CONFLICT,
        "active owner prevents exceptional recovery"
    );
    assert_eq!(conflict.organization().draft["org_id"], org);
    assert!(conflict.organization().selected.is_none());
    assert!(!conflict
        .organization()
        .organizations
        .iter()
        .any(|value| value.id == f.org));
    assert!(!conflict.body.contains("Stored display"));
    for (token, cross_site) in [("invalid", false), (f.outsider.csrf.as_str(), true)] {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf_token", token)
            .extend_pairs(fields.iter().copied())
            .finish();
        let reply = f
            .request(
                &f.outsider,
                "POST",
                RECOVERY,
                body,
                "application/x-www-form-urlencoded",
                (true, cross_site),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        reply.secure();
    }
    for id in ["0", "-1", "bad", "9223372036854775808"] {
        let bad = f
            .form(
                &f.outsider,
                RECOVERY,
                &[
                    ("action", "recover-owner"),
                    ("org_id", id),
                    ("username", "ordinary_member"),
                ],
            )
            .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
        assert_eq!(bad.organization().draft["username"], "ordinary_member");
    }
    let huge = "9".repeat(33);
    let bad = f
        .form(
            &f.outsider,
            RECOVERY,
            &[
                ("action", "recover-owner"),
                ("org_id", &huge),
                ("username", "ordinary_member"),
                ("billing_email", "do-not-echo"),
                ("redirect", "https://evil.invalid"),
            ],
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let draft = bad.organization().draft;
    assert_eq!(draft["username"], "ordinary_member");
    for key in ["org_id", "csrf_token", "billing_email", "redirect"] {
        assert!(!draft.contains_key(key));
    }
    assert!(!bad.body.contains("do-not-echo"));
    assert!(!bad.body.contains("evil.invalid"));
    let oversized = format!(
        "csrf_token={}&action=recover-owner&org_id={}",
        f.outsider.csrf,
        "1".repeat(33 * 1024)
    );
    let bad = f
        .request(
            &f.outsider,
            "POST",
            RECOVERY,
            oversized,
            "application/x-www-form-urlencoded",
            (true, false),
        )
        .await;
    assert_eq!(bad.status, StatusCode::PAYLOAD_TOO_LARGE);
    bad.secure();

    // The same verified session sees CURRENT privilege, not its creation-time flag.
    f.database
        .execute(
            "UPDATE users SET is_admin=FALSE WHERE id=$1",
            &[f.outsider.id],
        )
        .await;
    assert!(!f.get(&f.outsider, INDEX).await.organization().can_recover);
    assert_eq!(
        f.form(&f.outsider, RECOVERY, &fields).await.status,
        StatusCode::FORBIDDEN
    );
    f.database
        .execute(
            "UPDATE users SET is_admin=TRUE WHERE id=$1",
            &[f.outsider.id],
        )
        .await;
    for restriction in ["is_banned", "must_change_password"] {
        f.database
            .execute(
                &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
                &[f.outsider.id],
            )
            .await;
        let refused = f.form(&f.outsider, RECOVERY, &fields).await;
        if restriction == "must_change_password" {
            // Shared PageContext requires account recovery before non-settings ingress.
            refused.redirect("/settings");
            assert_eq!(
                f.get(&f.outsider, INDEX).await.status,
                StatusCode::FORBIDDEN
            );
        } else {
            refused.redirect("/login");
        }
        f.database
            .execute(
                &format!("UPDATE users SET {restriction}=FALSE WHERE id=$1"),
                &[f.outsider.id],
            )
            .await;
    }
    let former = Fixture::actor(f.database.pool(), "former_recovery_member").await;
    applied(
        add_organization_member(
            f.database.pool(),
            f.owner.id,
            f.org,
            former.id,
            OrgRole::Member,
        )
        .await
        .unwrap(),
    );
    applied(
        remove_organization_member(f.database.pool(), f.owner.id, f.org, former.id)
            .await
            .unwrap(),
    );
    f.database
        .execute("UPDATE users SET is_banned=TRUE WHERE id=$1", &[f.owner.id])
        .await;
    let suspended = f.get(&f.member, &f.path()).await.organization();
    assert!(suspended.selected.as_ref().unwrap().suspended);
    let source = html(suspended);
    assert!(!source.contains("value=\"team-edit\""));
    assert!(!source.contains("/admin/organizations/recovery"));
    // The site administrator's picker offers the organization that lost its owner.
    let picker = f.get(&f.outsider, INDEX).await.organization();
    assert!(picker
        .ownerless_organizations
        .iter()
        .any(|org| org.id == f.org));
    assert!(html(picker).contains("name=\"org_id\""));
    // Valid unauthorized ordinary tenant writes remain refused even for staff.
    assert_eq!(
        f.form(
            &f.outsider,
            &f.path(),
            &[("action", "profile"), ("display_name", "Staff override")]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    for username in ["site_admin_outsider", "orgs", "former_recovery_member"] {
        let refused = f
            .form(
                &f.outsider,
                RECOVERY,
                &[
                    ("action", "recover-owner"),
                    ("org_id", &org),
                    ("username", username),
                    ("confirm", "native_org"),
                ],
            )
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(refused.organization().selected.is_none());
        assert!(!refused.body.contains("Stored display"));
    }
    let missing = f
        .form(
            &f.outsider,
            RECOVERY,
            &[
                ("action", "recover-owner"),
                ("org_id", "9223372036854775807"),
                ("username", "ordinary_member"),
                ("confirm", "native_org"),
            ],
        )
        .await;
    let nonmember = f
        .form(
            &f.outsider,
            RECOVERY,
            &[
                ("action", "recover-owner"),
                ("org_id", &org),
                ("username", "site_admin_outsider"),
                ("confirm", "native_org"),
            ],
        )
        .await;
    assert_eq!(missing.status, nonmember.status);
    assert_eq!(
        missing.organization().message,
        nonmember.organization().message
    );
    for restriction in ["is_banned", "must_change_password"] {
        f.database
            .execute(
                &format!("UPDATE users SET {restriction}=TRUE WHERE id=$1"),
                &[f.member.id],
            )
            .await;
        assert_eq!(
            f.form(&f.outsider, RECOVERY, &fields).await.status,
            StatusCode::CONFLICT
        );
        f.database
            .execute(
                &format!("UPDATE users SET {restriction}=FALSE WHERE id=$1"),
                &[f.member.id],
            )
            .await;
    }
    f.database
        .execute(
            &format!(
                "UPDATE org_memberships SET role='invalid' WHERE org_id={} AND user_id=$1",
                f.org
            ),
            &[f.member.id],
        )
        .await;
    assert_eq!(
        f.form(&f.outsider, RECOVERY, &fields).await.status,
        StatusCode::CONFLICT
    );
    f.database
        .execute(
            &format!(
                "UPDATE org_memberships SET role='member' WHERE org_id={} AND user_id=$1",
                f.org
            ),
            &[f.member.id],
        )
        .await;
    match &f.database {
        Database::Sqlite(_) => f.database.execute("CREATE TRIGGER page_recovery_audit_outage BEFORE INSERT ON audit_log BEGIN SELECT RAISE(ABORT,'owned audit outage'); END", &[]).await,
        Database::Postgres(_) => {
            f.database.execute("CREATE FUNCTION page_recovery_fail_audit() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'owned audit outage'; END; $$ LANGUAGE plpgsql", &[]).await;
            f.database.execute("CREATE TRIGGER page_recovery_audit_outage BEFORE INSERT ON audit_log FOR EACH ROW EXECUTE FUNCTION page_recovery_fail_audit()", &[]).await;
        }
    }
    let outage = f.form(&f.outsider, RECOVERY, &fields).await;
    assert_eq!(outage.status, StatusCode::SERVICE_UNAVAILABLE);
    outage.secure();
    assert_eq!(outage.organization().draft["username"], "ordinary_member");
    assert!(!outage.body.contains("owned audit outage"));
    assert_eq!(
        f.database
            .count_rows("SELECT COUNT(*) FROM audit_log WHERE org_id=$1", f.org)
            .await,
        0
    );
    let page = f.get(&f.member, &f.path()).await.organization();
    assert!(
        page.selected.unwrap().suspended,
        "audit outage rolls back owner promotion"
    );
    assert!(page
        .members
        .iter()
        .any(|value| value.user_id == f.member.id && value.role == "member"));
    match &f.database {
        Database::Sqlite(_) => {
            f.database
                .execute("DROP TRIGGER page_recovery_audit_outage", &[])
                .await
        }
        Database::Postgres(_) => {
            f.database
                .execute("DROP TRIGGER page_recovery_audit_outage ON audit_log", &[])
                .await;
            f.database
                .execute("DROP FUNCTION page_recovery_fail_audit()", &[])
                .await;
        }
    }
    f.form(
        &f.outsider,
        &format!("{RECOVERY}?redirect=https://evil.invalid&team=1"),
        &fields,
    )
    .await
    .redirect(INDEX);
    let page = f.get(&f.member, &f.path()).await.organization();
    assert!(!page.selected.unwrap().suspended);
    assert!(page
        .members
        .iter()
        .any(|value| value.user_id == f.member.id && value.role == "owner"));
    assert_eq!(f.database.count_rows(&format!("SELECT COUNT(*) FROM audit_log WHERE org_id=$1 AND actor_id={} AND action='org.governance.recover_owner'", f.outsider.id), f.org).await, 1);
    let audit = f.database.audit_metadata(f.org).await;
    assert_eq!(audit["reason"], "no_usable_owner");
    assert_eq!(audit["previous_role"], "member");
    assert_eq!(audit["replacement_user_id"], f.member.id);
    assert_eq!(
        f.database
            .count_rows(
                "SELECT COUNT(*) FROM users WHERE id=$1 AND is_banned",
                f.owner.id
            )
            .await,
        1
    );
    assert_eq!(
        f.form(&f.outsider, RECOVERY, &fields).await.status,
        StatusCode::CONFLICT,
        "replay cannot recover a governed tenant"
    );
    assert_eq!(
        f.get(&f.outsider, &f.path()).await.status,
        StatusCode::NOT_FOUND,
        "recovery grants no implicit tenant read"
    );
}
