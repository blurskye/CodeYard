use crate::support::{database::Database, fake_webui_bundle as bundle, http_client::Client};
use axum::http::StatusCode;
use codeyard::backend::{db, repositories::lifecycle};

async fn contract(pg: bool) {
    bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(alice),
        "proj",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
    let blob = git.blob(b"branch content\n").unwrap();
    let mut docs = git.treebuilder(None).unwrap();
    docs.insert("a.txt", blob, 0o100644).unwrap();
    let mut tree = git.treebuilder(None).unwrap();
    tree.insert("docs", docs.write().unwrap(), 0o040000)
        .unwrap();
    tree.insert(
        "README.md",
        git.blob(b"[guide](docs/a.txt)\n").unwrap(),
        0o100644,
    )
    .unwrap();
    let signature = git2::Signature::now("Alice", "alice@example.invalid").unwrap();
    let tip = git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Files",
            &git.find_tree(tree.write().unwrap()).unwrap(),
            &[],
        )
        .unwrap();
    let mut wrong = git.treebuilder(None).unwrap();
    wrong
        .insert("a.txt", git.blob(b"wrong ref\n").unwrap(), 0o100644)
        .unwrap();
    let other = git
        .commit(
            None,
            &signature,
            &signature,
            "Other",
            &git.find_tree(wrong.write().unwrap()).unwrap(),
            &[],
        )
        .unwrap();
    for (reference, oid) in [
        ("refs/heads/abcdef1".to_owned(), tip),
        ("refs/tags/abcdef1/docs".to_owned(), other),
        ("refs/heads/refs/heads/topic".to_owned(), tip),
        ("refs/heads/topic".to_owned(), other),
        (format!("refs/heads/{tip}/docs"), other),
    ] {
        git.reference(&reference, oid, true, "fixture").unwrap();
    }
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), alice).await;
    let (status, html) = client.page("/alice/proj/tree/abcdef1").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(
        html.contains("href=\"/alice/proj/blob/refs/heads/abcdef1/docs/a.txt\""),
        "{html}"
    );
    assert!(html.contains("/tree/refs/heads/refs/heads/topic"));
    for reference in [
        "refs/heads/abcdef1",
        "refs/heads/refs/heads/topic",
        &tip.to_string(),
    ] {
        let (status, text) = client
            .page(&format!("/alice/proj/raw/{reference}/docs/a.txt"))
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(text, "branch content\n");
    }
    let (status, html) = client
        .page("/alice/proj/tree/refs/heads/refs/heads/topic")
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("/blob/refs/heads/refs/heads/topic/docs/a.txt"));
    let short = &tip.to_string()[..7];
    let (status, html) = client
        .page(&format!("/alice/proj/blob/{short}/README.md"))
        .await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains(&format!("/blob/{tip}/docs/a.txt")));
    let (status, _) = client
        .page(&format!("/alice/proj/tree/main/{}", "a/".repeat(5000)))
        .await;
    assert_eq!(status, StatusCode::URI_TOO_LONG);

    // A gitlink names a commit in another repository, not a blob in this one.
    tree.insert("vendor", other, 0o160000).unwrap();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "Submodule",
        &git.find_tree(tree.write().unwrap()).unwrap(),
        &[&git.find_commit(tip).unwrap()],
    )
    .unwrap();
    let (status, html) = client.page("/alice/proj").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("vendor (submodule)"));
    assert!(!html.contains("/blob/refs/heads/main/vendor"));
    let (status, html) = client.page("/alice/proj/tree/main/docs").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("vendor (submodule)"));
    assert!(!html.contains("/blob/refs/heads/main/vendor"));
    f.finish().await;
}

#[tokio::test]
async fn ref_names_commit_permalinks_and_submodules_on_both_databases() {
    contract(false).await;
    contract(true).await;
}
