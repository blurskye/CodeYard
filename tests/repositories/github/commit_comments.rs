use super::protection::call;
use super::{db, json, request, Client, Router, Value};
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use codeyard::backend::db::models::AccessLevel;
use codeyard::backend::{
    db::repositories::commit_comments as records, repositories::access::change_collaborator,
};
use tower::ServiceExt;

async fn delivery(pool: &db::DbPool, hook: i64, comment: i64) -> Value {
    for _ in 0..250 {
        for row in db::webhooks::list_deliveries(pool, hook).await.unwrap() {
            let body: Value = serde_json::from_str(&row.request_body).unwrap();
            if row.event == "commit_comment" && body["comment"]["id"] == comment {
                return body;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("commit-comment delivery was not enqueued");
}

pub(super) async fn run(
    router: &Router,
    resource: (&db::DbPool, i64, i64, i64),
    base: &str,
    tokens: (&str, &str, &str),
    sha: &str,
    session: &Client,
) {
    let (pool, repo, alice, bob) = resource;
    let (writer, reader, other) = tokens;
    change_collaborator(pool, alice, repo, "bob", Some(AccessLevel::Read))
        .await
        .unwrap();
    let hook_owner = db::webhooks::HookOwner::Repository(repo);
    let hook = db::webhooks::insert_webhook(
        pool,
        hook_owner,
        alice,
        &db::users::accounts::AccountCredential::Session(&crate::support::live_session(pool, alice).await),
        &db::webhooks::WebhookFields {
            username: None,
            password: None,
            payload_url: "https://example.test/commit-comment-hook".into(),
            content_type: "application/json".into(),
            events: "commit_comment".into(),
            active: true,
            ssl_verify: true,
        },
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let list = format!("{base}/commits/{sha}/comments");
    let first = call(
        router,
        "POST",
        &list,
        writer,
        json!({"body":"Commit discussion @bob"}),
        ("repos/create-commit-comment", 201),
    )
    .await;
    assert_eq!(first["commit_id"], sha);
    assert_eq!(first["author_association"], "OWNER");
    assert_eq!(first["user"]["id"], alice);
    assert!(first["path"].is_null());
    let delivery = delivery(pool, hook, first["id"].as_i64().unwrap()).await;
    assert_eq!(delivery["action"], "created");
    assert_eq!(delivery["comment"], first);
    assert_eq!(delivery["sender"]["id"], alice);
    let id = first["id"].as_i64().unwrap();
    let item = format!("{base}/comments/{id}");
    let fetched = call(
        router,
        "GET",
        &item,
        reader,
        Value::Null,
        ("repos/get-commit-comment", 200),
    )
    .await;
    assert_eq!(fetched, first);
    let (status, _, body) = request(router, "HEAD", &item, Some(reader), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Null);
    let notices = db::notifications::records::list_notifications_for_user(pool, bob, 100, false)
        .await
        .unwrap();
    assert!(notices
        .iter()
        .any(|notice| notice.metadata["commit_comment_id"] == id));
    for (method, path, operation) in [
        ("POST", list.as_str(), "repos/create-commit-comment"),
        ("PATCH", item.as_str(), "repos/update-commit-comment"),
        ("DELETE", item.as_str(), "repos/delete-commit-comment"),
    ] {
        call(
            router,
            method,
            path,
            reader,
            json!({"body":"read token cannot write"}),
            (operation, 403),
        )
        .await;
    }
    let second = call(
        router,
        "POST",
        &list,
        other,
        json!({"body":"Read access can participate"}),
        ("repos/create-commit-comment", 201),
    )
    .await;
    let second_item = format!("{base}/comments/{}", second["id"]);
    call(
        router,
        "PATCH",
        &item,
        other,
        json!({"body":"not my comment"}),
        ("repos/update-commit-comment", 403),
    )
    .await;
    call(
        router,
        "PATCH",
        &second_item,
        writer,
        json!({"body":"maintainers cannot rewrite authors"}),
        ("repos/update-commit-comment", 403),
    )
    .await;
    let changed = call(
        router,
        "PATCH",
        &second_item,
        other,
        json!({"body":"Edited reader discussion"}),
        ("repos/update-commit-comment", 200),
    )
    .await;
    assert_eq!(changed["body"], "Edited reader discussion");
    assert_eq!(changed["node_id"], second["node_id"]);
    let commit = call(
        router,
        "GET",
        &format!("{base}/commits/{sha}"),
        reader,
        Value::Null,
        ("repos/get-commit", 200),
    )
    .await;
    assert_eq!(commit["commit"]["comment_count"], 2);
    assert_eq!(commit["author"]["login"], "alice");
    let branch = call(
        router,
        "GET",
        &format!("{base}/branches/main"),
        reader,
        Value::Null,
        ("repos/get-branch", 200),
    )
    .await;
    assert_eq!(branch["commit"]["commit"]["comment_count"], 2);
    let compared = call(
        router,
        "GET",
        &format!("{base}/compare/{sha}...{sha}"),
        reader,
        Value::Null,
        ("repos/compare-commits", 200),
    )
    .await;
    assert_eq!(compared["base_commit"]["commit"]["comment_count"], 2);
    let page = call(
        router,
        "GET",
        &format!("{list}?per_page=1"),
        reader,
        Value::Null,
        ("repos/list-comments-for-commit", 200),
    )
    .await;
    assert_eq!(page[0]["id"], id);
    let (_, headers, _) = request(
        router,
        "GET",
        &format!("{list}?per_page=1"),
        Some(reader),
        Value::Null,
    )
    .await;
    assert!(headers["link"].to_str().unwrap().contains("rel=\"next\""));
    let page = call(
        router,
        "GET",
        &format!("{base}/comments?per_page=1&page=2"),
        reader,
        Value::Null,
        ("repos/list-commit-comments-for-repo", 200),
    )
    .await;
    assert_eq!(page[0]["id"], second["id"]);
    for body in [
        json!({"body":""}),
        json!({"body":" \n\t"}),
        json!({"body":"\u{0000}"}),
        json!({"body":"x".repeat(65_537)}),
        json!({"body":"bad position","path":"docs/hello.txt","position":99999}),
    ] {
        call(
            router,
            "POST",
            &list,
            writer,
            body,
            ("repos/create-commit-comment", 422),
        )
        .await;
    }
    call(
        router,
        "PATCH",
        &item,
        writer,
        json!({"body":""}),
        ("repos/update-commit-comment", 422),
    )
    .await;
    let inline = call(
        router,
        "POST",
        &list,
        writer,
        json!({"body":"This diff line","path":"docs/hello.txt","position":2}),
        ("repos/create-commit-comment", 201),
    )
    .await;
    assert_eq!(inline["position"], 2);
    assert_eq!(inline["line"], 1);
    let large = call(
        router,
        "POST",
        &list,
        writer,
        json!({"body":"😀".repeat(65_536)}),
        ("repos/create-commit-comment", 201),
    )
    .await;
    assert_eq!(large["body"].as_str().unwrap().chars().count(), 65_536);
    call(
        router,
        "DELETE",
        &format!("{base}/comments/{}", large["id"]),
        writer,
        Value::Null,
        ("repos/delete-commit-comment", 204),
    )
    .await;
    call(
        router,
        "DELETE",
        &second_item,
        writer,
        Value::Null,
        ("repos/delete-commit-comment", 204),
    )
    .await;
    call(
        router,
        "GET",
        &second_item,
        reader,
        Value::Null,
        ("repos/get-commit-comment", 404),
    )
    .await;
    let native = format!(
        "/{}/commit/{sha}",
        base.strip_prefix("/api/v3/repos/").unwrap()
    );
    let (status, html) = session.page(&native).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Commit discussion @bob"));
    let textarea = html
        .split("<textarea")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    assert!(
        textarea
            .split_whitespace()
            .any(|attribute| matches!(attribute, "maxlength=131072" | "maxlength=\"131072\"")),
        "the editor accommodates all allowed Unicode scalars: {textarea}"
    );
    // Hydrated navigation negotiates the same authorized, owned presentation model.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&native)
                .header(
                    header::ACCEPT,
                    codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
                )
                .header(header::COOKIE, format!("session_id={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let model: codeyard::webui::app::PageEnvelope = serde_json::from_slice(&bytes).unwrap();
    let codeyard::webui::app::Page::Repositories(
        codeyard::webui::pages::repos::page::Page::CommitDetail { discussion, .. },
    ) = model.page
    else {
        panic!("commit page model")
    };
    assert!(discussion.can_comment);
    assert!(discussion
        .comments
        .iter()
        .any(|comment| comment.id == id && comment.body == "Commit discussion @bob"));
    assert!(!String::from_utf8_lossy(&bytes).contains("password_hash"));
    let before = records::count_comments(pool, repo, sha, None)
        .await
        .unwrap();
    let repo_id = repo.to_string();
    let (status, html) = session
        .form(
            &format!("{native}/comments"),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", sha),
                ("body", "**Preview marker**"),
                ("intent", "preview"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.split("<strong").skip(1).any(|node| node
            .split_once('>')
            .is_some_and(|(_, text)| text.starts_with("Preview marker</strong>"))),
        "the native preview must render Markdown: {html}"
    );
    assert_eq!(
        records::count_comments(pool, repo, sha, None)
            .await
            .unwrap(),
        before
    );
    let native_large = session
        .form_reply(
            &format!("{native}/comments"),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", sha),
                ("body", large["body"].as_str().unwrap()),
            ],
        )
        .await;
    assert_eq!(
        native_large.status,
        StatusCode::SEE_OTHER,
        "{}",
        native_large.text()
    );
    let large_id = native_large.id_after("#commitcomment-");
    assert_eq!(
        records::get_comment(pool, repo, large_id)
            .await
            .unwrap()
            .unwrap()
            .body
            .chars()
            .count(),
        65_536
    );
    call(
        router,
        "DELETE",
        &format!("{base}/comments/{large_id}"),
        writer,
        Value::Null,
        ("repos/delete-commit-comment", 204),
    )
    .await;
    let created = session
        .form_reply(
            &format!("{native}/comments"),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", sha),
                ("body", "native body"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.text());
    let native_id = created.id_after("#commitcomment-");
    let revision = db::discussions::comment_revision("native body");
    let native_item = format!("{base}/comments/{native_id}");
    call(
        router,
        "PATCH",
        &native_item,
        writer,
        json!({"body":"newer body"}),
        ("repos/update-commit-comment", 200),
    )
    .await;
    let (status, html) = session
        .form(
            &format!("{native}/comments/{native_id}/edit"),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", sha),
                ("body", "draft marker <script>alert(9)</script>"),
                ("expected_revision", &revision),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert!(html.contains("draft marker"));
    assert!(!html.contains("<script>alert(9)</script>"));
    assert_eq!(
        records::get_comment(pool, repo, native_id)
            .await
            .unwrap()
            .unwrap()
            .body,
        "newer body"
    );
    let csrf = session
        .exchange(
            "POST",
            &format!("{native}/comments"),
            "application/x-www-form-urlencoded",
            b"body=no-csrf".to_vec(),
        )
        .await;
    assert_eq!(csrf.status, StatusCode::FORBIDDEN);
    let (status, html) = session
        .page(
            url::Url::parse(first["html_url"].as_str().unwrap())
                .unwrap()
                .path(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Commit discussion @bob"));
    let missing_sha = "f".repeat(40);
    let missing = session
        .form_reply(
            &format!(
                "{}/commit/{missing_sha}/comments",
                native.split("/commit/").next().unwrap()
            ),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", &missing_sha),
                ("body", "missing parent draft"),
            ],
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.text());
    assert!(missing.text().contains("missing parent draft"));
    let fresh = db::discussions::comment_revision("newer body");
    let saved = session
        .form_reply(
            &format!("{native}/comments/{native_id}/edit"),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", sha),
                ("body", "Saved natively"),
                ("expected_revision", &fresh),
            ],
        )
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.text());
    let stale_delete = session
        .form_reply(
            &format!("{native}/comments/{native_id}/delete"),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", sha),
                ("expected_revision", &fresh),
            ],
        )
        .await;
    assert_eq!(
        stale_delete.status,
        StatusCode::CONFLICT,
        "{}",
        stale_delete.text()
    );
    let current = db::discussions::comment_revision("Saved natively");
    let deleted = session
        .form_reply(
            &format!("{native}/comments/{native_id}/delete"),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", sha),
                ("expected_revision", &current),
            ],
        )
        .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER, "{}", deleted.text());
    assert!(records::get_comment(pool, repo, native_id)
        .await
        .unwrap()
        .is_none());
    let mismatch = session
        .form_reply(
            &format!("{native}/comments"),
            &[
                ("repository_id", "999999"),
                ("commit_sha", sha),
                ("body", "target mismatch draft"),
            ],
        )
        .await;
    assert_eq!(mismatch.status, StatusCode::CONFLICT, "{}", mismatch.text());
    assert!(mismatch.text().contains("target mismatch draft"));
    let mismatch_html = mismatch.text();
    let composer = mismatch_html
        .split(&format!("action=\"{native}/comments\""))
        .nth(1)
        .unwrap();
    assert_eq!(
        crate::support::http_client::field_value(composer, "repository_id").as_deref(),
        Some("999999"),
        "a rejected draft must not silently adopt a different target"
    );
    assert!(
        db::repositories::records::set_repository_archived(pool, alice, repo, true)
            .await
            .unwrap()
    );
    call(
        router,
        "POST",
        &list,
        writer,
        json!({"body":"archived discussion"}),
        ("repos/create-commit-comment", 403),
    )
    .await;
    let count = records::count_comments(pool, repo, sha, None)
        .await
        .unwrap();
    let archived = session
        .form_reply(
            &format!("{native}/comments"),
            &[
                ("repository_id", &repo_id),
                ("commit_sha", sha),
                ("body", "archived draft marker"),
            ],
        )
        .await;
    assert_eq!(
        archived.status,
        StatusCode::FORBIDDEN,
        "{}",
        archived.text()
    );
    let archived_html = archived.text();
    let textarea = archived_html
        .split("<textarea")
        .nth(1)
        .unwrap()
        .split("</textarea>")
        .next()
        .unwrap();
    assert!(
        textarea.contains("archived draft marker"),
        "a refused archived-repository draft remains visible: {archived_html}"
    );
    assert_eq!(
        records::count_comments(pool, repo, sha, None)
            .await
            .unwrap(),
        count
    );
    assert!(
        db::repositories::records::set_repository_archived(pool, alice, repo, false)
            .await
            .unwrap()
    );
    let mut last = None;
    for number in 0..25 {
        last = Some(
            records::create_comment(
                pool,
                records::CommentCreate {
                    actor: alice.into(),
                    repo_id: repo,
                    sha,
                    body: &format!("page-marker-{number}"),
                    path: None,
                    position: None,
                    line: None,
                },
            )
            .await
            .unwrap(),
        );
    }
    let last = last.unwrap();
    let fetched = call(
        router,
        "GET",
        &format!("{base}/comments/{}", last.id),
        reader,
        Value::Null,
        ("repos/get-commit-comment", 200),
    )
    .await;
    let permalink = url::Url::parse(fetched["html_url"].as_str().unwrap()).unwrap();
    let path = format!("{}?{}", permalink.path(), permalink.query().unwrap());
    let (status, html) = session.page(&path).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("page-marker-24"));
    assert!(
        html.contains("comments_page=1"),
        "permalink selects the actual comment page: {html}"
    );
    // Notification cleanup applies to commit-author notices as well as mentions.
    let notices = db::notifications::records::list_notifications_for_user(pool, alice, 100, false)
        .await
        .unwrap();
    assert!(!notices
        .iter()
        .any(|notice| notice.metadata["commit_comment_id"] == second["id"]));
    assert!(db::webhooks::delete_webhook(pool, hook_owner, alice, &db::users::accounts::AccountCredential::Session(&crate::support::live_session(pool, alice).await), hook)
        .await
        .unwrap()
        .unwrap());
    change_collaborator(pool, alice, repo, "bob", None)
        .await
        .unwrap();
    let refused = records::create_comment(
        pool,
        records::CommentCreate {
            actor: bob.into(),
            repo_id: repo,
            sha,
            body: "revoked read access",
            path: None,
            position: None,
            line: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        refused.downcast_ref::<records::CommentRefusal>(),
        Some(records::CommentRefusal::Denied)
    ));
    // The owner still has access, but the session that sent the comment has ended.
    let ended = crate::support::live_session(pool, alice).await;
    db::users::sessions::delete_session(pool, &ended).await.unwrap();
    let refused = records::create_comment(
        pool,
        records::CommentCreate {
            actor: (alice, ended).into(),
            repo_id: repo,
            sha,
            body: "ended session",
            path: None,
            position: None,
            line: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        refused.downcast_ref::<records::CommentRefusal>(),
        Some(records::CommentRefusal::Denied)
    ));
    call(
        router,
        "GET",
        &item,
        other,
        Value::Null,
        ("repos/get-commit-comment", 404),
    )
    .await;
}
