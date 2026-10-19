//! Upload files through the browser form, on SQLite and PostgreSQL. A writer adds files in one
//! commit; a reader, a visitor, a stale head, a protected branch and the size limits are refused.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]

use crate::support::database;
use crate::support::fake_webui_bundle as bundle;
use crate::support::pull_requests as http;

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, repositories::lifecycle,
    storage::ObjectStore,
};
use database::Database;
use serde_json::json;
use std::{path::Path, sync::Arc};
use tower::ServiceExt;

const BOUNDARY: &str = "upload-test-boundary";

/// A multipart form: the text fields, then one `files` part per file. Returns the content type.
fn form(
    repository_id: &str,
    fields: &[(&str, &str)],
    files: &[(&str, &[u8])],
) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    let fields = [("repository_id", repository_id)]
        .into_iter()
        .chain(fields.iter().copied());
    for (name, value) in fields {
        body.extend(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .into_bytes(),
        );
    }
    for (file_name, bytes) in files {
        body.extend(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .into_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend(format!("--{BOUNDARY}--\r\n").into_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// Sends one request. An empty session is a visitor. Returns the status, the redirect target and
/// the body.
async fn send(
    router: &Router,
    session: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Option<String>, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, content_type);
    if !session.is_empty() {
        builder = builder.header(header::COOKIE, format!("session_id={session}"));
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|value| value.to_str().unwrap().to_owned());
    let text = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        location,
        String::from_utf8_lossy(&text).into_owned(),
    )
}

fn head(disk: &Path) -> git2::Oid {
    git2::Repository::open_bare(disk)
        .unwrap()
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap()
}

fn file_at(disk: &Path, commit: git2::Oid, path: &str) -> Option<Vec<u8>> {
    let repository = git2::Repository::open_bare(disk).unwrap();
    let tree = repository.find_commit(commit).unwrap().tree().unwrap();
    let entry = tree.get_path(Path::new(path)).ok()?;
    let blob = repository.find_blob(entry.id()).unwrap();
    Some(blob.content().to_vec())
}

async fn contract(pg: bool) {
    // The HTML answers below render full pages, which need the browser bundle.
    bundle::install();
    let mut f = Database::new(pg).await;
    f.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
    let owner = f.actor("upload_owner").await;
    let reader_id = f.actor("upload_reader").await;
    let root = tempfile::tempdir().unwrap();
    // A public repository: a reader can see it but not write to it.
    let repository = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "site",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let repository_id = repository.id.to_string();
    let disk = std::path::PathBuf::from(repository.disk_path.clone());
    let state = AppState::new(
        f.pool().clone(),
        root.path().to_owned(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    );
    let upload_slots = state.upload_reads.clone();
    let router = create_router(state);
    let writer = http::Client::login(&router, f.pool(), owner).await;
    let reader = http::Client::login(&router, f.pool(), reader_id).await;
    let csrf = writer.csrf.clone();
    let page = "/upload_owner/site/upload/main";
    let tree = "/upload_owner/site/tree/main";

    // An upload opened for a private repository cannot reach a public replacement.
    let original = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "private-upload",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    db::repositories::records::rename_repository(f.pool(), owner, original.id, "renamed-upload")
        .await
        .unwrap();
    let replacement = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(owner),
        "private-upload",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let (content_type, body) = form(
        &original.id.to_string(),
        &[
            ("csrf_token", &csrf),
            ("expected_revision", "unborn"),
            ("message", "private draft"),
        ],
        &[("secret.txt", b"private\n")],
    );
    let (status, _, reply) = send(
        &router,
        &writer.session,
        "POST",
        "/upload_owner/private-upload/upload/main",
        &content_type,
        body,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(
        reply.contains(&format!("name=\"repository_id\" value=\"{}\"", original.id)),
        "{reply}"
    );
    assert!(reply.contains("private draft"));
    assert!(
        codeyard::backend::repositories::store::repo_store(&replacement, None)
            .refs()
            .await
            .unwrap()
            .is_empty()
    );

    // A writer sees the form. A reader is refused, and a visitor is sent to sign in.
    let (status, _, html) = send(&router, &writer.session, "GET", page, "", Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("name=\"files\""), "{html}");
    let (status, _, _) = send(&router, &reader.session, "GET", page, "", Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, location, _) = send(&router, "", "GET", page, "", Vec::new()).await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(location.as_deref(), Some("/login"));

    // Two files land in one commit on a branch that does not exist yet.
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", "unborn"),
            ("message", "Add two"),
            ("description", "Both files."),
        ],
        &[("one.txt", b"one\n"), ("two.txt", b"two\n")],
    );
    let (status, location, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{reply}");
    assert_eq!(location.as_deref(), Some(tree));
    let first = head(&disk);
    let repository = git2::Repository::open_bare(&disk).unwrap();
    let commit = repository.find_commit(first).unwrap();
    assert_eq!(commit.parent_count(), 0);
    assert!(commit
        .message()
        .unwrap()
        .starts_with("Add two\n\nBoth files."));
    assert_eq!(
        file_at(&disk, first, "one.txt").as_deref(),
        Some(&b"one\n"[..])
    );
    assert_eq!(
        file_at(&disk, first, "two.txt").as_deref(),
        Some(&b"two\n"[..])
    );

    // A file that exists is replaced, the other is kept, and the empty message gets the default.
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &first.to_string()),
            ("message", ""),
        ],
        &[("one.txt", b"uno\n")],
    );
    let (status, location, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{reply}");
    assert_eq!(location.as_deref(), Some(tree));
    let second = head(&disk);
    let repository = git2::Repository::open_bare(&disk).unwrap();
    let commit = repository.find_commit(second).unwrap();
    assert_eq!(commit.parent_id(0).unwrap(), first);
    assert!(commit
        .message()
        .unwrap()
        .starts_with("Add files via upload"));
    assert_eq!(
        file_at(&disk, second, "one.txt").as_deref(),
        Some(&b"uno\n"[..])
    );
    assert_eq!(
        file_at(&disk, second, "two.txt").as_deref(),
        Some(&b"two\n"[..])
    );

    // A directory upload keeps only the file name of each part, so `..` cannot leave it.
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &second.to_string()),
            ("message", "Add a note"),
        ],
        &[("../escape.txt", b"outside\n"), ("note.md", b"# note\n")],
    );
    let (status, location, reply) = send(
        &router,
        &writer.session,
        "POST",
        "/upload_owner/site/upload/main/docs",
        &content_type,
        body,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{reply}");
    assert_eq!(
        location.as_deref(),
        Some("/upload_owner/site/tree/refs/heads/main/docs")
    );
    let third = head(&disk);
    assert_eq!(
        file_at(&disk, third, "docs/escape.txt").as_deref(),
        Some(&b"outside\n"[..])
    );
    assert_eq!(
        file_at(&disk, third, "docs/note.md").as_deref(),
        Some(&b"# note\n"[..])
    );
    assert!(file_at(&disk, third, "escape.txt").is_none());

    // A stale head is a conflict, and the typed message comes back.
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &first.to_string()),
            ("message", "keep this text"),
            ("commit_target", "new_branch"),
            ("new_branch", "draft-branch"),
        ],
        &[("late.txt", b"late\n")],
    );
    let (status, _, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(reply.contains("keep this text"), "{reply}");
    assert!(reply.contains("value=\"new_branch\" checked"), "{reply}");
    assert!(reply.contains("Files were not uploaded"), "{reply}");
    assert_eq!(head(&disk), third);

    // Refusals that write nothing: a missing CSRF token, no file, an empty file part, a `..`
    // name, and a file over 25 MiB.
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", "not-the-token"),
            ("expected_revision", &third.to_string()),
        ],
        &[("forged.txt", b"forged\n")],
    );
    let (status, _, _) = send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &third.to_string()),
        ],
        &[],
    );
    let (status, _, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        reply.contains("Choose at least one file to upload."),
        "{reply}"
    );
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &third.to_string()),
        ],
        &[("", b"")],
    );
    let (status, _, _) = send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &third.to_string()),
        ],
        &[("..", b"x")],
    );
    let (status, _, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(reply.contains("Use a relative file path"), "{reply}");
    let big = vec![0u8; 25 * 1024 * 1024 + 1];
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &third.to_string()),
        ],
        &[("big.bin", &big)],
    );
    let (status, _, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        reply.contains("Each file can be at most 25 MiB."),
        "{reply}"
    );
    assert_eq!(head(&disk), third);

    // With every upload slot taken, a writer's upload is refused before its body is read.
    let held: Vec<_> = (0..4)
        .map(|_| upload_slots.clone().try_acquire_owned().unwrap())
        .collect();
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &third.to_string()),
        ],
        &[("busy.txt", b"busy\n")],
    );
    let (status, _, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reply}");
    assert!(
        reply.contains("Uploads are busy. Try again in a moment."),
        "{reply}"
    );
    assert_eq!(head(&disk), third);
    drop(held);

    // A reader and a visitor cannot write, whatever the form says.
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &reader.csrf),
            ("expected_revision", &third.to_string()),
        ],
        &[("reader.txt", b"reader\n")],
    );
    let (status, _, _) = send(&router, &reader.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &third.to_string()),
        ],
        &[("visitor.txt", b"visitor\n")],
    );
    let (status, location, _) = send(&router, "", "POST", page, &content_type, body).await;
    assert!(status.is_redirection(), "{status}");
    assert_eq!(location.as_deref(), Some("/login"));
    assert_eq!(head(&disk), third);

    // A protected branch refuses direct commits, even from its owner.
    writer
        .expect(
            "PUT",
            "/api/repos/upload_owner/site/branch-protection",
            json!({"pattern": "main", "require_pull_request": true}),
            StatusCode::OK,
        )
        .await;
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &third.to_string()),
        ],
        &[("blocked.txt", b"blocked\n")],
    );
    let (status, _, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{reply}");
    assert_eq!(head(&disk), third);

    // The page offers a commit on a new branch, selected and named after the user.
    let (status, _, html) = send(&router, &writer.session, "GET", page, "", Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Commit directly to the main branch"),
        "{html}"
    );
    assert!(html.contains("This branch is protected."), "{html}");
    assert!(html.contains("upload_owner-patch-1"), "{html}");

    // The files go to a new branch that starts at the tip the page showed. Main does not move.
    let (content_type, body) = form(
        &repository_id,
        &[
            ("csrf_token", &csrf),
            ("expected_revision", &third.to_string()),
            ("commit_target", "new_branch"),
            ("new_branch", "reviewed"),
        ],
        &[("reviewed.txt", b"reviewed\n")],
    );
    let (status, location, reply) =
        send(&router, &writer.session, "POST", page, &content_type, body).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{reply}");
    assert_eq!(
        location.as_deref(),
        Some("/upload_owner/site/compare/main...reviewed")
    );
    assert_eq!(head(&disk), third);
    let repository = git2::Repository::open_bare(&disk).unwrap();
    let created = repository
        .find_reference("refs/heads/reviewed")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert_eq!(created.parent_id(0).unwrap(), third);
    assert_eq!(
        file_at(&disk, created.id(), "reviewed.txt").as_deref(),
        Some(&b"reviewed\n"[..])
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_upload_files_contract() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_upload_files_contract() {
    contract(true).await;
}
