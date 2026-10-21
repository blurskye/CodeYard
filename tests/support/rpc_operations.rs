//! Every repository operation, with whether a worker may read it while claimed.
use codeyard::backend::{
    cluster::rpc::Operation,
    repositories::git_sessions::ReceiveCredential,
    repositories::store::{
        BlobRange, FileWrite, GitService, GitSession, MergeMethod, MergeRequest, RestackMove,
    },
};
use uuid::Uuid;

pub fn operations(sha: &str) -> Vec<(Operation, bool)> {
    vec![
        (
            Operation::Initialize {
                source: None,
                default_branch: "main".into(),
            },
            false,
        ),
        (Operation::Remove, false),
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
        (Operation::Info, false),
        (Operation::CommitCount, false),
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
            },
            false,
        ),
        (
            Operation::CommitsBetween {
                base_ref: "main".into(),
                head_ref: "main".into(),
                limit: 10,
                raw_json: false,
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
                    commit: codeyard::backend::repositories::store::CommitInfo {
                        sha: sha.into(),
                        message: "fixture".into(),
                        author_name: "Tester".into(),
                        author_email: "tester@example.invalid".into(),
                        committer_name: "Tester".into(),
                        committer_email: "tester@example.invalid".into(),
                        timestamp: 0,
                        committer_timestamp: 0,
                        parents: vec![sha.into()],
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
            Operation::PublishMerge {
                head_branch: "main".into(),
                base_branch: "main".into(),
                head: sha.into(),
                base: sha.into(),
                result: sha.into(),
                method: MergeMethod::Merge,
            },
            false,
        ),
        (
            Operation::PublishStackedMerge {
                heads: vec![("main".into(), sha.into())],
                base_branch: "main".into(),
                base: sha.into(),
                result: sha.into(),
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
