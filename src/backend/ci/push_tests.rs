use super::*;
use crate::backend::ci::workflows::parse_push_workflow;

fn revision(paths: usize) -> (tempfile::TempDir, RepoStore, String) {
    let root = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init_bare(root.path()).unwrap();
    let blob = repo.blob(b"readme").unwrap();
    let mut tree = repo.treebuilder(None).unwrap();
    tree.insert("README", blob, 0o100644).unwrap();
    for index in 0..paths {
        tree.insert(format!("file-{index}"), blob, 0o100644)
            .unwrap();
    }
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let author = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
    let sha = repo
        .commit(None, &author, &author, "new branch", &tree, &[])
        .unwrap()
        .to_string();
    let store = RepoStore::from_disk_path(root.path().to_str().unwrap());
    (root, store, sha)
}

fn workflow(name: &str, filters: &str) -> WorkflowDefinition {
    parse_push_workflow(
        &format!(".github/workflows/{name}.yml"),
        &format!(
            "on:\n  push: {filters}\njobs: {{check: {{runs-on: ubuntu-latest, steps: []}}}}\n"
        ),
    )
    .unwrap()
}

#[tokio::test]
async fn new_refs_match_paths_against_the_same_root_tree_for_every_publisher() {
    let (_root, store, sha) = revision(0);
    let update = RefUpdate {
        reference: "refs/heads/topic".into(),
        old: None,
        new: sha,
        // A new branch compares its whole tree, not just the editor's last changed file.
        changed_paths: Some(vec!["src/not-in-the-tree.rs".into()]),
    };
    let matched = matching_push_workflows(
        &store,
        &update,
        vec![
            workflow("source", "{paths: ['src/**']}"),
            workflow("readme", "{paths: ['README']}"),
        ],
        None,
        &mut WorkflowMatchBudget::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        matched.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(),
        [".github/workflows/readme.yml"]
    );
}

#[tokio::test]
async fn unavailable_large_path_lists_skip_only_path_filtered_workflows() {
    let (_root, store, sha) = revision(crate::backend::git::MAX_CI_CHANGED_PATHS + 1);
    let update = RefUpdate {
        reference: "refs/heads/main".into(),
        old: None,
        new: sha,
        changed_paths: None,
    };
    let matched = matching_push_workflows(
        &store,
        &update,
        vec![
            workflow("all", "{}"),
            workflow("docs", "{paths: ['docs/**']}"),
            workflow("other-branch", "{branches: ['other'], paths: ['**']}"),
        ],
        None,
        &mut WorkflowMatchBudget::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        matched.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(),
        [".github/workflows/all.yml"]
    );
}

#[tokio::test]
async fn branch_excluded_path_filters_do_not_require_a_comparison() {
    let (_root, store, sha) = revision(0);
    let update = RefUpdate {
        reference: "refs/heads/main".into(),
        old: Some("missing".into()),
        new: sha,
        changed_paths: None,
    };
    let matched = matching_push_workflows(
        &store,
        &update,
        vec![
            workflow("all", "{}"),
            workflow("other", "{branches: ['other'], paths: ['docs/**']}"),
        ],
        None,
        &mut WorkflowMatchBudget::new(),
    )
    .await
    .unwrap();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].path, ".github/workflows/all.yml");
}
