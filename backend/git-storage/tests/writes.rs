use codeyard_database::models::WriteRecord;
use codeyard_git_storage::git_transport::*;
use codeyard_git_storage::repositories::store::RepoStore;
use codeyard_git_storage::store_types::GitStreams;
use codeyard_git_storage::wire::operation_body_limit;
use std::time::Duration;

use codeyard_database::models::RefChange;
use codeyard_git_storage::wire::Operation;
use codeyard_git_storage::wire::MAX_BYTES;
use tokio::io::AsyncWriteExt;

fn commit(repo: &git2::Repository, message: &str) -> String {
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let author = git2::Signature::now("Writer", "writer@example.invalid").unwrap();
    repo.commit(None, &author, &author, message, &tree, &[])
        .unwrap()
        .to_string()
}

fn write(target: &str) -> WriteRecord {
    WriteRecord {
        id: uuid::Uuid::new_v4().to_string(),
        repository_id: 1,
        term: 1,
        primary_id: 1,
        base_version: 0,
        required_votes: 2,
        deadline: chrono::Utc::now() + chrono::Duration::seconds(10),
        decision: "pending".into(),
        committed_version: None,
        finished: false,
        manifest_hash: "a".repeat(64),
        changes: vec![RefChange {
            name: "refs/heads/main".into(),
            kind: "oid".into(),
            old_target: None,
            new_target: Some(target.into()),
        }],
        votes: Vec::new(),
    }
}

fn limits() -> WriteLimits {
    WriteLimits {
        timeout: Duration::from_secs(10),
        pack_bytes: MAX_BYTES as u64,
    }
}

#[tokio::test]
async fn prepared_objects_are_pinned_without_publishing_and_apply_uses_old_values() {
    let directory = tempfile::tempdir().unwrap();
    let primary = directory.path().join("primary.git");
    let follower = directory.path().join("follower.git");
    let primary_repo = git2::Repository::init_bare(&primary).unwrap();
    let first = commit(&primary_repo, "first");
    let proposed = commit(&primary_repo, "proposed");
    let record = write(&proposed);
    git2::Repository::init_bare(&follower).unwrap();
    let preparation = preparation_path(&follower, &record).unwrap();
    tokio::fs::create_dir_all(&preparation).await.unwrap();
    let mut pack = tokio::fs::File::create(preparation.join("objects.pack"))
        .await
        .unwrap();
    let store = RepoStore::from_disk_path(primary.to_str().unwrap());
    let status = store
        .replica_pack(
            &[first.clone(), proposed.clone()],
            &[],
            GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut pack,
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success());
    pack.sync_all().await.unwrap();
    retain_write(&follower, &record, limits()).await.unwrap();
    let repo = git2::Repository::open_bare(&follower).unwrap();
    assert!(repo.find_reference("refs/heads/main").is_err());
    assert!(repo
        .find_object(git2::Oid::from_str(&proposed).unwrap(), None)
        .is_ok());
    for (name, target) in pin_names(&record).unwrap() {
        assert_eq!(repo.refname_to_id(&name).unwrap().to_string(), target);
    }
    assert!(preparation.join("manifest.json").is_file());
    apply_refs(&follower, &record, limits().timeout)
        .await
        .unwrap();
    apply_refs(&follower, &record, limits().timeout)
        .await
        .unwrap();
    assert_eq!(
        repo.refname_to_id("refs/heads/main").unwrap().to_string(),
        proposed
    );
    repo.reference(
        "refs/heads/main",
        git2::Oid::from_str(&first).unwrap(),
        true,
        "unexpected move",
    )
    .unwrap();
    assert!(apply_refs(&follower, &record, limits().timeout)
        .await
        .is_err());
    assert_eq!(
        repo.refname_to_id("refs/heads/main").unwrap().to_string(),
        first
    );
}

/// A write pack carries only what the voter lacks, so its size follows the change and
/// not the repository's history.
#[tokio::test]
async fn write_packs_leave_out_history_the_voter_holds() {
    let directory = tempfile::tempdir().unwrap();
    let primary = directory.path().join("primary.git");
    let follower = directory.path().join("follower.git");
    let primary_repo = git2::Repository::init_bare(&primary).unwrap();
    let author = git2::Signature::now("Writer", "writer@example.invalid").unwrap();
    let tree_with = |content: &[u8]| {
        let blob = primary_repo.blob(content).unwrap();
        let mut builder = primary_repo.treebuilder(None).unwrap();
        builder.insert("file", blob, 0o100644).unwrap();
        primary_repo.find_tree(builder.write().unwrap()).unwrap()
    };
    let history = primary_repo
        .commit(
            Some("refs/heads/main"),
            &author,
            &author,
            "history",
            &tree_with(&[7u8; 64 * 1024]),
            &[],
        )
        .unwrap();
    let parent = primary_repo.find_commit(history).unwrap();
    let proposed = primary_repo
        .commit(
            None,
            &author,
            &author,
            "proposed",
            &tree_with(b"change"),
            &[&parent],
        )
        .unwrap()
        .to_string();
    git2::Repository::init_bare(&follower)
        .unwrap()
        .remote_anonymous(primary.to_str().unwrap())
        .unwrap()
        .fetch(&["refs/heads/main:refs/heads/main"], None, None)
        .unwrap();
    let have = voter_tips(&follower, limits()).await.unwrap();
    assert_eq!(have, vec![history.to_string()]);
    let mut record = write(&proposed);
    record.changes[0].old_target = Some(history.to_string());
    let pack_path = preparation_path(&follower, &record).unwrap();
    tokio::fs::create_dir_all(&pack_path).await.unwrap();
    let mut pack = tokio::fs::File::create(pack_path.join("objects.pack"))
        .await
        .unwrap();
    let status = RepoStore::from_disk_path(primary.to_str().unwrap())
        .replica_pack(
            &write_targets(&record).unwrap(),
            &have,
            GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut pack,
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success());
    pack.sync_all().await.unwrap();
    let bytes = tokio::fs::read(pack_path.join("objects.pack"))
        .await
        .unwrap();
    // The proposed commit, its tree and its blob; nothing of the voter's history.
    assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 3);
    retain_write(&follower, &record, limits()).await.unwrap();
    apply_refs(&follower, &record, limits().timeout)
        .await
        .unwrap();
    let repo = git2::Repository::open_bare(&follower).unwrap();
    assert_eq!(
        repo.refname_to_id("refs/heads/main").unwrap().to_string(),
        proposed
    );
}

#[tokio::test]
async fn a_non_commit_branch_target_cannot_receive_a_durable_vote() {
    let primary = tempfile::tempdir().unwrap();
    let follower = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init_bare(primary.path()).unwrap();
    let target = repo.treebuilder(None).unwrap().write().unwrap().to_string();
    let record = write(&target);
    git2::Repository::init_bare(follower.path()).unwrap();
    let preparation = preparation_path(follower.path(), &record).unwrap();
    tokio::fs::create_dir_all(&preparation).await.unwrap();
    let mut pack = tokio::fs::File::create(preparation.join("objects.pack"))
        .await
        .unwrap();
    let status = RepoStore::from_disk_path(primary.path().to_str().unwrap())
        .replica_pack(
            &[target],
            &[],
            GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut pack,
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success());
    pack.sync_all().await.unwrap();
    assert!(retain_write(follower.path(), &record, limits())
        .await
        .is_err());
    assert!(!preparation.join("manifest.json").exists());
    let repo = git2::Repository::open_bare(follower.path()).unwrap();
    assert!(repo.find_reference("refs/heads/main").is_err());
    for (name, _) in pin_names(&record).unwrap() {
        assert!(repo.find_reference(&name).is_err());
    }
}

#[tokio::test]
async fn truncated_preparation_cannot_pin_or_publish_refs() {
    let root = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init_bare(root.path()).unwrap();
    let target = commit(&repo, "target");
    let record = write(&target);
    let preparation = preparation_path(root.path(), &record).unwrap();
    tokio::fs::create_dir_all(&preparation).await.unwrap();
    tokio::fs::write(preparation.join("objects.pack"), b"PACK\0\0\0\x02")
        .await
        .unwrap();
    assert!(retain_write(root.path(), &record, limits()).await.is_err());
    assert!(repo.find_reference("refs/heads/main").is_err());
    assert!(!preparation.join("manifest.json").exists());
    for (name, _) in pin_names(&record).unwrap() {
        assert!(repo.find_reference(&name).is_err());
    }
}

#[tokio::test]
async fn write_pack_limit_is_enforced_before_writing_excess_bytes() {
    let mut bytes = Vec::new();
    let mut output = LimitedWriter::new(&mut bytes, 4);
    output.write_all(b"PACK").await.unwrap();
    assert!(output.write_all(b"x").await.is_err());
    assert_eq!(bytes, b"PACK");
}

#[test]
fn voting_and_apply_are_storage_only_and_never_fail_over() {
    for operation in [
        Operation::PrepareWrite {
            id: uuid::Uuid::new_v4().to_string(),
        },
        Operation::ApplyWrite {
            id: uuid::Uuid::new_v4().to_string(),
        },
        Operation::WritePack {
            id: uuid::Uuid::new_v4().to_string(),
            have: Vec::new(),
        },
    ] {
        assert!(codeyard_repos_rpc::repository::authorize_operation(
            &["storage".into()],
            &operation
        )
        .is_ok());
        for role in ["web", "ssh", "runner"] {
            assert!(codeyard_repos_rpc::repository::authorize_operation(
                &[role.into()],
                &operation
            )
            .is_err());
        }
        assert!(!operation.is_read_only());
        assert_eq!(operation_body_limit(&operation), 0);
    }
}
