use crate::support::{database::Database, fake_webui_bundle, http_client::Client};
use axum::http::StatusCode;
use codeyard::{
    backend::db,
    webui::{app::Page, pages::repos},
};

async fn contract(pg: bool) {
    fake_webui_bundle::install();
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let actor = f.actor("preview_owner").await;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("preview.git");
    let git = git2::Repository::init_bare(&path).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let empty = git
        .find_tree(git.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let submodule = git
        .commit(None, &signature, &signature, "submodule", &empty, &[])
        .unwrap();
    let mut tree = git.treebuilder(None).unwrap();
    for (name, bytes) in [
        (
            "late.md",
            format!("{}needle\n", "line\n".repeat(60_000)).into_bytes(),
        ),
        ("dense.txt", vec![b'\n'; 262_144]),
        ("huge.bin", vec![0; 40 * 1024 * 1024]),
    ] {
        tree.insert(name, git.blob(&bytes).unwrap(), 0o100644)
            .unwrap();
    }
    tree.insert("vendor", submodule, 0o160000).unwrap();
    let tree = git.find_tree(tree.write().unwrap()).unwrap();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "files",
        &tree,
        &[],
    )
    .unwrap();
    git.set_head("refs/heads/main").unwrap();
    db::repositories::records::create_repository(
        f.pool(),
        actor,
        "preview",
        None,
        false,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let router = f.router(root.path()).await;
    let client = Client::login(&router, f.pool(), actor).await;
    let (status, page) = client
        .model("/preview_owner/preview/blob/main/late.md?view=code&line=60001")
        .await;
    assert_eq!(status, StatusCode::OK);
    let Page::Repositories(repos::page::Page::Blob { page, .. }) = page else {
        panic!("blob page")
    };
    assert_eq!(page.first_line, Some(60_001));
    assert_eq!(page.offset, 300_000);
    assert_eq!(page.content, "needle\n");
    let (status, html) = client
        .page("/preview_owner/preview/blob/main/late.md?view=code&line=60001")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("id=\"L60001\""));
    assert!(html.contains("needle"));

    let (_, page) = client
        .model("/preview_owner/preview/blob/main/dense.txt")
        .await;
    let Page::Repositories(repos::page::Page::Blob { page, .. }) = page else {
        panic!("blob page")
    };
    assert_eq!(page.content.len(), 2000);
    assert_eq!(page.next, Some(2000));
    let (_, next) = client
        .model("/preview_owner/preview/blob/main/dense.txt?offset=2000")
        .await;
    let Page::Repositories(repos::page::Page::Blob { page: next, .. }) = next else {
        panic!("blob page")
    };
    assert_eq!(next.first_line, Some(2001));
    assert_eq!(next.next, Some(4000));
    let (_, html) = client
        .page("/preview_owner/preview/blob/main/dense.txt")
        .await;
    assert_eq!(html.matches("class=\"repo-code-line\"").count(), 2000);

    for url in [
        "/preview_owner/preview",
        "/preview_owner/preview/blob/main/late.md",
    ] {
        let (status, html) = client.page(url).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert!(html.contains("vendor (submodule)"));
        assert!(!html.contains("/blob/refs/heads/main/vendor"));
    }
    let (_, html) = client
        .page("/preview_owner/preview/find/main?q=vendor")
        .await;
    assert!(html.contains("No files match"));
    let (_, html) = client
        .page("/preview_owner/preview/blob/main/huge.bin")
        .await;
    assert!(html.contains("32 MiB download limit"));
    assert!(!html.contains("download=\"huge.bin\""));
    assert!(!html.contains("Raw contains the complete file"));
    f.finish().await;
}

#[tokio::test]
async fn sqlite_bounded_line_previews_and_submodules() {
    contract(false).await;
}
#[tokio::test]
async fn postgres_bounded_line_previews_and_submodules() {
    contract(true).await;
}
