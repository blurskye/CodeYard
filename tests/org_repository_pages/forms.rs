use super::fixture::{applied, Actor, Fixture, Reply};
use axum::http::StatusCode;
use codeyard::backend::cluster::NodeKey;
use codeyard::backend::db::{
    self,
    models::{AccessLevel, OrgRole},
    organizations as orgs,
};
use codeyard::webui::{
    app::{Page, PageEnvelope},
    bootstrap::{Bootstrap, BOOTSTRAP_ID},
    pages::discovery as view,
};

fn fields<'a>(
    name: &'a str,
    description: &'a str,
    visibility: &'a str,
    target: &'a str,
) -> [(&'a str, &'a str); 4] {
    [
        ("name", name),
        ("description", description),
        ("visibility", visibility),
        ("owner_org_id", target),
    ]
}
fn bootstrap(reply: &super::fixture::Reply) -> Bootstrap {
    let marker = format!("id=\"{BOOTSTRAP_ID}\"");
    let tag = reply
        .body
        .find(&marker)
        .expect("real safe hydration bootstrap");
    let start = tag + reply.body[tag..].find('>').unwrap() + 1;
    let end = reply.body[start..].find("</script>").unwrap() + start;
    serde_json::from_str(&reply.body[start..end]).unwrap()
}

pub async fn contracts(f: &Fixture) {
    let target = f.org.to_string();
    // Old clients omit the field: only the verified actor's personal namespace.
    let personal = f
        .form(
            &f.owner,
            "/new",
            &[
                ("name", "personal_public"),
                ("description", "Personal draft"),
                ("visibility", "public"),
                ("owner", "siteadmin_only"),
                ("owner_user_id", "999"),
            ],
            true,
        )
        .await;
    personal.redirect("/personal_owner/personal_public");
    let repo = f
        .created("personal_owner", "personal_public", f.owner.id, None, false)
        .await;
    assert_eq!(repo.description.as_deref(), Some("Personal draft"));
    for (actor, name, visibility, private) in [
        (&f.owner, "owner_public", "public", false),
        (&f.admin, "admin_private", "private", true),
    ] {
        let mut submitted = fields(name, "Org draft", visibility, &target).to_vec();
        // Extra API-shaped inputs cannot override visibility or introduce a fork.
        submitted.extend([
            ("is_private", "false"),
            ("source_id", "999"),
            ("forked_from_id", "999"),
        ]);
        let reply = f.form(actor, "/new", &submitted, true).await;
        reply.redirect(&format!("/canonical_org/{name}"));
        let repo = f
            .created("canonical_org", name, actor.id, Some(f.org), private)
            .await;
        assert!(!repo.is_fork);
        assert_eq!(repo.forked_from_id, None);
        assert_eq!(repo.description.as_deref(), Some("Org draft"));
        if name == "owner_public" {
            // Direct write authority on a repository is not organization CREATE.
            db::repositories::access::grant_repo_access(
                f.database.pool(),
                f.writer.id,
                repo.id,
                AccessLevel::Write,
            )
            .await
            .unwrap();
            assert!(db::repositories::access::get_user_repo_access(
                f.database.pool(),
                f.writer.id,
                repo.id
            )
            .await
            .unwrap()
            .can_write());
        }
    }
    // A current allowed target outside the display page is still checked by the
    // native operation, not rejected or certified by the choices model.
    let outside_page = f
        .form(
            &f.admin,
            &format!("/new?org_after={}&org_limit=1", f.org),
            &fields(
                "paged_target",
                "Native authority, not a list permit",
                "private",
                &target,
            ),
            true,
        )
        .await;
    outside_page.redirect("/canonical_org/paged_target");
    f.created(
        "canonical_org",
        "paged_target",
        f.admin.id,
        Some(f.org),
        true,
    )
    .await;
    let private = f
        .form(
            &f.member,
            "/new",
            &fields("member_personal_private", "", "private", ""),
            true,
        )
        .await;
    private.redirect("/ordinary_member/member_personal_private");
    f.created(
        "ordinary_member",
        "member_personal_private",
        f.member.id,
        None,
        true,
    )
    .await;
    // The native private/public floor survives request projection. No source/fork
    // selector or arbitrary personal owner is part of /new's creation contract.
    let public = db::repositories::records::list_public_repositories(
        f.database.pool(),
        db::repositories::records::RepositorySort::Recent,
        100,
    )
    .await
    .unwrap();
    assert!(public.iter().any(|repo| repo.name == "owner_public"));
    assert!(!public.iter().any(|repo| matches!(
        repo.name.as_str(),
        "admin_private" | "member_personal_private"
    )));
    let before_count = f.database.count().await;
    let before_disk = f.disk_inventory();
    let mut denial_message = None;
    for actor in [&f.member, &f.staff, &f.outsider, &f.writer, &f.unknown] {
        let reply = f
            .form(
                actor,
                "/new",
                &fields("unauthorized", "Safe refusal draft", "private", &target),
                true,
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
        reply.secure();
        reply.no_internal_fields();
        let (_, choices, draft, error) = reply.creation();
        assert!(choices.organizations.is_empty());
        assert_eq!(draft.owner_org_id, target);
        assert_eq!(draft.description, "Safe refusal draft");
        assert!(draft.is_private);
        assert!(!reply.body.contains("canonical_org"));
        if let Some(message) = &denial_message {
            assert_eq!(&error, message);
        } else {
            denial_message = Some(error);
        }
    }
    let absent = f
        .form(
            &f.staff,
            "/new",
            &fields(
                "unauthorized",
                "Safe refusal draft",
                "private",
                &i64::MAX.to_string(),
            ),
            true,
        )
        .await;
    assert_eq!(absent.status, StatusCode::FORBIDDEN);
    assert_eq!(absent.creation().3, denial_message.unwrap());
    assert_eq!(f.database.count().await, before_count);
    assert_eq!(f.disk_inventory(), before_disk);

    for raw in ["-1", "0", "+1", "org:1", "garbage", "9223372036854775808"] {
        let reply = f
            .form(
                &f.admin,
                "/new",
                &fields("valid_name", "<script>draft</script>", "private", raw),
                true,
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let draft = reply.creation().2;
        assert_eq!(draft.name, "valid_name");
        assert_eq!(draft.description, "<script>draft</script>");
        assert_eq!(draft.owner_org_id, raw);
        assert!(draft.is_private);
        reply.no_internal_fields();
    }
    let invalid = f
        .form(
            &f.admin,
            "/new?org_after=0&org_limit=1",
            &fields("../bad", "Nonsecret draft", "private", &target),
            true,
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.creation().2.name, "../bad");
    assert_eq!(invalid.creation().2.owner_org_id, target);
    for path in ["/new?org_after=-1", "/new?org_limit=101"] {
        let invalid_page = f
            .form(
                &f.admin,
                path,
                &fields("cursor_draft", "Retain cursor draft", "private", &target),
                true,
            )
            .await;
        assert_eq!(invalid_page.status, StatusCode::BAD_REQUEST);
        assert_eq!(invalid_page.creation().2.description, "Retain cursor draft");
        assert_eq!(invalid_page.creation().2.owner_org_id, target);
    }
    for (token, cross_site) in [("", false), ("wrong", false), (f.admin.csrf.as_str(), true)] {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf_token", token)
            .extend_pairs(fields("csrf_denied", "", "private", &target))
            .finish();
        let reply = f
            .request(&f.admin, "POST", "/new", body, true, cross_site)
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        reply.secure();
    }
    assert_eq!(f.database.count().await, before_count);
    assert_eq!(f.disk_inventory(), before_disk);

    // The existing row (including its directory) and real HEAD are unchanged.
    let original = db::repositories::records::get_repository(
        f.database.pool(),
        "canonical_org",
        "admin_private",
    )
    .await
    .unwrap()
    .unwrap();
    let row = serde_json::to_value(f.database.repository(original.id).await).unwrap();
    let head = std::fs::read(std::path::Path::new(&original.disk_path).join("HEAD")).unwrap();
    let duplicate = f
        .form(
            &f.admin,
            "/new?org_after=0&org_limit=1",
            &fields(
                "admin_private",
                "<script>Conflict safe draft</script>",
                "public",
                &target,
            ),
            true,
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    let (_, choices, draft, error) = duplicate.creation();
    assert_eq!(choices.limit, 1);
    assert_eq!(draft.owner_org_id, target);
    assert_eq!(draft.name, "admin_private");
    assert_eq!(draft.description, "<script>Conflict safe draft</script>");
    assert!(!draft.is_private);
    assert!(error.is_some());
    duplicate.secure();
    duplicate.no_internal_fields();
    assert_eq!(
        serde_json::to_value(f.database.repository(original.id).await).unwrap(),
        row
    );
    assert_eq!(
        std::fs::read(std::path::Path::new(&original.disk_path).join("HEAD")).unwrap(),
        head
    );
    assert_eq!(f.database.count().await, before_count);
    assert_eq!(f.disk_inventory(), before_disk);
}

async fn stale_refusal(f: &Fixture, actor: &Actor, name: &str) {
    let count = f.database.count().await;
    let disk = f.disk_inventory();
    let reply = f
        .form(
            actor,
            "/new",
            &fields(name, "stale safe draft", "private", &f.org.to_string()),
            true,
        )
        .await;
    assert!(
        matches!(reply.status, StatusCode::FORBIDDEN | StatusCode::CONFLICT),
        "{}",
        reply.body
    );
    let (_, choices, draft, _) = reply.creation();
    assert!(choices.organizations.is_empty());
    assert_eq!(draft.name, name);
    assert_eq!(draft.owner_org_id, f.org.to_string());
    assert!(draft.is_private);
    assert!(!reply.body.contains("canonical_org"));
    assert_eq!(f.database.count().await, count);
    assert_eq!(f.disk_inventory(), disk);
}
pub async fn revocation(f: &Fixture) {
    let pool = f.database.pool();
    assert_eq!(
        f.get(&f.admin, "/new", true)
            .await
            .creation()
            .1
            .organizations[0]
            .id,
        f.org
    );
    applied(
        orgs::update_organization_member_role(pool, f.owner.id, f.org, f.admin.id, OrgRole::Member)
            .await
            .unwrap(),
    );
    stale_refusal(f, &f.admin, "after_demotion").await;
    applied(
        orgs::update_organization_member_role(pool, f.owner.id, f.org, f.admin.id, OrgRole::Admin)
            .await
            .unwrap(),
    );
    assert_eq!(
        f.get(&f.admin, "/new", true)
            .await
            .creation()
            .1
            .organizations[0]
            .id,
        f.org
    );
    applied(
        orgs::remove_organization_member(pool, f.owner.id, f.org, f.admin.id)
            .await
            .unwrap(),
    );
    stale_refusal(f, &f.admin, "after_removal").await;
    applied(
        orgs::add_organization_member(pool, f.owner.id, f.org, f.admin.id, OrgRole::Admin)
            .await
            .unwrap(),
    );
    for column in ["is_banned", "must_change_password"] {
        assert_eq!(
            f.get(&f.admin, "/new", true)
                .await
                .creation()
                .1
                .organizations[0]
                .id,
            f.org
        );
        f.database
            .execute(
                &format!("UPDATE users SET {column}=TRUE WHERE id=$1"),
                &[f.owner.id],
            )
            .await;
        stale_refusal(f, &f.admin, &format!("after_{column}")).await;
        f.database
            .execute(
                &format!("UPDATE users SET {column}=FALSE WHERE id=$1"),
                &[f.owner.id],
            )
            .await;
    }
}

pub async fn raw(f: &Fixture) {
    let raw = f.get(&f.admin, "/new", false).await;
    assert_eq!(
        raw.status,
        StatusCode::OK,
        "matching assets/helper environment required, no skip: {}",
        raw.body
    );
    assert_eq!(raw.headers["x-codeyard-rendering"], "ssr-hydrated");
    raw.secure();
    raw.no_internal_fields();
    for text in [
        "Create a new repository",
        "name=\"owner_org_id\"",
        "canonical_org (organization)",
        "org_admin (personal)",
        "name=\"csrf_token\"",
        "method=\"post\"",
    ] {
        assert!(raw.body.contains(text), "missing {text}");
    }
    let model = f.get(&f.admin, "/new", true).await;
    let envelope: PageEnvelope = serde_json::from_str(&model.body).unwrap();
    assert_eq!(
        bootstrap(&raw).initial,
        envelope,
        "SSR/hydration receive exactly the authorized page-model props"
    );
    let target = f.org.to_string();
    let submitted = fields(
        "../bad",
        "</script><script>unsafe</script>& draft",
        "private",
        &target,
    );
    let invalid = f.form(&f.admin, "/new", &submitted, false).await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.headers["x-codeyard-rendering"], "ssr-hydrated");
    assert!(!invalid.body.contains("<script>unsafe</script>"));
    assert!(invalid.body.contains("unsafe"));
    let draft = match bootstrap(&invalid).initial.page {
        Page::Discovery(view::Page::NewRepository { form, .. }) => form,
        other => panic!("wrong raw error model: {other:?}"),
    };
    assert_eq!(draft.name, "../bad");
    assert_eq!(draft.description, submitted[1].1);
    assert_eq!(draft.owner_org_id, target);
    assert!(draft.is_private);
    let selected = invalid
        .body
        .split("<option")
        .find(|option| {
            option
                .split('>')
                .next()
                .unwrap()
                .contains(&format!("value=\"{}\"", f.org))
        })
        .unwrap();
    assert!(selected.split('>').next().unwrap().contains("selected"));
    let create = f
        .form(
            &f.admin,
            "/new",
            &fields("raw_created", "Raw native form", "private", &target),
            false,
        )
        .await;
    create.redirect("/canonical_org/raw_created");
    f.created(
        "canonical_org",
        "raw_created",
        f.admin.id,
        Some(f.org),
        true,
    )
    .await;
    let duplicate = f
        .form(
            &f.admin,
            "/new",
            &fields("raw_created", "Retained conflict draft", "private", &target),
            false,
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert!(duplicate.body.contains("Retained conflict draft"));
    assert!(matches!(bootstrap(&duplicate).initial.page,
        Page::Discovery(view::Page::NewRepository { form, .. }) if form.name == "raw_created" && form.owner_org_id == target && form.is_private));
    let paged = f
        .get(
            &f.admin,
            &format!(
                "/new?org_after={}&org_limit=1&owner_org_id={}",
                f.org, f.org
            ),
            false,
        )
        .await;
    assert_eq!(paged.status, StatusCode::OK);
    assert!(paged
        .body
        .contains("Selected organization unavailable on this page"));
    assert!(!paged.body.contains("canonical_org"));
    paged.no_internal_fields();
}

async fn owner_post(f: &Fixture, path: &str, fields: &[(&str, &str)]) -> Reply {
    f.form(&f.owner, path, fields, false).await
}

/// A settings page kept open across a rename must not reach a different repository that later
/// takes the old owner and name. Its forms name the repository by ID, so stale submissions are
/// refused and the replacement is untouched. The same delete still works on the original.
pub async fn stale_settings_forms(f: &Fixture) {
    let pool = f.database.pool();
    let old = "/personal_owner/settings_old";
    let new = "/personal_owner/settings_new";
    let vacated = fields("settings_old", "", "public", "");
    f.form(&f.owner, "/new", &vacated, true)
        .await
        .redirect("/personal_owner/settings_old");
    let original = f
        .created("personal_owner", "settings_old", f.owner.id, None, false)
        .await;
    let id = original.id.to_string();
    db::repositories::access::grant_repo_access(pool, f.member.id, original.id, AccessLevel::Read)
        .await
        .unwrap();
    let hidden = format!("name=\"repository_id\" value=\"{id}\"");
    // General settings, merge methods, rename, delete, both collaborator forms and the branch
    // protection add form carry the ID, spread over the settings sections. The transfer form
    // carries it too. A repository with rules adds an edit and a delete form for each rule, but
    // this one has none.
    let mut shown = 0;
    for tab in [
        "general",
        "merging",
        "branches",
        "collaborators",
        "labels",
        "danger",
    ] {
        let path = format!("/personal_owner/settings_old/settings?tab={tab}");
        let page = f.get(&f.owner, &path, false).await;
        assert_eq!(page.status, StatusCode::OK, "{tab}: {}", page.body);
        shown += page.body.matches(hidden.as_str()).count();
    }
    assert_eq!(shown, 8);
    let rename = [
        ("repository_id", id.as_str()),
        ("name", "settings_new"),
        ("confirm", "yes"),
    ];
    owner_post(f, &format!("{old}/_forms/rename"), &rename)
        .await
        .redirect("/personal_owner/settings_new");
    // A different private repository now has the vacated owner and name.
    let replacement_fields = fields("settings_old", "", "private", "");
    f.form(&f.owner, "/new", &replacement_fields, true)
        .await
        .redirect("/personal_owner/settings_old");
    let replacement = f
        .created("personal_owner", "settings_old", f.owner.id, None, true)
        .await;
    assert_ne!(replacement.id, original.id);
    db::repositories::access::grant_repo_access(
        pool,
        f.member.id,
        replacement.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    for access in ["read", "write", "admin"] {
        for repository_id in [id.as_str(), "", "not-an-id"] {
            let reply = owner_post(
                f,
                &format!("{old}/_forms/collaborator"),
                &[
                    ("repository_id", repository_id),
                    ("username", "org_admin"),
                    ("access", access),
                ],
            )
            .await;
            assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
        }
    }
    for action in ["collaborator", "remove-collaborator"] {
        let reply = owner_post(
            f,
            &format!("{old}/_forms/{action}"),
            &[
                ("username", "ordinary_member"),
                ("access", "admin"),
                ("confirm", "yes"),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    }
    // Forms still open on the old address must be refused before anything changes.
    let settings = [
        ("repository_id", id.as_str()),
        ("visibility", "private"),
        ("description", "stale"),
        ("confirm", "yes"),
    ];
    let stolen = [
        ("repository_id", id.as_str()),
        ("name", "stolen"),
        ("confirm", "yes"),
    ];
    let deletion = [
        ("repository_id", id.as_str()),
        ("confirmation", "personal_owner/settings_old"),
    ];
    let removal = [
        ("repository_id", id.as_str()),
        ("username", "ordinary_member"),
        ("confirm", "yes"),
    ];
    let moved = [
        ("repository_id", id.as_str()),
        ("new_owner", "canonical_org"),
        ("confirmation", "settings_old"),
    ];
    let stale: [(&str, &[(&str, &str)]); 5] = [
        ("settings", &settings[..]),
        ("rename", &stolen[..]),
        ("transfer", &moved[..]),
        ("delete", &deletion[..]),
        ("remove-collaborator", &removal[..]),
    ];
    for (action, submitted) in stale {
        let path = format!("{old}/_forms/{action}");
        let reply = owner_post(f, &path, submitted).await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{action}");
        let changed = reply.body.contains("The repository changed");
        assert!(changed, "{action}: {}", reply.body);
    }
    let kept = db::repositories::records::get_repository(pool, "personal_owner", "settings_old")
        .await
        .unwrap()
        .expect("replacement still has the vacated name");
    assert_eq!(kept.id, replacement.id);
    assert!(kept.is_private);
    for repo in [original.id, replacement.id] {
        let collaborators = db::repositories::access::list_repo_collaborators(pool, repo)
            .await
            .unwrap();
        assert_eq!(collaborators.len(), 1);
        assert_eq!(collaborators[0].user_id, f.member.id);
        assert_eq!(collaborators[0].level(), AccessLevel::Read);
    }
    // Fresh forms still grant and remove access to the repository whose ID they carry.
    let fresh_id = replacement.id.to_string();
    owner_post(
        f,
        &format!("{old}/_forms/collaborator"),
        &[
            ("repository_id", &fresh_id),
            ("username", "org_admin"),
            ("access", "write"),
        ],
    )
    .await
    .redirect(&format!("{old}/settings?tab=collaborators"));
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, f.admin.id, replacement.id)
            .await
            .unwrap(),
        AccessLevel::Write
    );
    owner_post(
        f,
        &format!("{old}/_forms/remove-collaborator"),
        &[
            ("repository_id", &fresh_id),
            ("username", "org_admin"),
            ("confirm", "yes"),
        ],
    )
    .await
    .redirect(&format!("{old}/settings?tab=collaborators"));
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, f.admin.id, replacement.id)
            .await
            .unwrap(),
        AccessLevel::None
    );
    let renamed = db::repositories::records::get_repository_by_id(pool, original.id)
        .await
        .unwrap()
        .expect("original still exists");
    assert_eq!(renamed.name, "settings_new");
    // A rejected edit keeps the ID, so its retry is checked against the same repository.
    let edit = [
        ("repository_id", id.as_str()),
        ("visibility", "secret"),
        ("confirm", "yes"),
    ];
    let path = format!("{new}/_forms/settings");
    let rejected = owner_post(f, &path, &edit).await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    let retry = rejected.body.contains(hidden.as_str());
    assert!(retry, "{}", rejected.body);
    // The same delete, sent to the original's current address, still deletes the original.
    let delete = [
        ("repository_id", id.as_str()),
        ("confirmation", "personal_owner/settings_new"),
    ];
    owner_post(f, &format!("{new}/_forms/delete"), &delete)
        .await
        .redirect("/personal_owner");
    let deleted = db::repositories::records::get_repository_by_id(pool, original.id)
        .await
        .unwrap();
    assert!(deleted.is_none());
    let survivor = db::repositories::records::get_repository_by_id(pool, replacement.id)
        .await
        .unwrap();
    assert!(survivor.is_some());
}

/// A personal repository moves into an organization its owner administers. The form needs the
/// repository name typed, a name the organization already has is refused, and the moved
/// repository keeps its ID and storage.
pub async fn transfer_form(f: &Fixture) {
    let pool = f.database.pool();
    let path = "/personal_owner/transfer_me/_forms/transfer";
    f.form(
        &f.owner,
        "/new",
        &fields("transfer_me", "", "private", ""),
        true,
    )
    .await
    .redirect("/personal_owner/transfer_me");
    let repo = f
        .created("personal_owner", "transfer_me", f.owner.id, None, true)
        .await;
    let id = repo.id.to_string();
    let untyped = [
        ("repository_id", id.as_str()),
        ("new_owner", "canonical_org"),
        ("confirmation", "transfer"),
    ];
    assert_eq!(
        owner_post(f, path, &untyped).await.status,
        StatusCode::BAD_REQUEST
    );
    let unknown = [
        ("repository_id", id.as_str()),
        ("new_owner", "no_such_owner"),
        ("confirmation", "transfer_me"),
    ];
    assert_eq!(
        owner_post(f, path, &unknown).await.status,
        StatusCode::BAD_REQUEST
    );
    let moved = [
        ("repository_id", id.as_str()),
        ("new_owner", "canonical_org"),
        ("confirmation", "transfer_me"),
    ];
    owner_post(f, path, &moved)
        .await
        .redirect("/canonical_org/transfer_me");
    let transferred = db::repositories::records::get_repository_by_id(pool, repo.id)
        .await
        .unwrap()
        .expect("transferred repository keeps its ID");
    assert_eq!(transferred.owner_org_id, Some(f.org));
    assert_eq!(transferred.owner_user_id, None);
    assert_eq!(transferred.disk_path, repo.disk_path);
    assert_eq!(
        f.get(&f.owner, "/canonical_org/transfer_me", false)
            .await
            .status,
        StatusCode::OK
    );

    // The organization now has this name, so a second personal repository cannot move there.
    f.form(
        &f.owner,
        "/new",
        &fields("transfer_me", "", "private", ""),
        true,
    )
    .await
    .redirect("/personal_owner/transfer_me");
    let again = f
        .created("personal_owner", "transfer_me", f.owner.id, None, true)
        .await;
    let again_id = again.id.to_string();
    let conflict = [
        ("repository_id", again_id.as_str()),
        ("new_owner", "canonical_org"),
        ("confirmation", "transfer_me"),
    ];
    assert_eq!(
        owner_post(f, path, &conflict).await.status,
        StatusCode::CONFLICT
    );
    let still_personal = db::repositories::records::get_repository_by_id(pool, again.id)
        .await
        .unwrap()
        .expect("refused repository is untouched");
    assert_eq!(still_personal.owner_user_id, Some(f.owner.id));
}

/// The repository action area of a rendered page, so assertion messages show what it offers.
fn action_area(body: &str) -> &str {
    let marker = "class=\"repo-engagement ";
    let after = body.split(marker).nth(1).unwrap_or("");
    after.split("repo-tabs").next().unwrap_or(after)
}

/// Native repository pages outside Git. A reader can fork a private personal repository and
/// the fork stays private. Collaborator controls follow personal ownership. Database-only
/// pages and actions keep working while Git storage is unavailable, and Git pages say so.
pub async fn repository_pages(f: &Fixture) {
    let target = f.org.to_string();
    let pool = f.database.pool();
    let private_fields = fields("fork_private", "", "private", "");
    let created = f.form(&f.owner, "/new", &private_fields, true).await;
    created.redirect("/personal_owner/fork_private");
    let source = f
        .created("personal_owner", "fork_private", f.owner.id, None, true)
        .await;
    // The fork copies real branches, so the source gets a commit before anyone forks it.
    let git = git2::Repository::open_bare(&source.disk_path).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
    let tree_id = git.treebuilder(None).unwrap().write().unwrap();
    let tree = git.find_tree(tree_id).unwrap();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "fixture",
        &tree,
        &[],
    )
    .unwrap();
    db::repositories::access::grant_repo_access(pool, f.member.id, source.id, AccessLevel::Read)
        .await
        .unwrap();
    let issues = "/personal_owner/fork_private/issues";
    let fork_path = "/personal_owner/fork_private/_forms/fork";
    let fork_form = "action=\"/personal_owner/fork_private/_forms/fork\"";
    let reader = f.get(&f.member, issues, false).await;
    assert_eq!(reader.status, StatusCode::OK, "{}", reader.body);
    let member_area = action_area(&reader.body);
    assert!(member_area.contains(fork_form), "{member_area}");
    let owner = f.get(&f.owner, issues, false).await;
    let owner_area = action_area(&owner.body);
    let own_title = "title=\"Cannot fork your own repository\"";
    assert!(!owner_area.contains("/_forms/fork"), "{owner_area}");
    assert!(owner_area.contains(own_title), "{owner_area}");
    let forked = f.form(&f.member, fork_path, &[], false).await;
    forked.redirect("/ordinary_member/fork_private");
    let fork = db::repositories::records::get_repository(pool, "ordinary_member", "fork_private")
        .await
        .unwrap()
        .expect("native fork row");
    assert!(fork.is_private);
    assert!(fork.is_fork);
    assert_eq!(fork.forked_from_id, Some(source.id));
    let fork_git = git2::Repository::open_bare(&fork.disk_path).unwrap();
    assert!(fork_git.find_reference("refs/heads/main").is_ok());

    // An archived source offers no Fork action, and the refusal holds for a site admin too.
    let archive = "UPDATE repositories SET is_archived=TRUE WHERE id=$1";
    f.database.execute(archive, &[source.id]).await;
    let staff = f.get(&f.staff, issues, false).await;
    let staff_area = action_area(&staff.body);
    let staff_title = "title=\"Archived repositories cannot be forked\"";
    assert!(!staff_area.contains("/_forms/fork"), "{staff_area}");
    assert!(staff_area.contains(staff_title), "{staff_area}");
    let refused = f.form(&f.staff, fork_path, &[], false).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);

    // Collaborator controls exist for personal repositories only. An organization owner who
    // submits the collaborator form directly is still refused.
    let personal_fields = fields("collab_personal", "", "public", "");
    let personal_created = f.form(&f.owner, "/new", &personal_fields, true).await;
    personal_created.redirect("/personal_owner/collab_personal");
    f.created("personal_owner", "collab_personal", f.owner.id, None, false)
        .await;
    let personal_settings = "/personal_owner/collab_personal/settings?tab=collaborators";
    let personal = f.get(&f.owner, personal_settings, false).await;
    assert_eq!(personal.status, StatusCode::OK, "{}", personal.body);
    let collaborator_form = "action=\"/personal_owner/collab_personal/_forms/collaborator\"";
    assert!(personal.body.contains(collaborator_form));
    let org_fields = fields("collab_org", "", "public", &target);
    let org_created = f.form(&f.owner, "/new", &org_fields, true).await;
    org_created.redirect("/canonical_org/collab_org");
    let org = f
        .created(
            "canonical_org",
            "collab_org",
            f.owner.id,
            Some(f.org),
            false,
        )
        .await;
    let org_settings = f
        .get(&f.owner, "/canonical_org/collab_org/settings", false)
        .await;
    assert_eq!(org_settings.status, StatusCode::OK, "{}", org_settings.body);
    assert!(!org_settings.body.contains("/_forms/collaborator"));
    assert!(!org_settings.body.contains("/_forms/remove-collaborator"));
    let org_id = org.id.to_string();
    let grant = [
        ("repository_id", org_id.as_str()),
        ("username", "ordinary_member"),
        ("access", "write"),
    ];
    let grant_path = "/canonical_org/collab_org/_forms/collaborator";
    let refused = f.form(&f.owner, grant_path, &grant, false).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.body);
    let access = db::repositories::access::list_repo_collaborators(pool, org.id).await;
    assert!(!access.unwrap().iter().any(|row| row.user_id == f.member.id));

    // Git storage offline: database-only pages and actions work; the Git page answers 503.
    let store_fields = fields("offline_store", "", "public", "");
    let store_created = f.form(&f.owner, "/new", &store_fields, true).await;
    store_created.redirect("/personal_owner/offline_store");
    let offline = f
        .created("personal_owner", "offline_store", f.owner.id, None, false)
        .await;
    let key = NodeKey::generate().public_hex();
    let roles = ["storage".to_owned()];
    let node = db::cluster::register_own_node(pool, &key, "offline-storage", &roles, "test")
        .await
        .unwrap();
    let assign = "UPDATE repositories SET storage_node_id=$1 WHERE id=$2";
    f.database.execute(assign, &[node.id, offline.id]).await;
    let offline_issues = f
        .get(&f.owner, "/personal_owner/offline_store/issues", false)
        .await;
    assert_eq!(
        offline_issues.status,
        StatusCode::OK,
        "{}",
        offline_issues.body
    );
    assert!(!offline_issues
        .body
        .contains("Git storage is temporarily unavailable."));
    let star = "/personal_owner/offline_store/_forms/star";
    let starred = f.form(&f.owner, star, &[], false).await;
    starred.redirect("/personal_owner/offline_store");
    let stars = db::repositories::engagement::is_repo_starred(pool, f.owner.id, offline.id);
    assert!(stars.await.unwrap());
    let offline_settings = "/personal_owner/offline_store/settings";
    let settings = f.get(&f.owner, offline_settings, false).await;
    assert_eq!(settings.status, StatusCode::OK, "{}", settings.body);
    let code = f
        .get(&f.owner, "/personal_owner/offline_store", false)
        .await;
    assert_eq!(
        code.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        code.body
    );
    assert!(code
        .body
        .contains("Git storage is temporarily unavailable."));
}
