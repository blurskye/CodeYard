//! Editor saves through the JSON API and the HTML form, on SQLite and PostgreSQL.
//! Long account and branch names also go through pull requests and merges.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::http_client as http;
use crate::support::native_database as native;
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use base64::Engine;
use codeyard::{
    backend::db,
    webui::{
        app::{Page, PageEnvelope},
        pages::repos as view,
    },
};
use database::Database;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn request(
    router: &Router,
    session: &str,
    csrf: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: String,
) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::COOKIE, format!("session_id={session}"))
                .header("x-csrf-token", csrf)
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, private"
    );
    (
        status,
        String::from_utf8(
            to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap(),
    )
}
async fn model(client: &http::Client, path: &str) -> (StatusCode, Page) {
    let response = client
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::COOKIE, format!("session_id={}", client.session))
                .header(
                    header::ACCEPT,
                    codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        codeyard::webui::bootstrap::MODEL_CONTENT_TYPE
    );
    let envelope: PageEnvelope = serde_json::from_slice(
        &to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    (status, envelope.page)
}

async fn api_file(
    client: &http::Client,
    file: &str,
    bytes: &[u8],
    revision: &str,
    creating: bool,
) -> String {
    let response = client.expect(if creating { "POST" } else { "PUT" },
        &format!("/api/repos/editor_boundary/save/contents/{file}"),
        json!({"branch":"main", "content":base64::engine::general_purpose::STANDARD.encode(bytes), "message":"encoding fixture", "expected_revision":revision}),
        if creating { StatusCode::CREATED } else { StatusCode::OK }).await;
    if creating {
        response["sha"].as_str().unwrap().to_owned()
    } else {
        response["commit"]["sha"].as_str().unwrap().to_owned()
    }
}

fn blob_bytes(repo: &git2::Repository, path: &str) -> Vec<u8> {
    let tree = repo
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .tree()
        .unwrap();
    let entry = tree.get_path(std::path::Path::new(path)).unwrap();
    repo.find_blob(entry.id()).unwrap().content().to_vec()
}

async fn contract(pg: bool) {
    // The HTML form answers below render full pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("editor_boundary").await;
    let session = native::token();
    let csrf = native::token();
    db::users::sessions::create_session(
        f.pool(),
        &session,
        actor,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(
        db::users::sessions::get_or_create_csrf_token(f.pool(), &session, Some(actor), &csrf, 3600)
            .await
            .unwrap(),
        Some(csrf.clone())
    );
    let root = tempfile::tempdir().unwrap();
    let repository = codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "save",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let repository_id = repository.id.to_string();
    let path = std::path::PathBuf::from(repository.disk_path);
    let router = f.router(root.path()).await;
    let outsider_id = f.actor("editor_outsider").await;
    let outsider = http::Client::login(&router, f.pool(), outsider_id).await;
    for method in ["POST", "PUT"] {
        let revision = if method == "POST" {
            "unborn".to_owned()
        } else {
            "a".repeat(40)
        };
        // An unreadable repository answers like a missing one, whatever the body says.
        for input in [
            json!({"branch":"main", "content":"YQ==", "message":"probe", "expected_revision": revision}),
            json!({"branch":"main", "content":"YQ==", "message":"", "expected_revision": revision}),
            json!({"branch":"main", "content":"!!!", "message":"probe", "expected_revision": revision}),
        ] {
            let missing = outsider
                .expect(
                    method,
                    "/api/repos/editor_boundary/missing/contents/probe.txt",
                    input.clone(),
                    StatusCode::NOT_FOUND,
                )
                .await;
            let private = outsider
                .expect(
                    method,
                    "/api/repos/editor_boundary/save/contents/probe.txt",
                    input,
                    StatusCode::NOT_FOUND,
                )
                .await;
            assert_eq!(private, missing);
        }
    }
    // The owner can read the repository, so the same malformed bodies are validation errors.
    for input in [
        json!({"branch":"main", "content":"YQ==", "message":"bad\u{0}message", "expected_revision":"unborn"}),
        json!({"branch":"main", "content":"!!!", "message":"probe", "expected_revision":"unborn"}),
    ] {
        let (status, reply) = request(
            &router,
            &session,
            &csrf,
            "POST",
            "/api/repos/editor_boundary/save/contents/probe.txt",
            "application/json",
            input.to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
    }
    db::repositories::access::grant_repo_access(
        f.pool(),
        outsider_id,
        repository.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    for method in ["POST", "PUT"] {
        outsider.expect(method, "/api/repos/editor_boundary/save/contents/probe.txt",
            json!({"branch":"main", "content":"YQ==", "message":"probe", "expected_revision":"a".repeat(40)}), StatusCode::FORBIDDEN).await;
    }
    db::repositories::access::revoke_repo_access(f.pool(), outsider_id, repository.id)
        .await
        .unwrap();
    db::repositories::records::update_repository(
        f.pool(),
        actor,
        repository.id,
        None,
        false,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await
    .unwrap();
    for method in ["POST", "PUT"] {
        outsider.expect(method, "/api/repos/editor_boundary/save/contents/probe.txt",
            json!({"branch":"main", "content":"YQ==", "message":"probe", "expected_revision":"a".repeat(40)}), StatusCode::FORBIDDEN).await;
    }
    db::repositories::records::update_repository(
        f.pool(),
        actor,
        repository.id,
        None,
        true,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        git2::Repository::open_bare(&path)
            .unwrap()
            .references()
            .unwrap()
            .count(),
        0
    );
    let endpoint = "/api/repos/editor_boundary/save/contents/file.bin";
    let (status, body) = request(&router, &session, &csrf, "POST", endpoint, "application/json",
        json!({"branch":"main", "content":"AP8=", "message":"binary first", "expected_revision":"unborn"}).to_string()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let first: Value = serde_json::from_str(&body).unwrap();
    let first = first["sha"].as_str().unwrap();
    let (status, body) = request(&router, &session, &csrf, "PUT", endpoint, "application/json",
        json!({"branch":"main", "content":"AP8B", "message":"binary second", "expected_revision":first}).to_string()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let saved: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        saved["commit"]["message"].as_str().unwrap().trim_end(),
        "binary second"
    );
    assert_eq!(saved["commit"]["author_name"], "editor_boundary");
    assert_eq!(saved["commit"]["author"]["id"], actor);
    assert_eq!(saved["commit"]["parents"], json!([first]));
    let second = saved["commit"]["sha"].as_str().unwrap();
    for endpoint in [
        "/api/repos/editor_boundary/save/commits".to_string(),
        format!("/api/repos/editor_boundary/save/commits/{second}"),
    ] {
        let (status, body) = request(
            &router,
            &session,
            &csrf,
            "GET",
            &endpoint,
            "application/json",
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{endpoint}: {body}");
        let response: Value = serde_json::from_str(&body).unwrap();
        let commit = if endpoint.ends_with("/commits") {
            &response[0]
        } else {
            &response["commit"]
        };
        for field in [
            "sha",
            "message",
            "author_name",
            "author_email",
            "author_date",
            "committer_name",
            "committer_email",
            "committer_date",
            "parents",
        ] {
            assert_eq!(commit[field], saved["commit"][field], "{endpoint}: {field}");
        }
        assert!(
            commit.get("author").is_none(),
            "unlinked Git author must not gain an identity"
        );
    }
    assert!(
        chrono::DateTime::parse_from_rfc3339(saved["commit"]["author_date"].as_str().unwrap())
            .is_ok()
    );
    let repo = git2::Repository::open_bare(&path).unwrap();
    let commit = repo
        .find_commit(git2::Oid::from_str(second).unwrap())
        .unwrap();
    let entry = commit
        .tree()
        .unwrap()
        .get_path(std::path::Path::new("file.bin"))
        .unwrap();
    assert_eq!(repo.find_blob(entry.id()).unwrap().content(), &[0, 255, 1]);
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf_token", &csrf)
        .append_pair("branch", "main")
        .append_pair("path", "file.bin")
        .append_pair("intent", "save")
        .append_pair("repository_id", &repository_id)
        .append_pair("message", "stale draft")
        .append_pair("content", "preserved-editor-draft")
        .append_pair("expected_revision", first)
        .finish();
    let (status, body) = request(
        &router,
        &session,
        &csrf,
        "POST",
        "/editor_boundary/save/_forms/write-file",
        "application/x-www-form-urlencoded",
        form,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("preserved-editor-draft"));
    assert!(body.contains("The branch changed"));
    assert_eq!(
        repo.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        second
    );
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf_token", &csrf)
        .append_pair("branch", "main")
        .append_pair("path", "native.txt")
        .append_pair("intent", "save")
        .append_pair("repository_id", &repository_id)
        .append_pair("message", "native save")
        .append_pair("content", "native-editor-text")
        .append_pair("creating", "yes")
        .append_pair("expected_revision", second)
        .finish();
    let (status, body) = request(
        &router,
        &session,
        &csrf,
        "POST",
        "/editor_boundary/save/_forms/write-file",
        "application/x-www-form-urlencoded",
        form,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_ne!(
        repo.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        second
    );
    let client = http::Client {
        router: router.clone(),
        session: session.clone(),
        csrf: csrf.clone(),
    };
    let mut tip = repo.head().unwrap().target().unwrap().to_string();
    // Invalid UTF-8 without NUL must not slip past Git's binary sniffing.
    let latin1 = b"caf\xe9\n";
    tip = api_file(&client, "latin1.txt", latin1, &tip, true).await;
    assert_eq!(blob_bytes(&repo, "latin1.txt"), latin1);
    let (status, body) = client
        .page("/editor_boundary/save/edit/main/latin1.txt")
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("UTF-8 text only"));
    let (status, page) = model(&client, "/editor_boundary/save/edit/main/latin1.txt").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(matches!(page, Page::Error { status: 422, .. }));
    let (status, page) = model(&client, "/editor_boundary/save/blob/main/latin1.txt").await;
    assert_eq!(status, StatusCode::OK);
    let Page::Repositories(view::page::Page::Blob { page, .. }) = page else {
        panic!("expected file model")
    };
    assert!(
        !page.binary,
        "encoding rejection must not depend on NUL detection"
    );
    assert!(!page.editable);
    let draft = "not a replacement-character rewrite";
    for intent in ["preview", "save"] {
        let (status, body) = client
            .form(
                "/editor_boundary/save/_forms/write-file",
                &[
                    ("branch", "main"),
                    ("repository_id", &repository_id),
                    ("path", "latin1.txt"),
                    ("intent", intent),
                    ("message", "native draft"),
                    ("content", draft),
                    ("expected_revision", &tip),
                ],
            )
            .await;
        assert_eq!(
            status,
            if intent == "preview" {
                StatusCode::OK
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            },
            "{body}"
        );
        assert!(body.contains(draft));
        // The appearance switch returns to the editor's GET page, not to this POST-only form.
        let return_to = "/editor_boundary/save/edit/refs/heads/main/latin1.txt";
        assert!(body.contains(&format!("name=\"return_to\" value=\"{return_to}\"")));
        assert!(body.contains(&format!("name=\"expected_revision\" value=\"{tip}\"")));
        assert_eq!(blob_bytes(&repo, "latin1.txt"), latin1);
        assert_eq!(repo.head().unwrap().target().unwrap().to_string(), tip);
    }
    // Raw API updates remain byte-exact even when text editing is forbidden.
    let raw_update = b"caf\xe9\r\n";
    tip = api_file(&client, "latin1.txt", raw_update, &tip, false).await;
    assert_eq!(blob_bytes(&repo, "latin1.txt"), raw_update);

    // A text input would strip these line breaks and submit another file's name.
    tip = api_file(&client, "docs/ab.txt", b"keep me", &tip, true).await;
    for (encoded, path) in [
        ("docs/a%0Ab.txt", "docs/a\nb.txt"),
        ("docs/a%0Db.txt", "docs/a\rb.txt"),
    ] {
        tip = api_file(&client, encoded, b"other file", &tip, true).await;
        let (status, body) = client
            .page(&format!("/editor_boundary/save/edit/main/{encoded}"))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("Filenames containing line breaks cannot be edited"),
            "{body}"
        );
        assert!(!body.contains("id=\"repo-editor-form\""));
        assert_eq!(blob_bytes(&repo, path), b"other file");
        assert_eq!(blob_bytes(&repo, "docs/ab.txt"), b"keep me");
    }

    tip = api_file(&client, "script.sh", b"#!/bin/sh\necho old\n", &tip, true).await;
    let (status, page) = model(&client, "/editor_boundary/save/edit/main/script.sh").await;
    assert_eq!(status, StatusCode::OK);
    let Page::Repositories(view::page::Page::Editor { editor, .. }) = page else {
        panic!("expected editor model")
    };
    assert_eq!(editor.content, "#!/bin/sh\necho old\n");
    assert_eq!(editor.expected_revision, tip);
    let (status, body) = client
        .form(
            "/editor_boundary/save/_forms/write-file",
            &[
                ("branch", "main"),
                ("path", "script.sh"),
                ("intent", "save"),
                ("repository_id", &repository_id),
                ("message", "LF save"),
                ("content", "#!/bin/sh\r\necho edited\r\n"),
                ("expected_revision", &tip),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(blob_bytes(&repo, "script.sh"), b"#!/bin/sh\necho edited\n");
    let stale = tip.clone();
    tip = repo.head().unwrap().target().unwrap().to_string();
    let (status, body) = client
        .form(
            "/editor_boundary/save/_forms/write-file",
            &[
                ("branch", "main"),
                ("path", "script.sh"),
                ("intent", "save"),
                ("repository_id", &repository_id),
                ("message", "stale LF draft"),
                ("content", "#!/bin/sh\r\necho unsaved\r\n"),
                ("expected_revision", &stale),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("echo unsaved"));
    assert!(body.contains(&format!("name=\"expected_revision\" value=\"{stale}\"")));
    assert_eq!(repo.head().unwrap().target().unwrap().to_string(), tip);
    assert_eq!(blob_bytes(&repo, "script.sh"), b"#!/bin/sh\necho edited\n");

    tip = api_file(&client, "windows.txt", b"one\r\ntwo\r\n", &tip, true).await;
    for submission in ["one\nchanged\n", "one\r\nchanged\r\n"] {
        let (status, body) = client
            .form(
                "/editor_boundary/save/_forms/write-file",
                &[
                    ("branch", "main"),
                    ("path", "windows.txt"),
                    ("intent", "save"),
                    ("repository_id", &repository_id),
                    ("message", "CRLF save"),
                    ("content", submission),
                    ("expected_revision", &tip),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(blob_bytes(&repo, "windows.txt"), b"one\r\nchanged\r\n");
        tip = repo.head().unwrap().target().unwrap().to_string();
    }
    let (status, body) = client
        .form(
            "/editor_boundary/save/_forms/write-file",
            &[
                ("branch", "main"),
                ("path", "new-script.sh"),
                ("intent", "save"),
                ("creating", "yes"),
                ("repository_id", &repository_id),
                ("message", "native creation"),
                ("content", "#!/bin/sh\r\necho new\r\n"),
                ("expected_revision", &tip),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(blob_bytes(&repo, "new-script.sh"), b"#!/bin/sh\necho new\n");
    let tip = repo.head().unwrap().target().unwrap().to_string();
    // A blank message takes the placeholder the editor shows, for a create and for an edit.
    let created_endpoint = "/api/repos/editor_boundary/save/contents/placeholder.txt";
    let (status, body) = request(
        &router,
        &session,
        &csrf,
        "POST",
        created_endpoint,
        "application/json",
        json!({"branch":"main", "content":"YQ==", "message":"", "expected_revision":tip})
            .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        created["message"].as_str().unwrap().trim_end(),
        "Create file"
    );
    let created_tip = created["sha"].as_str().unwrap().to_owned();
    let (status, body) = request(
        &router,
        &session,
        &csrf,
        "PUT",
        created_endpoint,
        "application/json",
        json!({"branch":"main", "content":"YQ==", "message":"  ", "expected_revision":created_tip})
            .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let blank: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        blank["commit"]["message"].as_str().unwrap().trim_end(),
        "Update placeholder.txt"
    );
    let blank_tip = blank["commit"]["sha"].as_str().unwrap().to_owned();
    // A refused message says what is wrong with it.
    let (status, body) = request(
        &router,
        &session,
        &csrf,
        "POST",
        created_endpoint,
        "application/json",
        json!({"branch":"main", "content":"YQ==", "message":"x".repeat(4097), "expected_revision":blank_tip})
            .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("A commit message can be at most 4096 bytes."),
        "{body}"
    );
    drop(commit);
    drop(repo);
    f.finish().await;
}
#[tokio::test]
async fn sqlite_editor_api_and_form_saves() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_editor_api_and_form_saves() {
    contract(true).await;
}

/// Posts a save from the editor of a replaced repository, with the repository ID it carries.
async fn stale_save(
    client: &http::Client,
    owner: &str,
    repository_id: &str,
    expected_revision: &str,
) -> (StatusCode, String) {
    client
        .form(
            &format!("/{owner}/save/_forms/write-file"),
            &[
                ("repository_id", repository_id),
                ("branch", "main"),
                ("path", "unsaved.txt"),
                ("intent", "save"),
                ("creating", "yes"),
                ("message", "stale repository"),
                ("content", "must not reach replacement"),
                ("expected_revision", expected_revision),
            ],
        )
        .await
}

/// The repository ID that a returned editor page sends back with its next save.
fn hidden_repository_id(page: &str) -> String {
    page.split("name=\"repository_id\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the editor page keeps its repository ID")
        .to_owned()
}

async fn stale_repository_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    for (owner, populated) in [("unborn_editor", false), ("populated_editor", true)] {
        let actor = f.actor(owner).await;
        let (client, root) = long_name_client(&f, actor).await;
        if !populated {
            let (status, page) = model(
                &client,
                &format!("/{owner}/save/new/refs/heads/main/.github/workflows"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let Page::Repositories(view::page::Page::Editor { editor, .. }) = page else {
                panic!("workflow editor")
            };
            assert_eq!(editor.branch, "main");
            assert_eq!(editor.path, ".github/workflows/");
        }
        if populated {
            save_file(
                &client,
                &long_name_repo(owner),
                "main",
                "draft.txt",
                "first",
                "unborn",
                true,
            )
            .await;
        }
        let repository = db::repositories::records::get_repository(f.pool(), owner, "save")
            .await
            .unwrap()
            .unwrap();
        let (status, page) = client.page(&format!("/{owner}/save/new/main")).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(page.contains(&format!(
            "name=\"repository_id\" value=\"{}\"",
            repository.id
        )));
        let (status, page) = model(&client, &format!("/{owner}/save/new/main")).await;
        assert_eq!(status, StatusCode::OK);
        let Page::Repositories(view::page::Page::Editor { editor, .. }) = page else {
            panic!("expected editor")
        };
        f.execute(
            "UPDATE repositories SET name='renamed' WHERE id=$1",
            &[repository.id],
        )
        .await;
        let replacement = codeyard::backend::repositories::lifecycle::provision(
            (f.pool(), None),
            root.path(),
            db::repositories::creation::RepositoryCreationOwner::personal(actor),
            "save",
            None,
            true,
            None,
        )
        .await
        .unwrap();
        let git = git2::Repository::open_bare(&replacement.disk_path).unwrap();
        if populated {
            git.remote_anonymous(&repository.disk_path)
                .unwrap()
                .fetch(&["refs/heads/main:refs/heads/main"], None, None)
                .unwrap();
        }
        for id in [repository.id.to_string(), String::new()] {
            let (status, body) = stale_save(&client, owner, &id, &editor.expected_revision).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert!(body.contains("must not reach replacement"), "{body}");
            assert!(body.contains("nothing was saved"), "{body}");
            // The editor keeps the repository ID the form was rendered for. An empty ID
            // comes back as 0, which no repository has, so the next save is refused too.
            let kept = hidden_repository_id(&body);
            let expected = if id.is_empty() {
                "0".to_owned()
            } else {
                id.clone()
            };
            assert_eq!(kept, expected, "{body}");
            let (status, again) =
                stale_save(&client, owner, &kept, &editor.expected_revision).await;
            assert_eq!(status, StatusCode::CONFLICT, "{again}");
            assert!(again.contains("must not reach replacement"), "{again}");
        }
        if populated {
            assert_eq!(
                git.head().unwrap().target().unwrap().to_string(),
                editor.expected_revision
            );
        } else {
            assert!(git.find_reference("refs/heads/main").is_err());
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_stale_editor_cannot_write_replacement_repository() {
    stale_repository_contract(false).await;
}

#[tokio::test]
async fn postgres_stale_editor_cannot_write_replacement_repository() {
    stale_repository_contract(true).await;
}

/// A signed-in client for `owner`'s empty repository `save`, behind a fresh router.
async fn long_name_client(f: &Database, owner: i64) -> (http::Client, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    codeyard::backend::repositories::lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "save",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    (http::Client::login(&router, f.pool(), owner).await, root)
}

/// The API path of repository `save` owned by `owner`. Account names may contain
/// non-ASCII characters, so the owner is percent-encoded.
fn long_name_repo(owner: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(owner.as_bytes()).collect();
    format!("/api/repos/{encoded}/save")
}

/// Creates or updates `file` on `branch` through the editor API and returns the new tip.
async fn save_file(
    client: &http::Client,
    repo: &str,
    branch: &str,
    file: &str,
    message: &str,
    expected: &str,
    creating: bool,
) -> String {
    let body = json!({
        "branch": branch,
        "content": base64::engine::general_purpose::STANDARD.encode(format!("{file}\n")),
        "message": message,
        "expected_revision": expected,
    });
    let path = format!("{repo}/contents/{file}");
    if creating {
        let saved = client
            .expect("POST", &path, body, StatusCode::CREATED)
            .await;
        saved["sha"].as_str().unwrap().to_owned()
    } else {
        let saved = client.expect("PUT", &path, body, StatusCode::OK).await;
        saved["commit"]["sha"].as_str().unwrap().to_owned()
    }
}

/// Merges pull request `number` with the revisions it records now, as the web client does.
async fn merge_pull(client: &http::Client, repo: &str, number: i64) -> Value {
    let pull = client
        .expect(
            "GET",
            &format!("{repo}/pulls/{number}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    client
        .expect(
            "POST",
            &format!("{repo}/pulls/{number}/merge"),
            json!({"expected_head": pull["head_sha"], "expected_base": pull["base_sha"]}),
            StatusCode::OK,
        )
        .await
}

/// A 128-character account (256 bytes) saves and merges. A 257-byte branch is created,
/// edited and merged. A 256-character branch cannot back a pull request.
async fn long_account_and_branch_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    // 128 two-byte characters: 256 bytes, within the 255-character account bound.
    let name = "é".repeat(128);
    let owner = f.actor(&name).await;
    let (client, _root) = long_name_client(&f, owner).await;
    let repo = long_name_repo(&name);
    let main0 = save_file(&client, &repo, "main", "README.md", "first", "unborn", true).await;
    // 64 + 1 + 64 two-byte characters: 257 bytes.
    let branch = format!("{}/{}", "é".repeat(64), "é".repeat(64));
    client
        .expect(
            "POST",
            &format!("{repo}/branches"),
            json!({"name": branch, "sha": main0}),
            StatusCode::OK,
        )
        .await;
    let topic_tip = save_file(&client, &repo, &branch, "README.md", "edit", &main0, false).await;
    let main_tip = save_file(&client, &repo, "main", "main.txt", "main", &main0, true).await;
    let pull = client
        .expect(
            "POST",
            &format!("{repo}/pulls"),
            json!({"title": "Long names", "head_branch": branch, "base_branch": "main"}),
            StatusCode::CREATED,
        )
        .await;
    let merged = merge_pull(&client, &repo, pull["number"].as_i64().unwrap()).await;
    let merge_sha = merged["sha"].as_str().unwrap();
    let merge = client
        .expect(
            "GET",
            &format!("{repo}/commits/{merge_sha}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(merge["commit"]["author_name"], name.as_str());
    assert_eq!(merge["commit"]["parents"], json!([main_tip, topic_tip]));

    // 256 characters is a valid branch name, but too long for a pull request row. Each
    // path component stays under the 255-byte file name limit that git's ref files need.
    let long = format!("{}/{}", "a".repeat(128), "a".repeat(127));
    client
        .expect(
            "POST",
            &format!("{repo}/branches"),
            json!({"name": long, "sha": merge_sha}),
            StatusCode::OK,
        )
        .await;
    let (status, body) = client
        .send(
            "POST",
            &format!("{repo}/pulls"),
            "application/json",
            json!({"title": "Too long", "head_branch": long, "base_branch": "main"}).to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    f.finish().await;
}

/// The longest account name (255 characters, 1020 bytes, no email) saves with a 4 KiB
/// message, merges, and its commits are listed and read back with the name.
async fn longest_account_name_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    // 255 four-byte characters: 1020 bytes.
    let name = "𠀀".repeat(255);
    let owner = f.actor(&name).await;
    let (client, _root) = long_name_client(&f, owner).await;
    let repo = long_name_repo(&name);
    let message = "m".repeat(4096);
    let main0 = save_file(
        &client,
        &repo,
        "main",
        "README.md",
        &message,
        "unborn",
        true,
    )
    .await;
    client
        .expect(
            "POST",
            &format!("{repo}/branches"),
            json!({"name": "topic", "sha": main0}),
            StatusCode::OK,
        )
        .await;
    save_file(&client, &repo, "topic", "topic.txt", "topic", &main0, true).await;
    save_file(&client, &repo, "main", "main.txt", "main", &main0, true).await;
    let pull = client
        .expect(
            "POST",
            &format!("{repo}/pulls"),
            json!({"title": "Topic", "head_branch": "topic", "base_branch": "main"}),
            StatusCode::CREATED,
        )
        .await;
    let merged = merge_pull(&client, &repo, pull["number"].as_i64().unwrap()).await;
    let listed = client
        .expect(
            "GET",
            &format!("{repo}/commits"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed[0]["sha"], merged["sha"]);
    assert_eq!(listed[0]["author_name"], name.as_str());
    assert_eq!(
        listed[0]["author_email"],
        format!("{name}@users.noreply").as_str()
    );
    let first = client
        .expect(
            "GET",
            &format!("{repo}/commits/{main0}"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(first["commit"]["author_name"], name.as_str());
    f.finish().await;
}

async fn default_branch_contract(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("default_owner").await;
    let (client, _root) = long_name_client(&f, actor).await;
    let api = "/api/repos/default_owner/save";
    let tip = save_file(&client, api, "topic", "README.md", "first", "unborn", true).await;
    let repository = db::repositories::records::get_repository(f.pool(), "default_owner", "save")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repository.default_branch, "topic");
    let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
    assert_eq!(bare.head().unwrap().name(), Some("refs/heads/topic"));
    let branches = client
        .expect(
            "GET",
            &format!("{api}/branches"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(branches, json!([{"name": "topic", "is_default": true}]));
    client
        .expect(
            "DELETE",
            &format!("{api}/branches/topic"),
            Value::Null,
            StatusCode::BAD_REQUEST,
        )
        .await;

    // Git HEAD and the stored default both name main, even when no main branch exists.
    bare.set_head("refs/heads/main").unwrap();
    f.execute(
        "UPDATE repositories SET default_branch = 'main' WHERE id = $1",
        &[repository.id],
    )
    .await;
    let branches = client
        .expect(
            "GET",
            &format!("{api}/branches"),
            Value::Null,
            StatusCode::OK,
        )
        .await;
    assert_eq!(branches[0]["name"], "topic");
    assert_eq!(branches[0]["is_default"], true);
    client
        .expect(
            "DELETE",
            &format!("{api}/branches/topic"),
            Value::Null,
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(
        bare.find_reference("refs/heads/topic")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        tip
    );

    // A later write repairs an existing mismatch without changing the advertised default.
    let tip = save_file(&client, api, "topic", "README.md", "repair", &tip, false).await;
    let stored = db::repositories::records::get_repository_by_id(f.pool(), repository.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.default_branch, "topic");
    assert_eq!(bare.head().unwrap().name(), Some("refs/heads/topic"));

    // Branch creation from a tag-only repository is another first-branch write.
    let oid = git2::Oid::from_str(&tip).unwrap();
    bare.reference("refs/tags/keep", oid, false, "fixture")
        .unwrap();
    bare.find_reference("refs/heads/topic")
        .unwrap()
        .delete()
        .unwrap();
    bare.set_head("refs/heads/main").unwrap();
    f.execute(
        "UPDATE repositories SET default_branch = 'main' WHERE id = $1",
        &[repository.id],
    )
    .await;
    client
        .expect(
            "POST",
            &format!("{api}/branches"),
            json!({"name": "trunk", "sha": tip}),
            StatusCode::OK,
        )
        .await;
    let stored = db::repositories::records::get_repository_by_id(f.pool(), repository.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.default_branch, "trunk");
    assert_eq!(bare.head().unwrap().name(), Some("refs/heads/trunk"));
    client
        .expect(
            "DELETE",
            &format!("{api}/branches/trunk"),
            Value::Null,
            StatusCode::BAD_REQUEST,
        )
        .await;

    // Repair must remain possible beyond the ref read limit, even with an unborn HEAD.
    bare.reference("refs/heads/z-spare", oid, false, "fixture")
        .unwrap();
    bare.set_head("refs/heads/main").unwrap();
    f.execute(
        "UPDATE repositories SET default_branch = 'main' WHERE id = $1",
        &[repository.id],
    )
    .await;
    for index in 0..10_257 {
        bare.reference(&format!("refs/tags/tag-{index}"), oid, false, "fixture")
            .unwrap();
    }
    client
        .expect(
            "DELETE",
            &format!("{api}/branches/trunk"),
            Value::Null,
            StatusCode::BAD_REQUEST,
        )
        .await;
    client
        .expect(
            "DELETE",
            &format!("{api}/branches/z-spare"),
            Value::Null,
            StatusCode::NO_CONTENT,
        )
        .await;
    assert_eq!(
        bare.find_reference("refs/heads/trunk").unwrap().target(),
        Some(oid)
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_first_branch_writes_preserve_the_default() {
    default_branch_contract(false).await;
}
#[tokio::test]
async fn postgres_first_branch_writes_preserve_the_default() {
    default_branch_contract(true).await;
}

#[tokio::test]
async fn sqlite_long_account_and_branch_names() {
    long_account_and_branch_contract(false).await;
}
#[tokio::test]
async fn postgres_long_account_and_branch_names() {
    long_account_and_branch_contract(true).await;
}
#[tokio::test]
async fn sqlite_longest_account_name_commits() {
    longest_account_name_contract(false).await;
}
#[tokio::test]
async fn postgres_longest_account_name_commits() {
    longest_account_name_contract(true).await;
}

/// A save to a branch that does not exist is refused by name, from the form and the API, and
/// writes nothing. A branch that moved still conflicts. The form still creates the first file
/// of an empty repository on `unborn`.
async fn missing_branch_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("missing_branch").await;
    let (client, _root) = long_name_client(&f, actor).await;
    let repository = db::repositories::records::get_repository(f.pool(), "missing_branch", "save")
        .await
        .unwrap()
        .unwrap();
    let repository_id = repository.id.to_string();
    let bare = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let api = "/api/repos/missing_branch/save";
    let form_path = "/missing_branch/save/_forms/write-file";

    let (status, body) = client
        .form(
            form_path,
            &[
                ("branch", "main"),
                ("path", "README.md"),
                ("intent", "save"),
                ("creating", "yes"),
                ("repository_id", &repository_id),
                ("message", "first"),
                ("content", "# first\n"),
                ("expected_revision", "unborn"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let tip = bare
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap()
        .to_string();

    // The editor was opened on main, then its branch was changed to one that does not exist.
    let (status, body) = client
        .form(
            form_path,
            &[
                ("branch", "feature-pr"),
                ("path", "feature.txt"),
                ("intent", "save"),
                ("creating", "yes"),
                ("repository_id", &repository_id),
                ("message", "feature draft"),
                ("content", "typed but not saved"),
                ("expected_revision", &tip),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("Branch `feature-pr` does not exist."),
        "{body}"
    );
    assert!(body.contains("typed but not saved"), "{body}");
    assert!(!body.contains("The branch changed"), "{body}");

    let refused = client
        .expect(
            "POST",
            &format!("{api}/contents/feature.txt"),
            json!({"branch": "feature-pr", "content": base64::engine::general_purpose::STANDARD.encode("feature\n"), "message": "feature", "expected_revision": tip}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(refused["error"], "file_write_rejected", "{refused}");
    assert_eq!(
        refused["message"],
        "Branch `feature-pr` does not exist. Create it on the Branches page first, or save to an existing branch.",
        "{refused}"
    );
    assert!(bare.find_reference("refs/heads/feature-pr").is_err());
    assert_eq!(
        bare.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        tip
    );

    // A branch that moved since the write was loaded still conflicts.
    let moved = save_file(&client, api, "main", "NOTES.md", "second", &tip, true).await;
    let stale = client
        .expect(
            "PUT",
            &format!("{api}/contents/README.md"),
            json!({"branch": "main", "content": base64::engine::general_purpose::STANDARD.encode("stale\n"), "message": "stale", "expected_revision": tip}),
            StatusCode::CONFLICT,
        )
        .await;
    assert!(
        stale["message"]
            .as_str()
            .unwrap()
            .contains("The branch changed"),
        "{stale}"
    );
    assert_eq!(
        bare.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        moved
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_missing_branch_saves_are_named() {
    missing_branch_contract(false).await;
}
#[tokio::test]
async fn postgres_missing_branch_saves_are_named() {
    missing_branch_contract(true).await;
}

/// A protected branch refuses the editor's direct commit, even a forged one. The form's new-branch
/// choice commits the same file to a new branch that starts at the tip the editor showed, and
/// `main` does not move.
async fn protected_branch_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("protected_editor").await;
    let (client, root) = long_name_client(&f, owner).await;
    let router = f.router(root.path()).await;
    let (session, csrf) = (client.session.clone(), client.csrf.clone());
    let api = "/api/repos/protected_editor/save";
    let tip = save_file(&client, api, "main", "README.md", "first", "unborn", true).await;
    let repository =
        db::repositories::records::get_repository(f.pool(), "protected_editor", "save")
            .await
            .unwrap()
            .unwrap();
    db::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        db::repositories::BranchProtectionRuleUpsert {
            actor_id: owner,
            repo_id: repository.id,
            pattern: "main",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let repository_id = repository.id.to_string();
    let page = "/protected_editor/save/edit/main/README.md";
    let (status, html) = request(&router, &session, &csrf, "GET", page, "", String::new()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Commit directly to the main branch"),
        "{html}"
    );
    assert!(html.contains("protected_editor-patch-1"), "{html}");

    let post = |commit_target: Option<&str>| {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("csrf_token", &csrf)
            .append_pair("branch", "main")
            .append_pair("path", "README.md")
            .append_pair("intent", "save")
            .append_pair("repository_id", &repository_id)
            .append_pair("message", "protected edit")
            .append_pair("content", "reviewed\n")
            .append_pair("expected_revision", &tip);
        if let Some(target) = commit_target {
            form.append_pair("commit_target", target)
                .append_pair("new_branch", "protected_editor-patch-1");
        }
        form.finish()
    };
    let (status, body) = request(
        &router,
        &session,
        &csrf,
        "POST",
        "/protected_editor/save/_forms/write-file",
        "application/x-www-form-urlencoded",
        post(None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("This branch is protected"), "{body}");

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/protected_editor/save/_forms/write-file")
                .header(header::COOKIE, format!("session_id={session}"))
                .header("x-csrf-token", &csrf)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(post(Some("new_branch"))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/protected_editor/save/compare/main...protected_editor-patch-1"
    );

    let repo = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let main = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(main.to_string(), tip);
    let created = repo
        .find_reference("refs/heads/protected_editor-patch-1")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert_eq!(created.parent_id(0).unwrap().to_string(), tip);
    let entry = created
        .tree()
        .unwrap()
        .get_path(std::path::Path::new("README.md"))
        .unwrap();
    assert_eq!(repo.find_blob(entry.id()).unwrap().content(), b"reviewed\n");
    f.finish().await;
}

#[tokio::test]
async fn sqlite_protected_branch_commits_to_a_new_branch() {
    protected_branch_contract(false).await;
}

#[tokio::test]
async fn postgres_protected_branch_commits_to_a_new_branch() {
    protected_branch_contract(true).await;
}

/// The commit at the tip of `branch`.
fn branch_tip<'r>(repo: &'r git2::Repository, branch: &str) -> git2::Commit<'r> {
    repo.find_reference(&format!("refs/heads/{branch}"))
        .unwrap()
        .peel_to_commit()
        .unwrap()
}

/// The bytes of `path` in `commit`.
fn commit_blob(repo: &git2::Repository, commit: &git2::Commit, path: &str) -> Vec<u8> {
    let entry = commit
        .tree()
        .unwrap()
        .get_path(std::path::Path::new(path))
        .unwrap();
    repo.find_blob(entry.id()).unwrap().content().to_vec()
}

/// The commit target of the file editor on a branch that takes direct commits. A direct commit
/// moves the branch and carries the description as the body of its message. A new-branch commit
/// starts a branch at the tip and leaves the branch where it was. A name git refuses, or one a
/// branch already has, refuses the save and moves nothing.
async fn commit_target_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("commit_target").await;
    let (client, _root) = long_name_client(&f, owner).await;
    let first = save_file(
        &client,
        "/api/repos/commit_target/save",
        "main",
        "README.md",
        "first",
        "unborn",
        true,
    )
    .await;
    let repository = db::repositories::records::get_repository(f.pool(), "commit_target", "save")
        .await
        .unwrap()
        .unwrap();
    let repository_id = repository.id.to_string();
    let repo = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let save = "/commit_target/save/_forms/write-file";

    // The editor offers a new branch named for the user, and this branch takes direct commits.
    let (status, page) = client
        .model("/commit_target/save/edit/main/README.md")
        .await;
    assert_eq!(status, StatusCode::OK);
    let Page::Repositories(view::page::Page::Editor { editor, .. }) = page else {
        panic!("an editor page")
    };
    assert!(!editor.protected);
    assert_eq!(editor.new_branch, "commit_target-patch-1");

    // Native Preview keeps a voluntary new-branch destination on an unprotected branch.
    let preview = client
        .form_reply(
            save,
            &[
                ("branch", "main"),
                ("path", "README.md"),
                ("intent", "preview"),
                ("repository_id", &repository_id),
                ("expected_revision", &first),
                ("commit_target", "new_branch"),
                ("new_branch", "preview-branch"),
                ("content", "draft"),
            ],
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.text());
    assert!(
        preview.text().contains("value=\"new_branch\" checked"),
        "{}",
        preview.text()
    );
    assert_eq!(
        repo.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        first
    );

    // A direct commit moves main, and its description becomes the body of the message.
    let direct = client
        .form_reply(
            save,
            &[
                ("branch", "main"),
                ("path", "README.md"),
                ("intent", "save"),
                ("repository_id", &repository_id),
                ("expected_revision", &first),
                ("commit_target", "direct"),
                ("new_branch", "commit_target-patch-1"),
                ("message", "Tidy README"),
                ("description", "First paragraph.\n\nSecond paragraph.\n"),
                ("content", "tidied\n"),
            ],
        )
        .await;
    assert_eq!(direct.status, StatusCode::SEE_OTHER, "{}", direct.text());
    assert_eq!(
        direct.location.as_deref(),
        Some("/commit_target/save/blob/refs/heads/main/README.md")
    );
    let tidied = branch_tip(&repo, "main");
    assert_eq!(tidied.parent_id(0).unwrap().to_string(), first);
    assert_eq!(
        tidied.message().unwrap().trim_end(),
        "Tidy README\n\nFirst paragraph.\n\nSecond paragraph."
    );
    assert_eq!(commit_blob(&repo, &tidied, "README.md"), b"tidied\n");
    let tidied_id = tidied.id().to_string();

    // A description that is only blank adds no body to the message.
    let plain = client
        .form_reply(
            save,
            &[
                ("branch", "main"),
                ("path", "README.md"),
                ("intent", "save"),
                ("repository_id", &repository_id),
                ("expected_revision", &tidied_id),
                ("commit_target", "direct"),
                ("new_branch", "commit_target-patch-1"),
                ("message", "Plain subject"),
                ("description", " \n"),
                ("content", "plain\n"),
            ],
        )
        .await;
    assert_eq!(plain.status, StatusCode::SEE_OTHER, "{}", plain.text());
    assert_eq!(
        branch_tip(&repo, "main").message().unwrap().trim_end(),
        "Plain subject"
    );

    // A new-branch commit starts at the tip of main. Main keeps its tip, and the new branch
    // carries the change and the description.
    let plain_tip = branch_tip(&repo, "main").id();
    let plain_tip_text = plain_tip.to_string();
    let notes = client
        .form_reply(
            save,
            &[
                ("branch", "main"),
                ("path", "README.md"),
                ("intent", "save"),
                ("repository_id", &repository_id),
                ("expected_revision", &plain_tip_text),
                ("commit_target", "new_branch"),
                ("new_branch", "editor-notes"),
                ("message", "Notes"),
                ("description", "Details line\n"),
                ("content", "notes\n"),
            ],
        )
        .await;
    assert_eq!(notes.status, StatusCode::SEE_OTHER, "{}", notes.text());
    assert_eq!(
        notes.location.as_deref(),
        Some("/commit_target/save/compare/main...editor-notes")
    );
    assert_eq!(branch_tip(&repo, "main").id(), plain_tip);
    let created = branch_tip(&repo, "editor-notes");
    assert_eq!(created.parent_id(0).unwrap(), plain_tip);
    assert_eq!(
        created.message().unwrap().trim_end(),
        "Notes\n\nDetails line"
    );
    assert_eq!(commit_blob(&repo, &created, "README.md"), b"notes\n");

    // A name git refuses, and a name a branch already has, refuse the save and move nothing.
    let invalid = client
        .form_reply(
            save,
            &[
                ("branch", "main"),
                ("path", "README.md"),
                ("intent", "save"),
                ("repository_id", &repository_id),
                ("expected_revision", &plain_tip_text),
                ("commit_target", "new_branch"),
                ("new_branch", "bad name"),
                ("message", "Refused"),
                ("content", "refused\n"),
            ],
        )
        .await;
    assert_eq!(
        invalid.status,
        StatusCode::BAD_REQUEST,
        "{}",
        invalid.text()
    );
    assert!(invalid
        .text()
        .contains("Choose a valid name for the new branch."));
    let taken = client
        .form_reply(
            save,
            &[
                ("branch", "main"),
                ("path", "README.md"),
                ("intent", "save"),
                ("repository_id", &repository_id),
                ("expected_revision", &plain_tip_text),
                ("commit_target", "new_branch"),
                ("new_branch", "editor-notes"),
                ("message", "Taken"),
                ("content", "taken\n"),
            ],
        )
        .await;
    // A name a branch already has is a bad request that asks for another name, not a conflict.
    assert_eq!(taken.status, StatusCode::BAD_REQUEST, "{}", taken.text());
    assert!(taken
        .text()
        .contains("already exists. Choose another name."));
    assert!(taken.location.is_none());
    assert_eq!(branch_tip(&repo, "main").id(), plain_tip);
    assert_eq!(branch_tip(&repo, "editor-notes").id(), created.id());
    f.finish().await;
}

#[tokio::test]
async fn sqlite_commit_target_direct_or_new_branch() {
    commit_target_contract(false).await;
}
#[tokio::test]
async fn postgres_commit_target_direct_or_new_branch() {
    commit_target_contract(true).await;
}

async fn delete_file_contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("delete_editor").await;
    let (client, root) = long_name_client(&f, owner).await;
    let router = f.router(root.path()).await;
    let (session, csrf) = (client.session.clone(), client.csrf.clone());
    let api = "/api/repos/delete_editor/save";
    let first = save_file(
        &client,
        api,
        "main",
        "docs/gone.md",
        "first",
        "unborn",
        true,
    )
    .await;
    let tip = save_file(&client, api, "main", "keep.md", "second", &first, true).await;
    let repository = db::repositories::records::get_repository(f.pool(), "delete_editor", "save")
        .await
        .unwrap()
        .unwrap();
    let repository_id = repository.id.to_string();

    let (status, html) = request(
        &router,
        &session,
        &csrf,
        "GET",
        "/delete_editor/save/delete/main/docs/gone.md",
        "",
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Delete docs/gone.md"), "{html}");

    // A branch that moved on refuses the delete, and the file stays.
    client
        .expect(
            "DELETE",
            &format!("{api}/contents/docs/gone.md"),
            json!({"branch":"main", "message":"stale", "expected_revision":first}),
            StatusCode::CONFLICT,
        )
        .await;
    let deleted = client
        .expect(
            "DELETE",
            &format!("{api}/contents/docs/gone.md"),
            json!({"branch":"main", "message":"", "expected_revision":tip}),
            StatusCode::OK,
        )
        .await;
    let tip = deleted["commit"]["sha"].as_str().unwrap().to_owned();
    let repo = git2::Repository::open_bare(&repository.disk_path).unwrap();
    let tree = repo
        .find_commit(git2::Oid::from_str(&tip).unwrap())
        .unwrap()
        .tree()
        .unwrap();
    assert!(tree.get_path(std::path::Path::new("docs")).is_err());
    assert!(tree.get_path(std::path::Path::new("keep.md")).is_ok());
    assert_eq!(
        deleted["commit"]["message"].as_str().unwrap(),
        "Delete docs/gone.md"
    );

    // Protection applies to the API and to the form exactly as it does to a save.
    db::repositories::protection::upsert_branch_protection_rule(
        f.pool(),
        db::repositories::BranchProtectionRuleUpsert {
            actor_id: owner,
            repo_id: repository.id,
            pattern: "main",
            ..Default::default()
        },
    )
    .await
    .unwrap();
    client
        .expect(
            "DELETE",
            &format!("{api}/contents/keep.md"),
            json!({"branch":"main", "message":"protected", "expected_revision":tip}),
            StatusCode::FORBIDDEN,
        )
        .await;
    let form = |new_branch: bool| {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("csrf_token", &csrf)
            .append_pair("branch", "main")
            .append_pair("path", "keep.md")
            .append_pair("repository_id", &repository_id)
            .append_pair("message", "delete keep")
            .append_pair("expected_revision", &tip);
        if new_branch {
            form.append_pair("commit_target", "new_branch")
                .append_pair("new_branch", "delete_editor-patch-1");
        }
        form.finish()
    };
    let (status, body) = request(
        &router,
        &session,
        &csrf,
        "POST",
        "/delete_editor/save/_forms/delete-file",
        "application/x-www-form-urlencoded",
        form(false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("This branch is protected"), "{body}");

    // A refused form returns to the delete page; the deletion lands on a new branch instead.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/delete_editor/save/_forms/delete-file")
                .header(header::COOKIE, format!("session_id={session}"))
                .header("x-csrf-token", &csrf)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form(true)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/delete_editor/save/compare/main...delete_editor-patch-1"
    );
    let created = repo
        .find_reference("refs/heads/delete_editor-patch-1")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert_eq!(created.parent_id(0).unwrap().to_string(), tip);
    assert!(created
        .tree()
        .unwrap()
        .get_path(std::path::Path::new("keep.md"))
        .is_err());
    let main = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(main.to_string(), tip);
    f.finish().await;
}

#[tokio::test]
async fn sqlite_delete_file_through_api_form_and_protection() {
    delete_file_contract(false).await;
}

#[tokio::test]
async fn postgres_delete_file_through_api_form_and_protection() {
    delete_file_contract(true).await;
}
