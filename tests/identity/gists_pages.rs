//! The native gist forms work without product JavaScript and use the same stored Git data.
use crate::support::{
    database::Database,
    http_client::{hidden_value, Client},
};
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use codeyard::{
    backend::db,
    webui::{app::Page, pages::gists},
};
use tower::ServiceExt;

async fn pages(pg: bool) {
    let _fixture = super::github_gists::fixture_slot().await;
    crate::support::fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let bob = f.actor("bob").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let a = Client::login(&router, f.pool(), alice).await;
    let b = Client::login(&router, f.pool(), bob).await;
    let guest = Client::anonymous(&router);
    let new = a.page_ok("/gists/new").await;
    assert!(new.contains("name=\"content_0\""));
    assert!(new.contains("action=\"/gists/new\""));
    assert!(new.contains("Secret gists"));
    let source = "\n# Notes\n<script>alert('unsafe')</script>\n🌻\n";
    let add = a
        .form_reply(
            "/gists/new",
            &[
                ("file_count", "1"),
                ("description", "Draft notes"),
                ("name_0", "notes.md"),
                ("content_0", source),
                ("intent", "add-file"),
            ],
        )
        .await;
    assert_eq!(add.status, 200, "{}", add.text());
    assert!(add.text().contains("name=\"content_1\""));
    assert!(add.text().contains("Draft notes"));
    assert!(!add.text().contains("<script>alert('unsafe')"));
    let saved = a
        .form_reply(
            "/gists/new",
            &[
                ("file_count", "2"),
                ("description", "Draft notes"),
                ("name_0", "notes.md"),
                ("content_0", source),
                ("name_1", "second.txt"),
                ("content_1", "second\n"),
                ("intent", "save"),
            ],
        )
        .await;
    assert_eq!(saved.status, 303, "{}", saved.text());
    let path = saved.location.unwrap();
    let id = path.strip_prefix("/gists/").unwrap();
    let record = db::gists::get(f.pool(), id).await.unwrap().unwrap();
    assert_eq!(record.owner_id, alice);
    assert!(!record.is_public);
    let response = router
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers()["x-robots-tag"]
        .to_str()
        .unwrap()
        .contains("noindex"));
    let html = String::from_utf8(
        to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Secret gist"));
    assert!(html.contains("Forks"));
    assert!(html.contains("Revision history"));
    assert!(!html.contains("<script>alert('unsafe')"));
    assert!(!html.contains("disk_path"));
    assert!(!html.contains("password_hash"));
    assert!(html.contains("gists.css"));
    let (status, model) = a.model(&format!("{path}/edit")).await;
    assert_eq!(status, 200);
    let Page::Gists(gists::Page::Editor(editor)) = model else {
        panic!("expected editor")
    };
    let first = editor.expected.clone();
    assert_eq!(editor.files[0].content.as_deref(), Some(source));
    assert_eq!(editor.files[0].original.as_deref(), Some("notes.md"));
    let edit = a.page_ok(&format!("{path}/edit")).await;
    assert_eq!(hidden_value(&edit, "expected"), first);
    assert_eq!(b.page(&format!("{path}/edit")).await.0, 403);
    assert_eq!(guest.page("/gists/new").await.0, 303);
    let raw = format!("{path}/raw/{first}/notes.md");
    let reply = guest.exchange("GET", &raw, "text/plain", vec![]).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, source.as_bytes());
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&raw)
                .header("range", "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert!(response.headers()["content-range"]
        .to_str()
        .unwrap()
        .starts_with("bytes 0-3/"));
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert!(response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("sandbox"));
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        &source.as_bytes()[..4]
    );
    let no_csrf = a
        .exchange(
            "POST",
            &format!("{path}/star"),
            "application/x-www-form-urlencoded",
            b"add=yes".to_vec(),
        )
        .await;
    assert_eq!(no_csrf.status, 403);
    assert!(!db::gists::starred(f.pool(), alice, record.repo_id)
        .await
        .unwrap());
    let changed = a
        .form_reply(
            &format!("{path}/edit"),
            &[
                ("file_count", "2"),
                ("description", "Updated notes"),
                ("expected", &first),
                ("original_0", "notes.md"),
                ("name_0", "renamed.md"),
                ("content_0", "# Revised\n"),
                ("original_1", "second.txt"),
                ("name_1", "second.txt"),
                ("delete_1", "yes"),
            ],
        )
        .await;
    assert_eq!(changed.status, 303, "{}", changed.text());
    let stale = a
        .form_reply(
            &format!("{path}/edit"),
            &[
                ("file_count", "1"),
                ("description", "Keep this draft"),
                ("expected", &first),
                ("original_0", "notes.md"),
                ("name_0", "notes.md"),
                ("content_0", "stale-draft"),
            ],
        )
        .await;
    assert_eq!(stale.status, 409, "{}", stale.text());
    assert!(stale.text().contains("stale-draft"));
    assert_eq!(hidden_value(&stale.text(), "expected"), first);
    let (_, model) = a.model(&format!("{path}/edit")).await;
    let Page::Gists(gists::Page::Editor(editor)) = model else {
        panic!("expected editor")
    };
    assert_ne!(editor.expected, first);
    assert_eq!(editor.files.len(), 1);
    assert_eq!(editor.files[0].filename, "renamed.md");
    assert_eq!(editor.files[0].content.as_deref(), Some("# Revised\n"));
    assert_eq!(
        guest.exchange("GET", &raw, "text/plain", vec![]).await.body,
        source.as_bytes()
    );
    let (_, model) = guest.model(&format!("{path}?revision={first}")).await;
    let Page::Gists(gists::Page::Detail(old)) = model else {
        panic!("expected gist")
    };
    assert!(old.historical);
    assert_eq!(old.files.len(), 2);
    assert_eq!(old.revision, first);
    assert!(!guest.page_ok("/gists?tab=public").await.contains(id));
    assert!(a.page_ok("/gists?tab=mine").await.contains(id));
    assert!(!guest.page_ok("/gists?user=alice").await.contains(id));
    let starred = b
        .form_reply(&format!("{path}/star"), &[("add", "yes")])
        .await;
    assert_eq!(starred.status, 303);
    assert!(db::gists::starred(f.pool(), bob, record.repo_id)
        .await
        .unwrap());
    assert!(b.page_ok("/gists?tab=starred").await.contains(id));
    let commented = b
        .form_reply(&format!("{path}/comments"), &[("body", "**Useful** notes")])
        .await;
    assert_eq!(commented.status, 303, "{}", commented.text());
    let comments = db::gists::comments(f.pool(), record.repo_id, 30, 0)
        .await
        .unwrap();
    let comment = comments[0].id;
    let (_, model) = a.model(&path).await;
    let Page::Gists(gists::Page::Detail(page)) = model else {
        panic!("expected gist")
    };
    assert!(!page.comments[0].can_edit);
    assert!(page.comments[0].can_delete);
    let forbidden = a
        .form_reply(
            &format!("{path}/comments/{comment}/edit"),
            &[("body", "Not my comment")],
        )
        .await;
    assert_eq!(forbidden.status, 403);
    let updated = b
        .form_reply(
            &format!("{path}/comments/{comment}/edit"),
            &[("body", "Edited comment")],
        )
        .await;
    assert_eq!(updated.status, 303);
    assert_eq!(
        db::gists::comment(f.pool(), record.repo_id, comment)
            .await
            .unwrap()
            .unwrap()
            .body,
        "Edited comment"
    );
    let refused = b
        .form_reply(&format!("{path}/comments"), &[("body", "   ")])
        .await;
    assert_eq!(refused.status, 422);
    assert!(refused.text().contains("Write a comment"));
    let fork = b.form_reply(&format!("{path}/fork"), &[]).await;
    assert_eq!(fork.status, 303, "{}", fork.text());
    let fork_path = fork.location.unwrap();
    let fork_id = fork_path.strip_prefix("/gists/").unwrap();
    assert!(b.page_ok(&fork_path).await.contains("Forked from"));
    let removed = a
        .form_reply(&format!("{path}/comments/{comment}/delete"), &[])
        .await;
    assert_eq!(removed.status, 303);
    assert!(db::gists::comment(f.pool(), record.repo_id, comment)
        .await
        .unwrap()
        .is_none());
    let mut last = 0;
    for index in 0..31 {
        last = db::gists::create_comment(
            f.pool(),
            bob,
            &db::users::accounts::AccountCredential::Session(&b.session),
            record.repo_id,
            &format!("Page comment {index}"),
        )
        .await
        .unwrap()
        .id;
    }
    let page_two = format!("{path}?comments_page=2");
    let (_, model) = b.model(&page_two).await;
    let Page::Gists(gists::Page::Detail(second)) = model else {
        panic!("expected second comment page")
    };
    assert_eq!(second.comments_page, 2);
    assert_eq!(second.comments[0].id, last);
    assert!(second.comments_previous.is_some());
    let edit = format!("{path}/comments/{last}/edit?comments_page=2");
    let refused = b.form_reply(&edit, &[("body", "   ")]).await;
    assert_eq!(refused.status, 422);
    assert!(refused.text().contains(&format!("id=\"comment-{last}\"")));
    let saved = b.form_reply(&edit, &[("body", "Edited on page two")]).await;
    assert_eq!(saved.status, 303);
    assert_eq!(
        saved.location.as_deref(),
        Some(format!("{path}?comments_page=2#comment-{last}").as_str())
    );
    let deleted_comment = b
        .form_reply(
            &format!("{path}/comments/{last}/delete?comments_page=2"),
            &[],
        )
        .await;
    assert_eq!(deleted_comment.status, 303);
    assert_eq!(
        deleted_comment.location.as_deref(),
        Some(format!("{path}?comments_page=1").as_str())
    );
    let deleted = a.form_reply(&format!("{path}/delete"), &[]).await;
    assert_eq!(deleted.status, 303, "{}", deleted.text());
    assert_eq!(guest.page(&path).await.0, 404);
    let (_, model) = b.model(&fork_path).await;
    let Page::Gists(gists::Page::Detail(fork)) = model else {
        panic!("expected fork")
    };
    assert!(fork.parent.is_none());
    assert_eq!(fork.files[0].content.as_deref(), Some("# Revised\n"));
    assert!(db::gists::get(f.pool(), fork_id).await.unwrap().is_some());
    f.finish().await;
}
#[tokio::test]
async fn sqlite_gist_pages() {
    pages(false).await
}
#[tokio::test]
async fn postgres_gist_pages() {
    pages(true).await
}
