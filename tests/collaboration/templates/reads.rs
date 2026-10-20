use super::*;

#[tokio::test]
async fn directory_valued_templates_do_not_break_issue_forms() {
    for pg in [false, true] {
        let (mut database, _root, alice, _) = setup(pg).await;
        let main = put(&alice, "main", "README.md", "proj", "unborn").await;
        let main = put(
            &alice,
            "main",
            ".github/ISSUE_TEMPLATE/bug.md",
            BUG_TEMPLATE,
            &main,
        )
        .await;
        put(
            &alice,
            "main",
            ".github/ISSUE_TEMPLATE/config.yml/README.md",
            "not a config file",
            &main,
        )
        .await;
        let (status, page) = alice.page("/alice/proj/issues/new").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(
            page.contains("Bug report") && page.contains("Open a blank issue"),
            "{page}"
        );
        database.finish().await;
    }
}

#[tokio::test]
async fn templates_use_exact_default_branch_even_when_it_resembles_a_full_ref() {
    let (mut database, _root, alice, _) = setup(false).await;
    let main = put(
        &alice,
        "main",
        ".github/ISSUE_TEMPLATE.md",
        "Tag issue text",
        "unborn",
    )
    .await;
    let main = put(
        &alice,
        "main",
        ".github/PULL_REQUEST_TEMPLATE.md",
        "Tag pull text",
        &main,
    )
    .await;
    let branch = "refs/tags/release";
    alice.branch(branch, &main).await;
    let mut head = main.clone();
    for (path, text) in [
        (".github/ISSUE_TEMPLATE.md", "Branch issue text"),
        (".github/PULL_REQUEST_TEMPLATE.md", "Branch pull text"),
    ] {
        let saved = alice.expect("PUT", &format!("/api/repos/alice/proj/contents/{path}"),
            json!({"content": base64::engine::general_purpose::STANDARD.encode(text), "message": "Edit template", "branch": branch, "expected_revision": head}), StatusCode::OK).await;
        head = saved["commit"]["sha"].as_str().unwrap().to_owned();
    }
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
    git.reference(
        "refs/tags/release",
        git2::Oid::from_str(&main).unwrap(),
        false,
        "fixture",
    )
    .unwrap();
    let store = codeyard::backend::repositories::store::repo_store(&repo, None);
    use codeyard::backend::repositories::issue_templates::{
        issue_templates, pull_request_template,
    };
    assert_eq!(
        issue_templates(&store, branch).await.unwrap().templates[0].body,
        "Branch issue text"
    );
    assert_eq!(
        pull_request_template(&store, branch)
            .await
            .unwrap()
            .as_deref(),
        Some("Branch pull text")
    );
    git.find_reference("refs/tags/release")
        .unwrap()
        .delete()
        .unwrap();
    assert_eq!(
        issue_templates(&store, branch).await.unwrap().templates[0].body,
        "Branch issue text"
    );
    assert_eq!(
        pull_request_template(&store, branch)
            .await
            .unwrap()
            .as_deref(),
        Some("Branch pull text")
    );
    database.finish().await;
}
