//! Every repository operation, with whether a worker may read it while claimed.
use codeyard::backend::{
    cluster::rpc::Operation,
    repositories::git_sessions::ReceiveCredential,
    repositories::store::{
        BlobRange, FileWrite, GitService, GitSession, MergeMethod, MergeRequest, RestackMove,
    },
};
use uuid::Uuid;

fn prepared(sha: &str) -> codeyard::backend::repositories::store::PreparedFile {
    use codeyard::backend::repositories::store::{CommitInfo, PreparedFile};
    PreparedFile {
        branch: "main".into(),
        reference: "refs/heads/main".into(),
        expected: Some(sha.into()),
        web_commit: false,
        new_branch: false,
        commit: CommitInfo {
            sha: sha.into(),
            tree_sha: sha.into(),
            message: "fixture".into(),
            author_name: "Tester".into(),
            author_email: "tester@example.invalid".into(),
            committer_name: "Tester".into(),
            committer_email: "tester@example.invalid".into(),
            timestamp: 0,
            committer_timestamp: 0,
            parents: vec![sha.into()],
            signature: None,
            signature_payload: None,
        },
    }
}
pub fn operations(sha: &str) -> Vec<(Operation, bool)> {
    vec![
        (
            Operation::Initialize {
                source: None,
                default_branch: "main".into(),
            },
            false,
        ),
        (
            Operation::InitializeCopy {
                source: Uuid::nil(),
                default_branch: "main".into(),
                template: true,
                default_branch_only: true,
            },
            false,
        ),
        (Operation::Remove, false),
        (Operation::GistSnapshot { revision: None }, false),
        (
            Operation::GistHistory {
                revision: None,
                offset: 0,
                limit: 1,
            },
            false,
        ),
        (
            Operation::GistWrite {
                credential: codeyard::backend::repositories::store::GistCredential::Token(1, String::new()),
            },
            false,
        ),
        (
            Operation::PatchRepository {
                default_branch: "main".into(),
                changes: Default::default(),
            },
            false,
        ),
        (
            Operation::TransferRepository {
                new_owner: "destination".into(),
            },
            false,
        ),
        (
            serde_json::from_value(serde_json::json!({"MutateRelease":{"mutation":{"Update":{"id":1,"changes":{}}},"attestation":null}})).unwrap(),
            false,
        ),
        (
            Operation::ConfigureDependencySecurity {
                feature: codeyard::backend::db::repositories::dependencies::Feature::Alerts,
                enabled: true,
                private_consent: false,
            },
            false,
        ),
        (
            Operation::PublishDependencyUpdate {
                prepared: prepared(sha),
                publication: Box::new(
                    codeyard::backend::db::repositories::dependencies::Publication {
                        job: codeyard::backend::db::repositories::dependencies::Job {
                            repo_id: 1,
                            generation: 1,
                            nonce: Uuid::new_v4().to_string(),
                            actor_id: 1,
                            updates_actor_id: Some(1),
                        },
                        key: "0".repeat(64),
                        update: codeyard::backend::db::repositories::dependencies::Update {
                            branch: "main".into(),
                            base_sha: sha.into(),
                            commit_sha: sha.into(),
                            old_version: "1.0.0".into(),
                            fixed_version: "1.0.1".into(),
                            requested_by: 1,
                            pull_request_id: None,
                        },
                    },
                ),
            },
            false,
        ),
        (
            Operation::FetchPullHead {
                source: Uuid::nil(),
                number: 1,
                head: "main".into(),
                expected_head: None,
            },
            false,
        ),
        (Operation::Exists, false),
        (Operation::SizeBytes, false),
        (Operation::Refs, false),
        (Operation::PrimaryRefs, false),
        (
            Operation::BranchPublication {
                repository_id: 1,
                reference: "refs/heads/main".into(),
            },
            false,
        ),
        (Operation::Info, false),
        (Operation::CommitCount, false),
        (
            Operation::GitTree {
                revision: sha.into(),
                recursive: true,
            },
            false,
        ),
        (Operation::GitTag { sha: sha.into() }, false),
        (Operation::GitObjectKind { sha: sha.into() }, false),
        (
            Operation::WriteGitObject {
                input: codeyard::backend::git::objects::ObjectWrite::Blob,
            },
            false,
        ),
        (
            Operation::UpdateReference {
                reference: "refs/heads/main".into(),
                sha: sha.into(),
                force: false,
            },
            false,
        ),
        (
            Operation::Tree {
                metadata_only: false,
                git_ref: "main".into(),
                path: "".into(),
            },
            false,
        ),
        (
            Operation::TemplateTexts {
                revision: "main".into(),
                paths: vec![".github/ISSUE_TEMPLATE.md".into()],
            },
            false,
        ),
        (
            Operation::WorkflowFiles {
                revision: "main".into(),
            },
            false,
        ),
        (
            Operation::Blob {
                git_ref: "main".into(),
                path: ".github/workflows/ci.yml".into(),
            },
            false,
        ),
        (
            Operation::BlobBytes {
                git_ref: "main".into(),
                path: ".github/workflows/ci.yml".into(),
                range: BlobRange::Full,
            },
            false,
        ),
        (Operation::BlobById { id: sha.into() }, true),
        (
            Operation::Commits {
                git_ref: "main".into(),
                limit: 10,
                filter: Default::default(),
                walk_millis: None,
                raw_json: false,
            },
            false,
        ),
        (
            Operation::CommitPage {
                git_ref: "main".into(),
                offset: 1,
                limit: 10,
                filter: Default::default(),
                walk_millis: None,
                raw_json: false,
            },
            false,
        ),
        (
            Operation::PathCommits {
                git_ref: "main".into(),
                path: "README.md".into(),
                offset: 0,
                limit: 10,
                filter: Default::default(),
                raw_json: false,
            },
            false,
        ),
        (
            Operation::CommitsBetween {
                base_ref: "main".into(),
                head_ref: "main".into(),
                limit: 10,
                raw_json: false,
                pull_request: false,
            },
            false,
        ),
        (
            Operation::Commit {
                revision: sha.into(),
            },
            false,
        ),
        (
            Operation::PrimaryCommit {
                revision: sha.into(),
            },
            false,
        ),
        (
            Operation::CommitDiff {
                revision: sha.into(),
            },
            false,
        ),
        (
            Operation::Diff {
                base_ref: "main".into(),
                head_ref: "main".into(),
                stats: false,
            },
            false,
        ),
        (
            Operation::ChangedPaths {
                base_ref: sha.into(),
                head_ref: sha.into(),
                endpoints: false,
                renames: false,
            },
            false,
        ),
        (
            Operation::DiffFrom {
                source: Uuid::nil(),
                base_ref: sha.into(),
                head_ref: sha.into(),
            },
            false,
        ),
        (
            Operation::ComparisonFrom {
                source: Uuid::nil(),
                base_ref: sha.into(),
                head_ref: sha.into(),
                offset: 0,
                limit: 1,
                raw_json: false,
            },
            false,
        ),
        (Operation::Tags, false),
        (
            Operation::BranchStatuses {
                base: sha.into(),
                heads: vec![sha.into()],
            },
            false,
        ),
        (
            Operation::Archive {
                git_ref: "main".into(),
                prefix: "project".into(),
                format: codeyard::backend::git::ArchiveFormat::Zip,
            },
            false,
        ),
        (
            Operation::Languages {
                git_ref: "main".into(),
            },
            false,
        ),
        (
            Operation::FilePaths {
                git_ref: "main".into(),
                limit: 10,
                raw_json: false,
            },
            false,
        ),
        (Operation::Contributors, false),
        (
            Operation::CommitStatistics {
                branch: "main".into(),
                raw_json: false,
            },
            false,
        ),
        (
            Operation::IsAncestor {
                ancestor: sha.into(),
                descendant: sha.into(),
            },
            false,
        ),
        (
            Operation::ResolveCiCommit {
                reference: "main".into(),
                sha: Some(sha.into()),
                must_be_on_ref: true,
            },
            false,
        ),
        (
            Operation::CiWorkflowFile {
                sha: sha.into(),
                workflow_path: ".github/workflows/ci.yml".into(),
            },
            false,
        ),
        (
            Operation::CreateBranch {
                name: "worker-branch".into(),
                sha: sha.into(),
            },
            false,
        ),
        (
            Operation::DeleteBranch {
                name: "main".into(),
            },
            false,
        ),
        (
            Operation::CreateTag {
                name: "worker-tag".into(),
                sha: sha.into(),
                message: None,
                tagger_name: "Tester".into(),
                tagger_email: "tester@example.invalid".into(),
            },
            false,
        ),
        (
            Operation::DeleteTag {
                name: "worker-tag".into(),
            },
            false,
        ),
        (
            Operation::PrepareFile {
                write: FileWrite {
                    branch: "main".into(),
                    path: "worker.txt".into(),
                    message: "Write from worker".into(),
                    expected_revision: sha.into(),
                    creating: true,
                    deleting: false,
                },
                author: "Tester".into(),
                email: "tester@example.invalid".into(),
            },
            false,
        ),
        (
            Operation::PublishFile {
                prepared: codeyard::backend::repositories::store::PreparedFile {
                    branch: "main".into(),
                    reference: "refs/heads/main".into(),
                    expected: Some(sha.into()),
                    web_commit: false,
                    new_branch: false,
                    commit: codeyard::backend::repositories::store::CommitInfo {
                        sha: sha.into(),
                        tree_sha: sha.into(),
                        message: "fixture".into(),
                        author_name: "Tester".into(),
                        author_email: "tester@example.invalid".into(),
                        committer_name: "Tester".into(),
                        committer_email: "tester@example.invalid".into(),
                        timestamp: 0,
                        committer_timestamp: 0,
                        parents: vec![sha.into()],
                        signature: None,
                        signature_payload: None,
                    },
                },
            },
            false,
        ),
        (
            Operation::SaveFile {
                write: FileWrite {
                    branch: "main".into(),
                    path: "worker.txt".into(),
                    message: "Write from worker".into(),
                    expected_revision: sha.into(),
                    creating: true,
                    deleting: false,
                },
                author: "Tester".into(),
                email: "tester@example.invalid".into(),
            },
            false,
        ),
        (
            Operation::Merge {
                request: MergeRequest {
                    head_branch: "feature".into(),
                    base_branch: "main".into(),
                    head: sha.into(),
                    base: sha.into(),
                    message: "Merge from worker".into(),
                    squash_body: None,
                    name: "Tester".into(),
                    email: "tester@example.invalid".into(),
                    method: MergeMethod::Merge,
                    author_name: None,
                    author_email: None,
                },
            },
            false,
        ),
        (
            Operation::PrepareMerge {
                request: MergeRequest {
                    head_branch: "feature".into(),
                    base_branch: "main".into(),
                    head: sha.into(),
                    base: sha.into(),
                    message: "Prepare from worker".into(),
                    squash_body: None,
                    name: "Tester".into(),
                    email: "tester@example.invalid".into(),
                    method: MergeMethod::Merge,
                    author_name: None,
                    author_email: None,
                },
            },
            false,
        ),
        (
            Operation::UpdatePullBranch {
                source: Uuid::nil(),
                request: codeyard::backend::repositories::store::PullBranchUpdate {
                    pull_request_id: 1,
                    base_branch: "main".into(),
                    branch: "feature".into(),
                    head: sha.into(),
                    base: sha.into(),
                    name: "Tester".into(),
                    email: "tester@example.invalid".into(),
                    token: None,
                },
            },
            false,
        ),
        (
            Operation::PublishAsyncMerge {
                request: codeyard::backend::repositories::store::AsyncMergePublication {
                    uuid: uuid::Uuid::nil().to_string(),
                    token_id: 1,
                    pull_request_id: 1,
                    heads: vec![("main".into(), sha.into())],
                    base_branch: "main".into(),
                    base: sha.into(),
                    result: sha.into(),
                    method: MergeMethod::Merge,
                },
            },
            false,
        ),
        (
            Operation::PublishMerge {
                head_branch: "main".into(),
                base_branch: "main".into(),
                head: sha.into(),
                base: sha.into(),
                result: sha.into(),
                method: MergeMethod::Merge,
                token: None,
            },
            false,
        ),
        (
            Operation::PublishBranchMerge {
                publication: codeyard::backend::repositories::store::BranchMergePublication {
                    base_branch: "main".into(),
                    base: sha.into(),
                    head_branch: Some("topic".into()),
                    head: sha.into(),
                    result: sha.into(),
                    token: None,
                },
            },
            false,
        ),
        (
            Operation::PublishStackedMerge {
                heads: vec![("main".into(), sha.into())],
                base_branch: "main".into(),
                base: sha.into(),
                result: sha.into(),
                token: None,
            },
            false,
        ),
        (
            Operation::PrepareRestack {
                from: sha.into(),
                head: sha.into(),
                onto: sha.into(),
                name: "Tester".into(),
                email: "tester@example.invalid".into(),
            },
            false,
        ),
        (
            Operation::PublishRestack {
                stack_id: 1,
                versions: vec![(1, 1)],
                base_branch: "main".into(),
                base: sha.into(),
                moves: vec![RestackMove {
                    branch: "feature".into(),
                    old: sha.into(),
                    new: sha.into(),
                }],
            },
            false,
        ),
        (
            Operation::AdvertiseRefs {
                service: GitService::UploadPack,
            },
            false,
        ),
        (
            Operation::AdvertiseRefs {
                service: GitService::ReceivePack,
            },
            false,
        ),
        (
            Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
            true,
        ),
        (
            Operation::GitSession(GitSession {
                service: GitService::ReceivePack,
                stateless: true,
                git_protocol: None,
                credential: Some(ReceiveCredential::SshKey(1)),
                owner: "ci_owner".into(),
                allow_unreachable: false,
            }),
            false,
        ),
        (
            Operation::ReplicaPack {
                have: vec![sha.into()],
            },
            false,
        ),
    ]
}
