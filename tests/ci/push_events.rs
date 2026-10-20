//! Push admission uses the published ref and endpoint trees, regardless of the write route.
use crate::support::database::Database;
use codeyard::backend::{
    ci::{runs::DispatchRequest, CiConfig, CiPipelineKind, CiRunRequest, CiRuns, RefUpdate},
    db,
    notifications::NotificationBroadcaster,
    repositories::{lifecycle, store::repo_store},
};
use std::{collections::BTreeSet, sync::Arc};
use tokio::sync::Notify;

fn commit(path: &str, reference: &str, parent: Option<&str>, source: &str, docs: &str) -> String {
    let git = git2::Repository::open_bare(path).unwrap();
    let mut workflows = git.treebuilder(None).unwrap();
    for (name, event) in [
        ("ci.yml", "push: {paths: [src.txt]}"),
        ("docs.yml", "push: {paths: ['docs/**']}"),
        ("unit-test.yml", "workflow_dispatch:"),
        ("unit test.yml", "workflow_dispatch:"),
        ("テスト.yml", "workflow_dispatch:"),
    ] {
        let yaml = format!("name: {name}\non:\n  {event}\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{{run: echo ok}}]\n");
        workflows
            .insert(name, git.blob(yaml.as_bytes()).unwrap(), 0o100644)
            .unwrap();
    }
    let mut github = git.treebuilder(None).unwrap();
    github
        .insert("workflows", workflows.write().unwrap(), 0o040000)
        .unwrap();
    let mut root = git.treebuilder(None).unwrap();
    root.insert(".github", github.write().unwrap(), 0o040000)
        .unwrap();
    for (name, content) in [("src.txt", source), ("docs.txt", docs)] {
        root.insert(name, git.blob(content.as_bytes()).unwrap(), 0o100644)
            .unwrap();
    }
    let tree = git.find_tree(root.write().unwrap()).unwrap();
    let parent = parent.map(|sha| git.find_commit(git2::Oid::from_str(sha).unwrap()).unwrap());
    let signature = git2::Signature::now("Writer", "writer@example.test").unwrap();
    let oid = git
        .commit(
            None,
            &signature,
            &signature,
            "update",
            &tree,
            &parent.as_ref().into_iter().collect::<Vec<_>>(),
        )
        .unwrap();
    git.reference(reference, oid, true, "test publication")
        .unwrap();
    oid.to_string()
}

async fn contract(pg: bool) {
    let mut f = Database::new(pg).await;
    let actor = f.actor("writer").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        db::repositories::creation::RepositoryCreationOwner::personal(actor),
        "project",
        None,
        false,
        None,
    )
    .await
    .unwrap();
    let base = commit(&repo.disk_path, "refs/heads/main", None, "base", "old docs");
    let old = commit(
        &repo.disk_path,
        "refs/heads/feature",
        Some(&base),
        "feature",
        "old docs",
    );
    let main = commit(
        &repo.disk_path,
        "refs/heads/main",
        Some(&base),
        "base",
        "new docs",
    );
    let new = commit(
        &repo.disk_path,
        "refs/heads/feature",
        Some(&main),
        "feature",
        "new docs",
    );
    let service = CiRuns::new(
        f.pool().clone(),
        Arc::new(CiConfig {
            trusted_actor_ids: BTreeSet::from([actor]),
            ..CiConfig::default()
        }),
        Arc::new(Notify::new()),
        None,
        Arc::new(NotificationBroadcaster::new(f.pool())),
    );
    let store = repo_store(&repo, None);
    assert!(store
        .changed_paths(&old, &new)
        .await
        .unwrap()
        .contains(&"src.txt".into()));
    assert_eq!(
        store.push_changed_paths(&old, &new).await.unwrap(),
        ["docs.txt"]
    );
    service
        .queue_push_runs(
            &repo,
            "writer",
            actor,
            RefUpdate {
                reference: "refs/heads/feature".into(),
                old: Some(old),
                new: new.clone(),
                changed_paths: None,
            },
        )
        .await;
    assert!(db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap()
        .is_empty());
    assert!(
        db::ci::has_ci_accepted_push(f.pool(), repo.id, actor, "refs/heads/feature", &new)
            .await
            .unwrap()
    );

    // Uploading an unchanged source alongside changed documentation must not run source CI.
    use codeyard::backend::repositories::{
        files,
        store::{FileWrite, UploadFile, UploadWrite},
    };
    let uploaded = files::upload_files(
        f.pool(),
        None,
        &service,
        actor,
        "writer",
        repo.id,
        UploadWrite {
            branch: "main".into(),
            message: "docs only".into(),
            expected_revision: main.clone(),
            files: vec![
                UploadFile {
                    path: "src.txt".into(),
                    size: 4,
                },
                UploadFile {
                    path: "docs.txt".into(),
                    size: 7,
                },
            ],
        },
        b"basedocs v2".to_vec(),
        None,
    )
    .await
    .unwrap();
    assert!(db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap()
        .is_empty());
    files::write_file_bytes(
        f.pool(),
        None,
        &service,
        actor,
        "writer",
        "project",
        FileWrite {
            branch: "main".into(),
            path: "src.txt".into(),
            message: "unchanged".into(),
            expected_revision: uploaded.commit.sha,
            creating: false,
            deleting: false,
        },
        b"base".to_vec(),
    )
    .await
    .unwrap();
    assert!(db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap()
        .is_empty());

    // A new ref compares against the empty tree. src.txt matches, docs/** does not.
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        git.reference(
            "refs/heads/new",
            git2::Oid::from_str(&new).unwrap(),
            true,
            "new branch",
        )
        .unwrap();
    }
    service
        .queue_push_runs(
            &repo,
            "writer",
            actor,
            RefUpdate {
                reference: "refs/heads/new".into(),
                old: None,
                new: new.clone(),
                changed_paths: None,
            },
        )
        .await;
    let runs = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 20)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].workflow_path, ".github/workflows/ci.yml");

    let dispatch = |branch: &str, event: &str, workflow: &str| DispatchRequest {
        repo_owner: "writer".into(),
        repo_name: "project".into(),
        branch: branch.into(),
        event: event.into(),
        head_sha: Some(new.clone()),
        workflow_path: Some(workflow.into()),
        pipeline: None,
        environment: None,
        inputs: None,
    };
    // Deleting the accepted branch must not reinterpret it as a same-named tag.
    {
        let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
        git.find_reference("refs/heads/new")
            .unwrap()
            .delete()
            .unwrap();
        git.reference(
            "refs/tags/new",
            git2::Oid::from_str(&main).unwrap(),
            true,
            "tag",
        )
        .unwrap();
    }
    let deleted = service
        .dispatch_run(actor, dispatch("new", "push", ".github/workflows/ci.yml"))
        .await
        .unwrap();
    assert_eq!(deleted.branch, "new");
    assert_eq!(deleted.head_sha.as_deref(), Some(new.as_str()));

    let discovered = codeyard::backend::ci::list_workflows_for_ref(&store, "main")
        .await
        .unwrap();
    let path = ".github/workflows/unit-test.yml";
    assert!(discovered.iter().any(|definition| definition.path == path));
    let run = service
        .dispatch_run(actor, dispatch("feature", "workflow_dispatch", path))
        .await
        .unwrap();
    assert_eq!(run.workflow_path, path);
    for filename in ["unit test.yml", "テスト.yml"] {
        let path = format!(".github/workflows/{filename}");
        assert!(!discovered.iter().any(|definition| definition.path == path));
        assert!(matches!(
            service
                .dispatch_run(actor, dispatch("feature", "workflow_dispatch", &path))
                .await,
            Err(codeyard::backend::ci::admission::AdmissionError::InvalidRequest(_))
        ));
    }
    for reference in [
        "refs/heads/topic/c++".to_owned(),
        "refs/tags/v1.2.3+build.1".to_owned(),
        format!("refs/heads/{}", "a".repeat(246)),
        "refs/heads/refs/tags/v1".to_owned(),
    ] {
        {
            let git = git2::Repository::open_bare(&repo.disk_path).unwrap();
            git.reference(
                &reference,
                git2::Oid::from_str(&new).unwrap(),
                true,
                "test ref",
            )
            .unwrap();
        }
        let run = service
            .trigger_run(CiRunRequest {
                repo_id: repo.id,
                repo_owner: "writer".into(),
                repo_name: repo.name.clone(),
                branch: reference.clone(),
                head_sha: Some(new.clone()),
                event: "push".into(),
                pull_request_id: None,
                pipeline: CiPipelineKind::Ci,
                workflow_paths: vec![".github/workflows/ci.yml".into()],
                environment: None,
                inputs: None,
                triggered_by_id: actor,
                manual: false,
            })
            .await
            .unwrap();
        assert_eq!(run.branch, codeyard::backend::ci::branch_name(&reference));
        let again = service.rerun_run(run.run_id, actor).await.unwrap();
        assert_eq!(again.branch, run.branch);
        assert_eq!(again.head_sha, run.head_sha);
    }
    let count = db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 200)
        .await
        .unwrap()
        .len();
    codeyard::backend::repositories::files::upload_files(
        f.pool(),
        None,
        &service,
        actor,
        "writer",
        repo.id,
        codeyard::backend::repositories::store::UploadWrite {
            branch: "feature".into(),
            expected_revision: new.clone(),
            message: "Docs only".into(),
            files: vec![
                codeyard::backend::repositories::store::UploadFile {
                    path: "src.txt".into(),
                    size: 7,
                },
                codeyard::backend::repositories::store::UploadFile {
                    path: "docs.txt".into(),
                    size: 7,
                },
            ],
        },
        b"featureupdated".to_vec(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        db::ci::list_ci_runs_for_repo(f.pool(), repo.id, 200)
            .await
            .unwrap()
            .len(),
        count,
        "uploading unchanged src.txt must not trigger its path-filtered workflow"
    );
    f.finish().await;
}

#[tokio::test]
async fn sqlite_push_refs_and_filters_match_published_updates() {
    contract(false).await;
}

#[tokio::test]
async fn postgres_push_refs_and_filters_match_published_updates() {
    contract(true).await;
}
