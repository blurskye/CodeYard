//! Profile pictures of users and organizations, on SQLite and PostgreSQL: a user
//! points their picture at a confirmed upload through the JSON API or sends a
//! picture from the settings form; organization owners and admins do the same
//! from the organization page. Only the holder's own confirmed upload made for
//! that picture can be used, and a replaced picture is deleted.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as pulls;
use axum::http::StatusCode;
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    db::{
        self,
        models::OrgRole,
        organizations::{self, NewOrganization, OrganizationOutcome, OrganizationProfile},
    },
    identity::avatars,
    storage::ObjectStore,
};
use database::Database;
use pulls::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

/// Something a browser might send as a "picture".
const NOT_A_PICTURE: &[u8] = b"<html>not a picture</html>";

/// A one-pixel PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// Uploads through the files API the way a client does: reserve, send the bytes,
/// and (unless `confirm` is false) confirm. Returns the file id.
async fn upload(client: &Client, reason: &str, bytes: &[u8], confirm: bool) -> String {
    let reserved = client
        .expect(
            "POST",
            "/api/files",
            json!({
                "filename": "me.png",
                "content_type": "image/png",
                "file_size": bytes.len(),
                "reason": reason,
                "visibility": if reason == "avatar" || reason == "org_avatar" { "public" } else { "owner" },
                "sha256_hex": hex::encode(Sha256::digest(bytes)),
            }),
            StatusCode::OK,
        )
        .await;
    let id = reserved["file_id"].as_str().unwrap().to_owned();
    let sent = client
        .exchange(
            "PUT",
            reserved["upload_url"].as_str().unwrap(),
            "image/png",
            bytes.to_vec(),
        )
        .await;
    assert_eq!(sent.status, StatusCode::NO_CONTENT, "{}", sent.text());
    if confirm {
        client
            .expect(
                "POST",
                "/api/files/confirm",
                json!({"file_id": id}),
                StatusCode::OK,
            )
            .await;
    }
    id
}

async fn set_avatar(client: &Client, file_id: &str) -> (StatusCode, Value) {
    client
        .api("PUT", "/api/user/avatar", json!({"file_id": file_id}))
        .await
}

async fn avatar_url(client: &Client) -> Option<String> {
    let me = client
        .expect("GET", "/api/auth/me", Value::Null, StatusCode::OK)
        .await;
    me["user"]["avatar_url"].as_str().map(str::to_owned)
}

/// What anyone can download of a file right now.
async fn download(client: &Client, id: &str) -> StatusCode {
    client
        .exchange(
            "GET",
            &format!("/api/files/{id}/download"),
            "application/json",
            Vec::new(),
        )
        .await
        .status
}

fn id_of(url: &str) -> &str {
    url.strip_prefix("/api/files/")
        .and_then(|rest| rest.strip_suffix("/download"))
        .unwrap_or_else(|| panic!("an avatar url, got {url}"))
}

/// Takes every free picture slot and keeps them until the returned permits drop.
fn take_picture_slots(storage: &ObjectStore) -> Vec<tokio::sync::OwnedSemaphorePermit> {
    let mut permits = Vec::new();
    while let Some(permit) = storage.try_acquire_picture() {
        permits.push(permit);
    }
    permits
}

/// Takes every free storage I/O slot and keeps them until the returned permits drop.
fn take_io_slots(storage: &ObjectStore) -> Vec<tokio::sync::OwnedSemaphorePermit> {
    let mut permits = Vec::new();
    while let Ok(permit) = storage.try_acquire_io() {
        permits.push(permit);
    }
    permits
}

async fn organization(pool: &db::DbPool, owner: i64, slug: &str) -> i64 {
    let profile = OrganizationProfile::new(None, None).unwrap();
    match organizations::create_organization(
        pool,
        owner,
        &NewOrganization::new(slug, profile).unwrap(),
    )
    .await
    .unwrap()
    {
        OrganizationOutcome::Applied(org) => org.id,
        other => panic!("organization was not created: {other:?}"),
    }
}

async fn contract(pg: bool) {
    // The HTML pages below need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let ids = [
        f.actor("alice").await,
        f.actor("bob").await,
        f.actor("carol").await,
        f.actor("dave").await,
        f.actor("erin").await,
    ];
    let root = tempfile::tempdir().unwrap();
    let storage = Arc::new(ObjectStore::open(root.path(), None).await.unwrap());
    let router = create_router(AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        storage.clone(),
        None,
        CiConfig::default(),
        None,
    ));
    let alice = Client::login(&router, f.pool(), ids[0]).await;
    let bob = Client::login(&router, f.pool(), ids[1]).await;
    let carol = Client::login(&router, f.pool(), ids[2]).await;
    let dave = Client::login(&router, f.pool(), ids[3]).await;
    let erin = Client::login(&router, f.pool(), ids[4]).await;
    let nobody = Client::anonymous(&router);

    // ----- A user's picture through the JSON API -----
    assert_eq!(avatar_url(&alice).await, None);
    let first = upload(&alice, "avatar", PNG, true).await;
    let (status, body) = set_avatar(&alice, &first).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = body["avatar_url"].as_str().unwrap().to_owned();
    assert_eq!(id_of(&url), first);
    assert_eq!(avatar_url(&alice).await.as_deref(), Some(url.as_str()));
    // Everyone can fetch it, the owner and strangers alike, and so can a visitor.
    for client in [&alice, &bob, &nobody] {
        assert_eq!(download(client, &first).await, StatusCode::OK);
    }

    // The same picture has one address by name. A name with a picture goes to its file; any
    // other name, an unknown one included, gets a generated square and never a 404.
    let by_name = nobody
        .exchange("GET", "/avatars/alice", "text/html", Vec::new())
        .await;
    assert_eq!(by_name.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(by_name.location.as_deref(), Some(url.as_str()));
    for name in ["bob", "nobody-by-this-name"] {
        let generated = nobody
            .exchange("GET", &format!("/avatars/{name}"), "text/html", Vec::new())
            .await;
        assert_eq!(generated.status, StatusCode::OK, "{name}");
        assert!(generated.text().starts_with("<svg"), "{name}");
    }

    // Only the user's own confirmed upload made for a picture can be used.
    let bobs = upload(&bob, "avatar", PNG, true).await;
    let other_reason = upload(&alice, "other", PNG, true).await;
    let pending = upload(&alice, "avatar", PNG, false).await;
    let organization_picture = upload(&alice, "org_avatar", PNG, true).await;
    for file in [
        bobs.as_str(),
        other_reason.as_str(),
        pending.as_str(),
        organization_picture.as_str(),
        "00000000-0000-4000-8000-000000000000",
    ] {
        let (status, body) = set_avatar(&alice, file).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{file}: {body}");
        assert_eq!(body["error"], "unusable_file", "{file}: {body}");
    }
    assert_eq!(avatar_url(&alice).await.as_deref(), Some(url.as_str()));
    assert_eq!(
        set_avatar(&nobody, &first).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        nobody
            .api("DELETE", "/api/user/avatar", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    // A new picture replaces the old one, which is deleted.
    let second = upload(&alice, "avatar", PNG, true).await;
    let (status, _) = set_avatar(&alice, &second).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(download(&bob, &first).await, StatusCode::NOT_FOUND);
    assert_eq!(download(&alice, &first).await, StatusCode::NOT_FOUND);
    assert_eq!(download(&bob, &second).await, StatusCode::OK);

    // Removing it deletes that file too.
    alice
        .expect(
            "DELETE",
            "/api/user/avatar",
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    assert_eq!(avatar_url(&alice).await, None);
    assert_eq!(download(&bob, &second).await, StatusCode::NOT_FOUND);

    // ----- A user's picture from the settings form -----
    let (status, page) = alice.page("/settings?tab=profile").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Profile picture"));
    assert!(page.contains("action=\"/settings/avatar\""), "{page}");
    assert!(page.contains("multipart/form-data"));
    // The upload button sits beside Remove but submits the upload form by its id.
    assert!(page.contains("id=\"profile-picture-upload\""));
    assert!(page.contains("form=\"profile-picture-upload\""));
    assert!(!page.contains("Remove picture"), "nothing to remove yet");
    // A competing request takes each released slot. This upload must keep its
    // original admission through verification rather than race for a second slot.
    use std::future::Future;
    let mut slots = take_io_slots(&storage);
    drop(slots.pop());
    let fields = [("csrf_token", alice.csrf.as_str())];
    let sent = alice.multipart("/settings/avatar", &fields, Some(("image/png", PNG)));
    tokio::pin!(sent);
    let mut admitted = false;
    let sent = std::future::poll_fn(|cx| {
        let result = sent.as_mut().poll(cx);
        if admitted {
            slots.extend(take_io_slots(&storage));
        } else if let Ok(slot) = storage.try_acquire_io() {
            drop(slot);
        } else {
            admitted = true;
        }
        result
    })
    .await;
    assert!(admitted);
    drop(slots);
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.text());
    assert_eq!(
        sent.location.as_deref(),
        Some("/settings?tab=profile&saved=avatar")
    );
    let url = avatar_url(&alice).await.expect("the form set a picture");
    assert_eq!(download(&nobody, id_of(&url)).await, StatusCode::OK);
    let (_, page) = alice.page("/settings?tab=profile&saved=avatar").await;
    assert!(page.contains("Profile picture updated."), "{page}");
    assert!(page.contains(&url), "the page shows the picture");
    assert!(page.contains("Remove picture"));
    let (_, profile) = nobody.page("/alice").await;
    assert!(profile.contains(&url), "the profile page shows it");

    // Every picture slot taken: the form is refused as busy at once, without being read.
    let slots = take_picture_slots(&storage);
    let fields = [("csrf_token", alice.csrf.as_str())];
    let sent = alice.multipart("/settings/avatar", &fields, Some(("image/png", PNG)));
    let busy = tokio::time::timeout(Duration::from_secs(10), sent)
        .await
        .expect("a busy picture form is refused, not waited for");
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.text()
    );
    let page = busy.text();
    assert!(
        page.contains("Pictures cannot be changed right now"),
        "{page}"
    );
    drop(slots);
    assert_eq!(avatar_url(&alice).await.as_deref(), Some(url.as_str()));

    // What is not a picture, is too large, or does not come with the page's token
    // changes nothing.
    let mut huge = PNG.to_vec();
    huge.resize(2 * 1024 * 1024 + 1, 0);
    let attempts = [
        (
            alice
                .multipart(
                    "/settings/avatar",
                    &[("csrf_token", &alice.csrf)],
                    Some(("image/png", NOT_A_PICTURE)),
                )
                .await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose a PNG, JPEG, GIF or WebP picture",
        ),
        (
            alice
                .multipart(
                    "/settings/avatar",
                    &[("csrf_token", &alice.csrf)],
                    Some(("image/png", huge.as_slice())),
                )
                .await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "larger than 2 MB",
        ),
        (
            alice
                .multipart("/settings/avatar", &[("csrf_token", &alice.csrf)], None)
                .await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose a picture to upload.",
        ),
        (
            alice
                .multipart("/settings/avatar", &[], Some(("image/png", PNG)))
                .await,
            StatusCode::FORBIDDEN,
            "form expired",
        ),
        (
            alice
                .multipart(
                    "/settings/avatar",
                    &[("csrf_token", &bob.csrf)],
                    Some(("image/png", PNG)),
                )
                .await,
            StatusCode::FORBIDDEN,
            "form expired",
        ),
    ];
    for (reply, status, message) in attempts {
        assert_eq!(reply.status, status, "{}", reply.text());
        assert!(
            reply.text().contains(message),
            "{message}: {}",
            reply.text()
        );
    }
    let (status, page) = alice
        .send(
            "POST",
            "/settings/avatar",
            "application/x-www-form-urlencoded",
            format!("csrf_token={}", alice.csrf),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert_eq!(avatar_url(&alice).await.as_deref(), Some(url.as_str()));
    let visitor = nobody
        .multipart(
            "/settings/avatar",
            &[("csrf_token", &nobody.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(visitor.status, StatusCode::SEE_OTHER);
    assert_eq!(visitor.location.as_deref(), Some("/login"));

    // Pictures far larger than a settings form (but within 2 MB) are accepted.
    for (client, size) in [(&bob, 1536 * 1024), (&dave, 2 * 1024 * 1024)] {
        let mut large = PNG.to_vec();
        large.resize(size, 0);
        let sent = client
            .multipart(
                "/settings/avatar",
                &[("csrf_token", &client.csrf)],
                Some(("image/png", large.as_slice())),
            )
            .await;
        assert_eq!(
            sent.status,
            StatusCode::SEE_OTHER,
            "{size} bytes: {}",
            sent.text()
        );
    }

    // The remove form clears it and deletes the file.
    let (status, page) = alice.form("/settings/avatar/remove", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(avatar_url(&alice).await, None);
    assert_eq!(download(&nobody, id_of(&url)).await, StatusCode::NOT_FOUND);

    // Concurrent API selections use the current database value, not the stale
    // request user, so exactly one selected upload remains.
    let frank_id = f.actor("frank").await;
    let frank = Arc::new(Client::login(&router, f.pool(), frank_id).await);
    let first = upload(&frank, "avatar", PNG, true).await;
    let mut changed_png = PNG.to_vec();
    changed_png.push(0);
    let second = upload(&frank, "avatar", &changed_png, true).await;
    let first_task = {
        let frank = frank.clone();
        let first = first.clone();
        tokio::spawn(async move { set_avatar(&frank, &first).await })
    };
    let second_task = {
        let frank = frank.clone();
        let second = second.clone();
        tokio::spawn(async move { set_avatar(&frank, &second).await })
    };
    assert_eq!(first_task.await.unwrap().0, StatusCode::OK);
    assert_eq!(second_task.await.unwrap().0, StatusCode::OK);
    let selected = avatar_url(&frank).await.unwrap();
    assert!(selected.contains(&first) || selected.contains(&second));
    let stats = db::files::get_user_file_stats(f.pool(), frank_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.avatar_count, 1);
    assert_eq!(download(&nobody, id_of(&selected)).await, StatusCode::OK);

    // Concurrent native uploads start with the same request-level user. The
    // transaction returns the actual predecessor, so the intermediate file is
    // removed and the final selected bytes survive.
    let request_user = db::users::accounts::get_user_by_id(f.pool(), frank_id)
        .await
        .unwrap()
        .unwrap();
    let upload_one = {
        let pool = f.pool().clone();
        let storage = storage.clone();
        let user = request_user.clone();
        tokio::spawn(async move {
            avatars::upload_user_avatar(&pool, &storage, &user, PNG.to_vec()).await
        })
    };
    let upload_two = {
        let pool = f.pool().clone();
        let storage = storage.clone();
        let user = request_user.clone();
        let bytes = changed_png.clone();
        tokio::spawn(
            async move { avatars::upload_user_avatar(&pool, &storage, &user, bytes).await },
        )
    };
    upload_one.await.unwrap().unwrap();
    upload_two.await.unwrap().unwrap();
    let selected = avatar_url(&frank).await.unwrap();
    let stats = db::files::get_user_file_stats(f.pool(), frank_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.avatar_count, 1);
    assert_eq!(download(&nobody, id_of(&selected)).await, StatusCode::OK);

    // Storage busy: the picture is refused before its quota is charged, so the five
    // uploads below still fit within the hour.
    let slots = take_io_slots(&storage);
    let busy = erin
        .multipart(
            "/settings/avatar",
            &[("csrf_token", &erin.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.text()
    );
    drop(slots);

    // Pictures a user may send in an hour are limited; the next one is refused.
    for _ in 0..5 {
        let sent = erin
            .multipart(
                "/settings/avatar",
                &[("csrf_token", &erin.csrf)],
                Some(("image/png", PNG)),
            )
            .await;
        assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.text());
    }
    let sixth = erin
        .multipart(
            "/settings/avatar",
            &[("csrf_token", &erin.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(
        sixth.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        sixth.text()
    );
    assert!(sixth.text().contains("Too many pictures"));

    // ----- An organization's picture -----
    let org = organization(f.pool(), ids[2], "acme").await;
    for (user, role) in [(ids[1], OrgRole::Admin), (ids[3], OrgRole::Member)] {
        assert!(matches!(
            organizations::add_organization_member(f.pool(), ids[2], org, user, role)
                .await
                .unwrap(),
            OrganizationOutcome::Applied(_)
        ));
    }
    let base = format!("/settings/organizations/{org}");
    let upload_path = format!("{base}/avatar");
    let remove_path = format!("{base}/avatar/remove");
    let (status, page) = carol.page(&base).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Organization picture"));
    assert!(
        page.contains(&format!("action=\"{upload_path}\"")),
        "{page}"
    );

    // Every picture slot taken: the organization form is refused as busy too.
    let slots = take_picture_slots(&storage);
    let busy = carol
        .multipart(
            &upload_path,
            &[("csrf_token", &carol.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(
        busy.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        busy.text()
    );
    let page = busy.text();
    assert!(
        page.contains("Pictures cannot be changed right now"),
        "{page}"
    );
    drop(slots);

    // An owner sets it.
    let sent = carol
        .multipart(
            &upload_path,
            &[("csrf_token", &carol.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.text());
    assert_eq!(sent.location.as_deref(), Some(base.as_str()));
    let first_org = match organizations::get_organization_avatar(f.pool(), ids[2], org)
        .await
        .unwrap()
    {
        OrganizationOutcome::Applied(Some(file)) => file.to_string(),
        other => panic!("the organization has no picture: {other:?}"),
    };
    let org_url = format!("/api/files/{first_org}/download");
    assert_eq!(download(&nobody, &first_org).await, StatusCode::OK);
    for client in [&carol, &bob, &dave] {
        let (_, page) = client.page(&base).await;
        assert!(page.contains(&org_url), "members see the picture");
    }
    // The organization's name is its address for the picture too.
    let by_org_name = nobody
        .exchange("GET", "/avatars/acme", "text/html", Vec::new())
        .await;
    assert_eq!(by_org_name.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(by_org_name.location.as_deref(), Some(org_url.as_str()));
    let (status, _) = erin.page(&base).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "outsiders do not see the organization"
    );

    // An admin replaces it and the old file is deleted; a plain member cannot.
    let sent = bob
        .multipart(
            &upload_path,
            &[("csrf_token", &bob.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.text());
    assert_eq!(download(&nobody, &first_org).await, StatusCode::NOT_FOUND);
    let by_member = dave
        .multipart(
            &upload_path,
            &[("csrf_token", &dave.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(
        by_member.status,
        StatusCode::FORBIDDEN,
        "{}",
        by_member.text()
    );
    let by_outsider = erin
        .multipart(
            &upload_path,
            &[("csrf_token", &erin.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(
        by_outsider.status,
        StatusCode::NOT_FOUND,
        "{}",
        by_outsider.text()
    );
    let unknown = carol
        .multipart(
            "/settings/organizations/not-a-number/avatar",
            &[("csrf_token", &carol.csrf)],
            Some(("image/png", PNG)),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let without_token = carol
        .multipart(&upload_path, &[], Some(("image/png", PNG)))
        .await;
    assert_eq!(without_token.status, StatusCode::FORBIDDEN);
    let second_org = match organizations::get_organization_avatar(f.pool(), ids[2], org)
        .await
        .unwrap()
    {
        OrganizationOutcome::Applied(Some(file)) => file.to_string(),
        other => panic!("{other:?}"),
    };
    assert_ne!(second_org, first_org);
    assert_eq!(download(&nobody, &second_org).await, StatusCode::OK);

    // The store only takes the actor's own confirmed `org_avatar` upload, from an
    // owner or admin.
    let carols = upload(&carol, "org_avatar", PNG, true).await;
    let carols_pending = upload(&carol, "org_avatar", PNG, false).await;
    let carols_user_picture = upload(&carol, "avatar", PNG, true).await;
    let bobs_org_picture = upload(&bob, "org_avatar", PNG, true).await;
    let uuid = |text: &str| uuid::Uuid::parse_str(text).unwrap();
    for (file, why) in [
        (bobs_org_picture.as_str(), "someone else's file"),
        (carols_pending.as_str(), "an unconfirmed upload"),
        (carols_user_picture.as_str(), "a user picture"),
        ("00000000-0000-4000-8000-000000000000", "no such file"),
    ] {
        let outcome =
            organizations::set_organization_avatar(f.pool(), ids[2], org, Some(uuid(file)))
                .await
                .unwrap();
        assert_eq!(outcome, OrganizationOutcome::Conflict, "{why}");
    }
    assert_eq!(
        organizations::set_organization_avatar(f.pool(), ids[3], org, Some(uuid(&carols)))
            .await
            .unwrap(),
        OrganizationOutcome::Denied,
        "a member cannot"
    );
    assert_eq!(
        organizations::set_organization_avatar(f.pool(), ids[4], org, None)
            .await
            .unwrap(),
        OrganizationOutcome::Denied,
        "an outsider cannot"
    );
    assert_eq!(
        organizations::set_organization_avatar(f.pool(), ids[2], org + 99, None)
            .await
            .unwrap(),
        OrganizationOutcome::Missing
    );
    // It names the picture it replaced.
    assert_eq!(
        organizations::set_organization_avatar(f.pool(), ids[2], org, Some(uuid(&carols)))
            .await
            .unwrap(),
        OrganizationOutcome::Applied(Some(uuid(&second_org)))
    );
    assert_eq!(
        download(&nobody, &carols).await,
        StatusCode::OK,
        "public now"
    );

    // Replacing a picture removes the old one in the same transaction, so it cannot
    // be selected again; cleanup never removes the picture that is still selected.
    assert_eq!(
        organizations::set_organization_avatar(
            f.pool(),
            ids[1],
            org,
            Some(uuid(&bobs_org_picture))
        )
        .await
        .unwrap(),
        OrganizationOutcome::Applied(Some(uuid(&carols)))
    );
    assert_eq!(
        organizations::set_organization_avatar(f.pool(), ids[2], org, Some(uuid(&carols)))
            .await
            .unwrap(),
        OrganizationOutcome::Conflict
    );
    assert_eq!(download(&nobody, &carols).await, StatusCode::NOT_FOUND);
    assert!(!codeyard::backend::files::delete_unreferenced_file(
        f.pool(),
        &storage,
        ids[1],
        uuid(&bobs_org_picture)
    )
    .await
    .unwrap());
    assert_eq!(download(&nobody, &bobs_org_picture).await, StatusCode::OK);

    // A former administrator who uploaded the selected organization picture
    // cannot bypass current organization authority through generic file DELETE.
    let bob_user = db::users::accounts::get_user_by_id(f.pool(), ids[1])
        .await
        .unwrap()
        .unwrap();
    avatars::set_org_avatar(f.pool(), &bob_user, org, Some(uuid(&bobs_org_picture)))
        .await
        .unwrap();
    assert!(matches!(
        organizations::remove_organization_member(f.pool(), ids[2], org, ids[1])
            .await
            .unwrap(),
        OrganizationOutcome::Applied(())
    ));
    let (status, body) = bob
        .api(
            "DELETE",
            &format!("/api/files/{bobs_org_picture}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "file_in_use");
    assert_eq!(
        organizations::get_organization_avatar(f.pool(), ids[2], org)
            .await
            .unwrap(),
        OrganizationOutcome::Applied(Some(uuid(&bobs_org_picture)))
    );
    assert_eq!(download(&nobody, &bobs_org_picture).await, StatusCode::OK);

    // The authority-checked remove path still works; a plain member cannot use it.
    let (status, page) = dave.form(&remove_path, &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    let (status, page) = carol.form(&remove_path, &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(
        download(&nobody, &bobs_org_picture).await,
        StatusCode::NOT_FOUND
    );
    assert!(matches!(
        organizations::get_organization_avatar(f.pool(), ids[2], org)
            .await
            .unwrap(),
        OrganizationOutcome::Applied(None)
    ));
    let (_, page) = carol.page(&base).await;
    assert!(
        !page.contains("/api/files/"),
        "no picture on the page: {page}"
    );
    assert!(!page.contains("Remove picture"));
    // Without a picture the organization's name gets the generated square.
    let generated = nobody
        .exchange("GET", "/avatars/acme", "text/html", Vec::new())
        .await;
    assert_eq!(generated.status, StatusCode::OK);
    assert!(generated.text().starts_with("<svg"), "{}", generated.text());

    // Picture types are recognized by their bytes.
    assert_eq!(avatars::picture_type(PNG), Some("image/png"));
    assert_eq!(
        avatars::picture_type(b"\xff\xd8\xff\xe0"),
        Some("image/jpeg")
    );
    assert_eq!(avatars::picture_type(b"GIF89a.."), Some("image/gif"));
    assert_eq!(
        avatars::picture_type(b"RIFF\0\0\0\0WEBPVP8 "),
        Some("image/webp")
    );
    assert_eq!(avatars::picture_type(b"<svg></svg>"), None);
    assert_eq!(avatars::picture_type(b""), None);

    f.finish().await;
}

async fn pending_picture_contract(pg: bool) {
    use codeyard::backend::files::{self, UploadDeclaration};
    use db::models::{FileReason, FileVisibility};
    let mut f = Database::new(pg).await;
    let actor = f.actor("pending_picture").await;
    let org = organization(f.pool(), actor, "pending-org").await;
    let root = tempfile::tempdir().unwrap();
    let storage = ObjectStore::open(root.path(), None).await.unwrap();
    let checksum = hex::encode(Sha256::digest(PNG));
    for (reason, table) in [
        (FileReason::Avatar, "users"),
        (FileReason::OrgAvatar, "organizations"),
    ] {
        let store = || {
            files::store_upload(
                f.pool(),
                &storage,
                actor,
                UploadDeclaration {
                    filename: "avatar",
                    content_type: "image/png",
                    file_size: PNG.len() as i64,
                    reason,
                    visibility: FileVisibility::Public,
                    sha256_hex: &checksum,
                },
                PNG.to_vec(),
            )
        };
        let installed = store().await.unwrap();
        let interrupted = store().await.unwrap();
        for id in [installed, interrupted] {
            let file = db::files::get_file_by_id(f.pool(), id)
                .await
                .unwrap()
                .unwrap();
            assert!(!file.is_active);
            assert!(file.confirmed_at.is_some() && file.upload_expires_at.is_some());
            assert!(db::files::get_downloadable_file(f.pool(), id, None)
                .await
                .unwrap()
                .is_none());
        }
        let pool = f.pool();
        let assign = |id| async move {
            if reason == FileReason::Avatar {
                db::users::accounts::set_user_avatar(pool, actor, Some(id))
                    .await
                    .map(|outcome| {
                        assert_eq!(
                            outcome,
                            db::users::accounts::UserAvatarOutcome::Applied(None)
                        );
                    })
            } else {
                organizations::set_organization_avatar(pool, actor, org, Some(id))
                    .await
                    .map(|outcome| {
                        assert_eq!(outcome, OrganizationOutcome::Applied(None));
                    })
            }
        };
        if pg {
            f.execute("CREATE FUNCTION reject_picture_assignment() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'assignment unavailable'; END $$", &[]).await;
            f.execute(&format!("CREATE TRIGGER reject_picture BEFORE UPDATE OF avatar_file_id ON {table} FOR EACH ROW EXECUTE FUNCTION reject_picture_assignment()"), &[]).await;
        } else {
            f.execute(&format!("CREATE TRIGGER reject_picture BEFORE UPDATE OF avatar_file_id ON {table} BEGIN SELECT RAISE(ABORT, 'assignment unavailable'); END"), &[]).await;
        }
        assert!(assign(installed).await.is_err());
        let file = db::files::get_file_by_id(f.pool(), installed)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !file.is_active && file.upload_expires_at.is_some(),
            "activation must roll back with assignment"
        );
        if pg {
            f.execute(&format!("DROP TRIGGER reject_picture ON {table}"), &[])
                .await;
            f.execute("DROP FUNCTION reject_picture_assignment()", &[])
                .await;
        } else {
            f.execute("DROP TRIGGER reject_picture", &[]).await;
        }
        assign(installed).await.unwrap();
        let file = db::files::get_file_by_id(f.pool(), installed)
            .await
            .unwrap()
            .unwrap();
        assert!(file.is_active && file.upload_expires_at.is_none());
        assert!(db::files::get_downloadable_file(f.pool(), installed, None)
            .await
            .unwrap()
            .is_some());
        f.execute(
            "UPDATE files SET upload_expires_at='2000-01-01T00:00:00.000000Z' WHERE NOT is_active",
            &[],
        )
        .await;
        if reason == FileReason::Avatar {
            assert_eq!(
                db::users::accounts::set_user_avatar(f.pool(), actor, Some(interrupted))
                    .await
                    .unwrap(),
                db::users::accounts::UserAvatarOutcome::NotUsable
            );
        } else {
            assert_eq!(
                organizations::set_organization_avatar(f.pool(), actor, org, Some(interrupted))
                    .await
                    .unwrap(),
                OrganizationOutcome::Conflict
            );
        }
        assert_eq!(
            files::remove_expired_uploads(f.pool(), &storage)
                .await
                .unwrap(),
            1
        );
        assert!(db::files::get_file_by_id(f.pool(), interrupted)
            .await
            .unwrap()
            .is_none());
        assert!(db::files::get_file_by_id(f.pool(), installed)
            .await
            .unwrap()
            .is_some());
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_pending_pictures_activate_only_with_assignment() {
    pending_picture_contract(false).await;
}

#[tokio::test]
async fn postgres_pending_pictures_activate_only_with_assignment() {
    pending_picture_contract(true).await;
}

#[tokio::test]
async fn sqlite_pictures_of_users_and_organizations() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_pictures_of_users_and_organizations() {
    contract(true).await;
}
