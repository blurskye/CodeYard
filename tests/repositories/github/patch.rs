use super::*;

async fn patch_atomicity(pg: bool) {
    crate::support::fake_webui_bundle::install();
    let f = Database::new(pg).await;
    f.complete_setup().await;
    let alice = f.actor("alice").await;
    let root = tempfile::tempdir().unwrap();
    let router = f.router(root.path()).await;
    let writer = credential(&router, &f, alice, "write").await;
    let (_,value)=checked(&router,"repos/create-for-authenticated-user","POST","/api/v3/user/repos",Some(&writer),json!({"name":"patch","description":"Original","homepage":"https://original.test","auto_init":true}),StatusCode::CREATED).await;
    let id = value["id"].as_i64().unwrap();
    let repo = f.repository(id).await;
    let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    git.branch("dev", &parent, false).unwrap();
    drop(parent);
    checked(&router,"repos/update","PATCH","/api/v3/repos/alice/patch",Some(&writer),json!({"name":"renamed","description":null,"homepage":null,"private":true,"is_template":true,"default_branch":"dev","archived":true}),StatusCode::OK).await;
    let row = f.repository(id).await;
    assert_eq!(row.name, "renamed");
    assert_eq!(row.description, None);
    assert_eq!(row.website, None);
    assert!(row.is_archived);
    assert!(row.is_template);
    assert!(row.is_private);
    assert_eq!(row.default_branch, "dev");
    assert_eq!(git.head().unwrap().name(), Some("refs/heads/dev"));
    checked(&router,"repos/update","PATCH","/api/v3/repos/alice/renamed",Some(&writer),json!({"archived":false,"name":"patch","default_branch":"main","description":"Restored","homepage":"https://original.test"}),StatusCode::OK).await;
    checked(
        &router,
        "repos/create-for-authenticated-user",
        "POST",
        "/api/v3/user/repos",
        Some(&writer),
        json!({"name":"taken"}),
        StatusCode::CREATED,
    )
    .await;
    checked(&router,"repos/update","PATCH","/api/v3/repos/alice/patch",Some(&writer),json!({"name":"taken","description":"Must roll back","homepage":null,"private":false,"default_branch":"dev","allow_merge_commit":false}),StatusCode::UNPROCESSABLE_ENTITY).await;
    let unchanged = |row: &db::models::Repository| {
        assert_eq!(row.name, "patch");
        assert_eq!(row.description.as_deref(), Some("Restored"));
        assert_eq!(row.website.as_deref(), Some("https://original.test/"));
        assert!(row.is_private);
        assert!(!row.is_archived);
        assert!(row.allow_merge_commit);
        assert_eq!(row.default_branch, "main");
    };
    unchanged(&f.repository(id).await);
    assert_eq!(git.head().unwrap().name(), Some("refs/heads/main"));
    let lock = std::path::Path::new(&repo.disk_path).join("HEAD.lock");
    std::fs::write(&lock, b"fixture lock").unwrap();
    checked(&router,"repos/update","PATCH","/api/v3/repos/alice/patch",Some(&writer),json!({"name":"failed","description":"Must not commit","default_branch":"dev","archived":true}),StatusCode::SERVICE_UNAVAILABLE).await;
    std::fs::remove_file(lock).unwrap();
    unchanged(&f.repository(id).await);
    assert_eq!(git.head().unwrap().name(), Some("refs/heads/main"));
    let oid = git.head().unwrap().target().unwrap();
    git.set_head_detached(oid).unwrap();
    checked(
        &router,
        "repos/update",
        "PATCH",
        "/api/v3/repos/alice/patch",
        Some(&writer),
        json!({"name":"taken","default_branch":"dev"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(git.head_detached().unwrap());
    assert_eq!(git.head().unwrap().target(), Some(oid));
    unchanged(&f.repository(id).await);
    git.set_head("refs/heads/main").unwrap();
    drop(git);
    let owner = Client::login(&router, f.pool(), alice).await;
    let reply = owner
        .form_reply(
            "/alice/patch/_forms/default-branch",
            &[("repository_id", &id.to_string()), ("branch", "dev")],
        )
        .await;
    assert!(reply.status.is_redirection(), "{}", reply.text());
    assert_eq!(f.repository(id).await.default_branch, "dev");
}
#[tokio::test]
async fn sqlite_github_patch_atomicity() {
    patch_atomicity(false).await;
}
#[tokio::test]
async fn postgres_github_patch_atomicity() {
    patch_atomicity(true).await;
}

#[test]
fn github_repository_patch_preserves_nullable_presence_over_rpc() {
    use codeyard::backend::cluster::rpc::wire::Operation;
    use db::repositories::records::{MergeMethodUpdate, RepositoryChanges};
    let input = RepositoryChanges {
        token: None,
        name: Some("renamed"),
        description: Some(None),
        website: None,
        is_private: Some(false),
        visibility_expected: None,
        archived: Some(false),
        is_template: None,
        wiki: None,
        discussions: None,
        merge_methods: MergeMethodUpdate::default(),
        internal: None,
        features: Default::default(),
    };
    let operation = Operation::PatchRepository {
        default_branch: "dev".into(),
        changes: input.owned(),
    };
    let encoded = serde_json::to_value(operation).unwrap();
    let decoded: Operation = serde_json::from_value(encoded).unwrap();
    let Operation::PatchRepository { changes, .. } = decoded else {
        panic!("wrong RPC operation")
    };
    assert_eq!(changes.description, Some(None));
    assert_eq!(changes.website, None);
    assert_eq!(changes.name.as_deref(), Some("renamed"));
    assert_eq!(changes.is_private, Some(false));
    assert_eq!(changes.archived, Some(false));
    let input = RepositoryChanges {
        description: Some(Some("text")),
        website: Some(None),
        ..Default::default()
    };
    let decoded: RepositoryChanges<String> =
        serde_json::from_value(serde_json::to_value(input.owned()).unwrap()).unwrap();
    assert_eq!(
        decoded.description.as_ref().map(|value| value.as_deref()),
        Some(Some("text"))
    );
    assert_eq!(decoded.website, Some(None));
}
