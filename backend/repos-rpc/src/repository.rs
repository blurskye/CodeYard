use crate::RpcHandler;
use anyhow::Result;
use codeyard_git::error::RepoError;
use codeyard_git::MAX_CI_WORKFLOW_BYTES;
use codeyard_repos::protection::check_branch_write;
use codeyard_repos::protection::matching_rules;
use codeyard_repos::protection::BranchWrite;
use codeyard_repos::protection::History;
use codeyard_repos::protection::Refusal;
use codeyard_repos::protection::WriteVia;
use codeyard_repos::protection::ANY_TIP;
use codeyard_repos_core::cluster::replicas::copy_path;
use codeyard_repos_core::cluster::replicas::moving;
use codeyard_repos_core::cluster::replicas::MOVE_WAIT;
use codeyard_repos_core::repositories::store::RepoStore;
use codeyard_repos_core::store_types::BlobMetadata;
use codeyard_repos_core::store_types::GitService;
use codeyard_repos_core::store_types::WorkflowPayloadEntry;
use codeyard_repos_core::store_types::MAX_WORKFLOW_PAYLOAD_BYTES;
use codeyard_repos_core::wire::invalid;
use codeyard_repos_core::wire::unavailable;
use codeyard_repos_core::wire::Failure;
use codeyard_repos_core::wire::Operation;
use codeyard_repos_core::wire::Request;
use codeyard_repos_core::wire::MAX_BYTES;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

fn encode_json(
    value: &impl serde::Serialize,
    raw: bool,
    body: &mut Vec<u8>,
) -> Result<Value, Failure> {
    if raw {
        *body = serde_json::to_vec(value).map_err(|_| Failure::Repository(unavailable()))?;
        Ok(Value::Null)
    } else {
        serde_json::to_value(value).map_err(|_| Failure::Repository(unavailable()))
    }
}

/// Returns the operation name for safe logging; workers also need a running claim.
pub fn authorize_operation(
    roles: &[String],
    operation: &Operation,
) -> Result<&'static str, RepoError> {
    let (name, worker_read) = match operation {
        Operation::Initialize { .. } => ("Initialize", false),
        Operation::InitializeForeignCopy { .. } => ("InitializeForeignCopy", false),
        Operation::InitializeCopy { .. } => ("InitializeCopy", false),
        Operation::Remove => ("Remove", false),
        Operation::Retire => ("Retire", false),
        Operation::GistSnapshot { .. } => ("GistSnapshot", false),
        Operation::GistHistory { .. } => ("GistHistory", false),
        Operation::GistWrite { .. } => ("GistWrite", false),
        Operation::FetchPullHead { .. } => ("FetchPullHead", false),
        Operation::CompareForeignPullHead { .. } => ("CompareForeignPullHead", false),
        Operation::FetchForeignPullHead { .. } => ("FetchForeignPullHead", false),
        Operation::FollowForeignPullHead { .. } => ("FollowForeignPullHead", false),
        Operation::Exists => ("Exists", false),
        Operation::SizeBytes => ("SizeBytes", false),
        Operation::RefreshStorageSize => ("RefreshStorageSize", false),
        Operation::Refs => ("Refs", false),
        Operation::MatchingRefs { .. } => ("MatchingRefs", false),
        Operation::PrimaryRefs => ("PrimaryRefs", false),
        Operation::PrimaryLanguages { .. } => ("PrimaryLanguages", false),
        Operation::BranchPublication { .. } => ("BranchPublication", false),
        Operation::Info => ("Info", false),
        Operation::CommitCount => ("CommitCount", false),
        Operation::Tree { .. } => ("Tree", false),
        Operation::GitTree { .. } => ("GitTree", false),
        Operation::GitTag { .. } => ("GitTag", false),
        Operation::GitObjectKind { .. } => ("GitObjectKind", false),
        Operation::IntroducedBlobs { .. } => ("IntroducedBlobs", false),
        Operation::WriteGitObject { .. } => ("WriteGitObject", false),
        Operation::ImportGit { .. } => ("ImportGit", false),
        Operation::CancelImport => ("CancelImport", false),
        Operation::LockMigration { .. } => ("LockMigration", false),
        Operation::MigrationArchive => ("MigrationArchive", false),
        Operation::UpdateReference { .. } => ("UpdateReference", false),
        Operation::SidebarTrees { .. } => ("SidebarTrees", false),
        Operation::WorkflowFiles { .. } => ("WorkflowFiles", false),
        Operation::TemplateTexts { .. } => ("TemplateTexts", false),
        Operation::Blob { .. } => ("Blob", false),
        Operation::BlobBytes { .. } => ("BlobBytes", false),
        Operation::BlobById { .. } => ("BlobById", true),
        Operation::BlobByIdRange { .. } => ("BlobByIdRange", false),
        Operation::Commits { .. } => ("Commits", false),
        Operation::CommitPage { .. } => ("CommitPage", false),
        Operation::SearchCode { .. } => ("SearchCode", false),
        Operation::PathCommits { .. } => ("PathCommits", false),
        Operation::Commit { .. } => ("Commit", false),
        Operation::PrimaryCommit { .. } => ("PrimaryCommit", false),
        Operation::ReleaseHistory { .. } => ("ReleaseHistory", false),
        Operation::CommitDiff { .. } => ("CommitDiff", false),
        Operation::Diff { .. } => ("Diff", false),
        Operation::CommitsBetween { .. } => ("CommitsBetween", false),
        Operation::DiffFile { .. } => ("DiffFile", false),
        Operation::ChangedPaths { .. } => ("ChangedPaths", false),
        Operation::DiffFrom { .. } => ("DiffFrom", false),
        Operation::ComparisonFrom { .. } => ("ComparisonFrom", false),
        Operation::Tags => ("Tags", false),
        Operation::Archive { .. } => ("Archive", false),
        Operation::Languages { .. } => ("Languages", false),
        Operation::FilePaths { .. } => ("FilePaths", false),
        Operation::Blame { .. } => ("Blame", false),
        Operation::Contributors => ("Contributors", false),
        Operation::BranchContributors { .. } => ("BranchContributors", false),
        Operation::CommitStatistics { .. } => ("CommitStatistics", false),
        Operation::CodeFrequency { .. } => ("CodeFrequency", false),
        Operation::IsAncestor { .. } => ("IsAncestor", false),
        Operation::BranchStatuses { .. } => ("BranchStatuses", false),
        Operation::ResolveCiCommit { .. } => ("ResolveCiCommit", false),
        Operation::CiWorkflowFile { .. } => ("CiWorkflowFile", false),
        Operation::RenameBranch { .. } => ("RenameBranch", false),
        Operation::MergeUpstream { .. } => ("MergeUpstream", false),
        Operation::CreateBranch { .. } => ("CreateBranch", false),
        Operation::DeleteBranch { .. } => ("DeleteBranch", false),
        Operation::SetDefaultBranch { .. } => ("SetDefaultBranch", false),
        Operation::PatchRepository { .. } => ("PatchRepository", false),
        Operation::MutateRelease { .. } => ("MutateRelease", false),
        Operation::TransferRepository { .. } => ("TransferRepository", false),
        Operation::ConfigureDependencySecurity { .. } => ("ConfigureDependencySecurity", false),
        Operation::CreateTag { .. } => ("CreateTag", false),
        Operation::DeleteTag { .. } => ("DeleteTag", false),
        Operation::PrepareFile { .. } => ("PrepareFile", false),
        Operation::PrepareUpload { .. } => ("PrepareUpload", false),
        Operation::PublishFile { .. } => ("PublishFile", false),
        Operation::PublishDependencyUpdate { .. } => ("PublishDependencyUpdate", false),
        Operation::SaveFile { .. } => ("SaveFile", false),
        Operation::Merge { .. } => ("Merge", false),
        Operation::PrepareMerge { .. } => ("PrepareMerge", false),
        Operation::PrepareRevert { .. } => ("PrepareRevert", false),
        Operation::PublishMerge { .. } => ("PublishMerge", false),
        Operation::PublishAsyncMerge { .. } => ("PublishAsyncMerge", false),
        Operation::UpdatePullBranch { .. } => ("UpdatePullBranch", false),
        Operation::PublishBranchMerge { .. } => ("PublishBranchMerge", false),
        Operation::PublishStackedMerge { .. } => ("PublishStackedMerge", false),
        Operation::PrepareRestack { .. } => ("PrepareRestack", false),
        Operation::PublishRestack { .. } => ("PublishRestack", false),
        Operation::AdvertiseRefs { .. } => ("AdvertiseRefs", false),
        Operation::GitSession(session) => ("GitSession", session.service == GitService::UploadPack),
        Operation::ObjectPut { .. }
        | Operation::ObjectGet { .. }
        | Operation::ObjectDelete { .. } => {
            return Err(invalid("object RPC requires object admission"))
        }
        Operation::ForkPack { .. } => ("ForkPack", false),
        Operation::ReplicaPack { .. } => ("ReplicaPack", false),
        Operation::PrepareWrite { .. } => ("PrepareWrite", false),
        Operation::ApplyWrite { .. } => ("ApplyWrite", false),
        Operation::WritePack { .. } => ("WritePack", false),
    };
    // A copy holder asks only for packs of its own copies; authorize_request checks the copy.
    if matches!(
        operation,
        Operation::ReplicaPack { .. }
            | Operation::PrepareWrite { .. }
            | Operation::ApplyWrite { .. }
            | Operation::WritePack { .. }
    ) {
        return if roles.iter().any(|role| role == "storage")
            || (matches!(operation, Operation::ReplicaPack { .. })
                && roles.iter().any(|role| role == "web" || role == "ssh"))
        {
            Ok(name)
        } else {
            Err(invalid(
                "repository RPC ReplicaPack requires a storage node",
            ))
        };
    }
    if (matches!(operation, Operation::Initialize { source: None, .. })
        && roles.iter().any(|role| role == "storage"))
        || roles.iter().any(|role| role == "web" || role == "ssh")
        || (worker_read && roles.iter().any(|role| role == "runner"))
    {
        Ok(name)
    } else {
        Err(invalid(&format!(
            "repository RPC {name} requires a web or ssh node"
        )))
    }
}

fn requires_actor(operation: &Operation) -> bool {
    matches!(
        operation,
        Operation::Initialize {
            source: Some(_),
            ..
        } | Operation::InitializeCopy { .. }
            | Operation::InitializeForeignCopy { .. }
            | Operation::ForkPack { .. }
            | Operation::FetchPullHead { .. }
            | Operation::CompareForeignPullHead { .. }
            | Operation::FetchForeignPullHead { .. }
            | Operation::FollowForeignPullHead { .. }
            | Operation::RenameBranch { .. }
            | Operation::MergeUpstream { .. }
            | Operation::CreateBranch { .. }
            | Operation::DeleteBranch { .. }
            | Operation::SetDefaultBranch { .. }
            | Operation::PatchRepository { .. }
            | Operation::MutateRelease { .. }
            | Operation::TransferRepository { .. }
            | Operation::ConfigureDependencySecurity { .. }
            | Operation::CreateTag { .. }
            | Operation::DeleteTag { .. }
            | Operation::PublishFile { .. }
            | Operation::PublishDependencyUpdate { .. }
            | Operation::WriteGitObject { .. }
            | Operation::GistWrite { .. }
            | Operation::ImportGit { .. }
            | Operation::CancelImport
            | Operation::LockMigration { .. }
            | Operation::MigrationArchive
            | Operation::UpdateReference { .. }
            | Operation::SaveFile { .. }
            | Operation::Merge { .. }
            | Operation::PublishMerge { .. }
            | Operation::PublishAsyncMerge { .. }
            | Operation::UpdatePullBranch { .. }
            | Operation::PublishBranchMerge { .. }
            | Operation::PublishStackedMerge { .. }
            | Operation::PublishRestack { .. }
    )
}

pub(super) struct ReadAdmission {
    pub store: RepoStore,
    pub repo_id: Option<i64>,
    pub guard: tokio::sync::OwnedRwLockReadGuard<()>,
}

/// Blob bytes go in the raw reply body; the JSON value carries only their metadata.
fn blob_reply(
    blob: codeyard_repos_core::store_types::BlobBytes,
    output: &mut Vec<u8>,
) -> Result<Value, Failure> {
    *output = blob.bytes;
    serde_json::to_value(BlobMetadata {
        id: blob.id,
        is_binary: blob.is_binary,
        size: blob.size,
        offset: blob.offset,
        mode: blob.mode,
        preview: blob.preview,
    })
    .map_err(|_| Failure::Repository(unavailable()))
}

fn store_at(path: &Path, node_id: i64) -> Result<RepoStore, RepoError> {
    Ok(RepoStore::on_node(
        path.to_str().ok_or_else(unavailable)?,
        node_id,
    ))
}

impl RpcHandler {
    async fn admitted_store(
        &self,
        repo_id: Option<i64>,
        path: &Path,
        read_only: bool,
    ) -> Result<RepoStore, RepoError> {
        let pool = self.context.database()?;
        if let Some(id) = repo_id {
            let repository =
                codeyard_db_repos::repositories::records::get_repository_by_id(pool, id)
                    .await
                    .map_err(|_| unavailable())?
                    .ok_or_else(unavailable)?;
            if repository.storage_node_id.is_some()
                && codeyard_db_cluster::cluster::writes::write_settings(pool)
                    .await
                    .map_err(|_| unavailable())?
                    .write_quorum
                    .is_some()
            {
                let store = self
                    .context
                    .repository_store(&repository, read_only)
                    .await?;
                if Path::new(store.location()) != path {
                    return Err(unavailable());
                }
                return Ok(store);
            }
        }
        store_at(path, self.context.node_id)
    }

    async fn initialize_copy(
        &self,
        store: &RepoStore,
        repo_id: Option<i64>,
        actor: Option<codeyard_db_core::queries::credentials::Actor>,
        source: Uuid,
        options: &codeyard_repos_core::repositories::store::CopyOptions,
    ) -> Result<(), RepoError> {
        let (source_repo_id, path) = self.check_repository(source, false, false).await?;
        let source = self.admitted_store(source_repo_id, &path, false).await?;
        store
            .initialize_copy(
                self.context.database()?,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                source_repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &source,
                options,
            )
            .await
    }

    /// The row id of a repository stored here, and the directory to read or write it from:
    /// the primary's directory when this node is the primary. A read-only operation may
    /// read this node's copy instead, and a copy without a directory is unavailable.
    async fn check_repository(
        &self,
        uuid: Uuid,
        removing: bool,
        read_only: bool,
    ) -> Result<(Option<i64>, PathBuf), RepoError> {
        let pool = self.context.database()?;
        let repository =
            codeyard_db_cluster::cluster::get_repository_by_uuid(pool, &uuid.to_string())
                .await
                .map_err(|_| unavailable())?;
        let primary =
            codeyard_repos_core::repositories::store::primary_path(&self.context.repos_path, uuid);
        let Some(repo) = repository else {
            // Deletion removes the row before its directory.
            if removing {
                return Ok((None, primary));
            }
            return Err(RepoError::storage_node_unavailable());
        };
        if repo.storage_node_id.is_some()
            && codeyard_db_cluster::cluster::writes::write_settings(pool)
                .await
                .map_err(|_| unavailable())?
                .write_quorum
                .is_some()
        {
            let (path, _) = self
                .context
                .repository_path(repo.id, uuid, read_only, None)
                .await?;
            return Ok((Some(repo.id), path));
        }
        // A repository from before the cluster has no node; it is on the node that
        // has its directory.
        let primary_here = repo.storage_node_id == Some(self.context.node_id)
            || (repo.storage_node_id.is_none() && primary.exists());
        if primary_here {
            let copy = copy_path(&self.context.repos_path, uuid);
            if !primary.exists() && copy.exists() {
                // Promotion changes the row before periodic relocation. Never move a
                // mirror underneath an admitted reader, including a second authorization
                // pass which already owns a read guard. Refuse after a short wait instead of
                // nesting the same mutex or waiting behind a long clone.
                let guards = tokio::time::timeout(MOVE_WAIT, moving(&primary, &copy))
                    .await
                    .map_err(|_| unavailable())?;
                let current =
                    codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo.id)
                        .await
                        .map_err(|_| unavailable())?
                        .ok_or_else(unavailable)?;
                if current.storage_node_id != Some(self.context.node_id) {
                    return Err(RepoError::storage_node_unavailable());
                }
                let target = primary.clone();
                tokio::task::spawn_blocking(move || {
                    let _guards = guards;
                    if !target.exists() {
                        std::fs::rename(&copy, &target)?;
                    }
                    Ok::<_, std::io::Error>(())
                })
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
            }
            return Ok((Some(repo.id), primary));
        }
        if read_only
            && codeyard_db_cluster::cluster::replica_applied_version(
                pool,
                repo.id,
                self.context.node_id,
            )
            .await
            .map_err(|_| unavailable())?
            .is_some_and(|version| version >= 0)
        {
            let copy = copy_path(&self.context.repos_path, uuid);
            if !copy.exists() {
                return Err(unavailable());
            }
            return Ok((Some(repo.id), copy));
        }
        Err(RepoError::storage_node_unavailable())
    }

    /// CI may request its pinned objects after a force-push made them unreachable.
    /// Public Git sessions keep ordinary upload-pack reachability checks.
    pub(super) async fn checkout_access(
        &self,
        request: &Request,
        repo_id: Option<i64>,
        caller: iroh::PublicKey,
    ) -> Result<bool, RepoError> {
        let (Some(repo_id), Some(run)) = (repo_id, request.ci_run.as_ref()) else {
            return Ok(false);
        };
        if !matches!(&request.operation, Operation::GitSession(session) if session.service == GitService::UploadPack)
        {
            return Ok(false);
        }
        codeyard_db_actions::ci::ci_run_allows_repository_read(
            self.context.database()?,
            run.id,
            repo_id,
            &run.worker,
            &caller.to_string(),
        )
        .await
        .map_err(|_| unavailable())
    }

    pub(super) async fn authorize_request(
        &self,
        request: &Request,
        caller: iroh::PublicKey,
    ) -> Result<(RepoStore, Option<i64>), RepoError> {
        let pool = self.context.database()?;
        let serving = codeyard_db_cluster::cluster::get_node_by_id(pool, self.context.node_id)
            .await
            .map_err(|_| unavailable())?
            .is_some_and(|node| {
                node.status == "active"
                    && node.public_key == self.context.key.public_hex()
                    && node.roles.iter().any(|role| role == "storage")
            });
        if !serving {
            return Err(unavailable());
        }
        let node = codeyard_db_cluster::cluster::get_node_by_public_key(pool, &caller.to_string())
            .await
            .map_err(|_| unavailable())?
            .filter(|node| node.status == "active")
            .ok_or_else(|| invalid("not an active cluster member"))?;
        let operation =
            authorize_operation(&node.roles, &request.operation).inspect_err(|error| {
                tracing::warn!(caller = %node.name, %error, "repository RPC refused");
            })?;
        if requires_actor(&request.operation) && request.actor.is_none() {
            tracing::warn!(operation, caller = %node.name, "repository RPC refused: mutation has no actor");
            return Err(invalid("repository mutation requires an actor"));
        }
        if matches!(
            request.operation,
            Operation::PrepareWrite { .. }
                | Operation::ApplyWrite { .. }
                | Operation::WritePack { .. }
        ) {
            return self.write_admission(request, node.id).await;
        }
        if matches!(
            request.operation,
            Operation::Initialize { .. }
                | Operation::InitializeCopy { .. }
                | Operation::InitializeForeignCopy { .. }
        ) && codeyard_db_cluster::cluster::writes::write_settings(pool)
            .await
            .map_err(|_| unavailable())?
            .write_quorum
            .is_some()
        {
            let row = codeyard_db_cluster::cluster::get_repository_by_uuid(
                pool,
                &request.repository.to_string(),
            )
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
            if row.storage_node_id != Some(self.context.node_id) {
                let view = codeyard_db_cluster::cluster::writes::recovery_view(pool, row.id)
                    .await
                    .map_err(|_| unavailable())?;
                if !matches!(
                    request.operation,
                    Operation::Initialize { source: None, .. }
                ) || view.primary_id != node.id
                    || view.serving
                    || view.term != 1
                    || view.committed_version != 0
                    || !view
                        .voters
                        .iter()
                        .any(|voter| voter.node_id == self.context.node_id)
                {
                    return Err(unavailable());
                }
            } else if !node.roles.iter().any(|role| role == "web" || role == "ssh") {
                return Err(invalid(
                    "repository initialization requires a web or ssh node",
                ));
            }
            return Ok((
                RepoStore::for_initialization(&row, &self.context)?,
                Some(row.id),
            ));
        }
        if matches!(request.operation, Operation::ReplicaPack { .. })
            && node.roles.iter().any(|role| role == "storage")
            && codeyard_db_cluster::cluster::writes::write_settings(pool)
                .await
                .map_err(|_| unavailable())?
                .write_quorum
                .is_some()
        {
            let repo = codeyard_db_cluster::cluster::get_repository_by_uuid(
                pool,
                &request.repository.to_string(),
            )
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
            let view = codeyard_db_cluster::cluster::writes::recovery_view(pool, repo.id)
                .await
                .map_err(|_| unavailable())?;
            if !view.serving {
                let current = view.voters.iter().any(|voter| {
                    voter.node_id == self.context.node_id
                        && voter.term == view.term
                        && voter.applied_version == view.committed_version
                });
                let caller_voter = view
                    .voters
                    .iter()
                    .any(|voter| voter.node_id == node.id && voter.term == view.term);
                if !current || !caller_voter {
                    return Err(unavailable());
                }
                let path = codeyard_repos_core::context::term_path(
                    &self.context.repos_path,
                    request.repository,
                    view.term,
                );
                codeyard_repos_core::context::literal_directory(&self.context.repos_path, &path)
                    .await?;
                return Ok((store_at(&path, self.context.node_id)?, Some(repo.id)));
            }
        }
        let (repo_id, directory) = self
            .check_repository(
                request.repository,
                matches!(request.operation, Operation::Remove | Operation::Retire),
                request.operation.is_read_only(),
            )
            .await?;
        // Recheck at the serving node, not just when choosing a fallback: a write may
        // have completed while the request was waiting for a connection or read guard.
        if request.operation.is_read_only()
            && directory == copy_path(&self.context.repos_path, request.repository)
            && !codeyard_db_cluster::cluster::replica_is_current(
                pool,
                repo_id.ok_or_else(unavailable)?,
                self.context.node_id,
            )
            .await
            .map_err(|_| unavailable())?
        {
            return Err(RepoError::storage_node_unavailable());
        }
        if matches!(request.operation, Operation::ReplicaPack { .. })
            && !node.roles.iter().any(|role| role == "web" || role == "ssh")
        {
            let copy_held = match repo_id {
                Some(repo_id) => {
                    codeyard_db_cluster::cluster::replica_applied_version(pool, repo_id, node.id)
                        .await
                        .map_err(|_| unavailable())?
                        .is_some()
                        || codeyard_db_cluster::cluster::writes::recovery_view(pool, repo_id)
                            .await
                            .is_ok_and(|view| {
                                view.voters.iter().any(|voter| voter.node_id == node.id)
                            })
                }
                None => false,
            };
            if !copy_held {
                tracing::warn!(caller = %node.name, "repository RPC refused: no copy on the caller");
                return Err(invalid("the calling node holds no copy of this repository"));
            }
        } else if !node.roles.iter().any(|role| role == "web" || role == "ssh") {
            let claimed = match (repo_id, request.ci_run.as_ref()) {
                (Some(repo_id), Some(run)) => {
                    codeyard_db_actions::ci::ci_run_allows_repository_read(
                        pool,
                        run.id,
                        repo_id,
                        &run.worker,
                        &node.public_key,
                    )
                    .await
                    .map_err(|_| unavailable())?
                }
                _ => false,
            };
            if !claimed {
                tracing::warn!(operation, caller = %node.name, "repository RPC refused: no running CI claim");
                return Err(invalid("repository RPC requires a running CI claim for this repository and worker node"));
            }
        }
        if let Operation::ForkPack {
            authority,
            default_branch_only,
            branch,
            publisher,
        } = &request.operation
        {
            // A publisher export names one branch and carries no credential; a fork copy
            // always carries its initiating credential.
            if request.actor != Some(authority.user_id)
                || authority.credential.is_some() == *publisher
                || (*publisher && (*default_branch_only || branch.is_none()))
            {
                return Err(invalid("Fork credential actor mismatch"));
            }
            codeyard_db_repos::repositories::creation::read_repository_source(
                pool,
                authority,
                repo_id.ok_or_else(unavailable)?,
            )
            .await
            .map_err(RepoError::from)?
            .ok_or_else(unavailable)?;
        }
        if let Operation::GitSession(session) = &request.operation {
            let repo = codeyard_db_repos::repositories::records::get_repository_by_id(
                pool,
                repo_id.ok_or_else(unavailable)?,
            )
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
            if let Some(credential) = session.credential {
                // A receive admits a maintainer of a pull head; its hooks check each update.
                credential
                    .authorize_session(
                        pool,
                        &repo,
                        session.service
                            == codeyard_repos_core::store_types::GitService::ReceivePack,
                    )
                    .await
                    .map_err(|_| {
                        RepoError::new(
                            codeyard_git::error::ErrorKind::Unavailable,
                            "Git credential or repository access denied",
                        )
                    })?;
            } else if request.ci_run.is_none() && repo.is_private {
                return Err(invalid("Git credential required"));
            }
        }
        Ok((
            self.admitted_store(repo_id, &directory, request.operation.is_read_only())
                .await?,
            repo_id,
        ))
    }

    async fn write_admission(
        &self,
        request: &Request,
        caller: i64,
    ) -> Result<(RepoStore, Option<i64>), RepoError> {
        use codeyard_db_cluster::cluster::writes as journal;
        let pool = self.context.database()?;
        let id = match &request.operation {
            Operation::PrepareWrite { id }
            | Operation::ApplyWrite { id }
            | Operation::WritePack { id } => id,
            _ => return Err(invalid("write operation required")),
        };
        journal::validate_id(id).map_err(|_| invalid("invalid write id"))?;
        let write = journal::load_write(pool, id)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        let repo = codeyard_db_cluster::cluster::get_repository_by_uuid(
            pool,
            &request.repository.to_string(),
        )
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
        let view = journal::recovery_view(pool, repo.id)
            .await
            .map_err(|_| unavailable())?;
        if repo.id != write.repository_id
            || view.term != write.term
            || view.primary_id != write.primary_id
            || !view
                .voters
                .iter()
                .any(|voter| voter.node_id == caller && voter.term == write.term)
            || !view
                .voters
                .iter()
                .any(|voter| voter.node_id == self.context.node_id && voter.term == write.term)
            || caller == self.context.node_id
        {
            return Err(unavailable());
        }
        let pack = matches!(request.operation, Operation::WritePack { .. });
        if (pack && (self.context.node_id != write.primary_id || caller == write.primary_id))
            || (!pack && (caller != write.primary_id || self.context.node_id == write.primary_id))
        {
            return Err(invalid("write RPC requires the assigned primary and voter"));
        }
        if matches!(request.operation, Operation::PrepareWrite { .. })
            && (!view.serving || write.decision != "pending")
        {
            return Err(unavailable());
        }
        if pack && write.decision == "abort" {
            return Err(unavailable());
        }
        if matches!(request.operation, Operation::ApplyWrite { .. }) && write.decision == "pending"
        {
            return Err(unavailable());
        }
        // Write RPCs must reach a voter whose applied version is still the batch's base.
        // Ordinary serving admission correctly refuses that copy after the SQL decision.
        let local = codeyard_db_cluster::cluster::get_node_by_id(pool, self.context.node_id)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        if local.status != "active"
            || local.public_key != self.context.key.public_hex()
            || !local.roles.iter().any(|role| role == "storage")
        {
            return Err(unavailable());
        }
        let path = codeyard_repos_core::context::term_path(
            &self.context.repos_path,
            request.repository,
            write.term,
        );
        codeyard_repos_core::context::literal_directory(&self.context.repos_path, &path).await?;
        Ok((store_at(&path, self.context.node_id)?, Some(repo.id)))
    }

    /// Performs the admission checks needed before accepting an editor body.
    /// `execute` repeats live authorization after the complete body arrives.
    pub(super) async fn authorize_body_request(
        &self,
        request: &Request,
        caller: iroh::PublicKey,
    ) -> Result<(), RepoError> {
        if request.operation.is_object() {
            self.object_admission(request, caller).await?;
            return Ok(());
        }
        let (_, repo_id) = self.authorize_request(request, caller).await?;
        if !matches!(
            &request.operation,
            Operation::SaveFile { .. }
                | Operation::WriteGitObject { .. }
                | Operation::ImportGit { .. }
        ) {
            return Ok(());
        }
        let actor = request
            .actor
            .ok_or_else(|| invalid("repository mutation requires an actor"))?;
        let pool = self.context.database()?;
        let (repository, user) = codeyard_identity::repository_access::check_write(
            pool,
            repo_id.ok_or_else(|| invalid("repository unavailable"))?,
            actor,
        )
        .await
        .map_err(|error| match error {
            codeyard_identity::repository_access::AccessError::Unavailable => {
                RepoError::authority_unavailable()
            }
            _ => RepoError::write_denied(),
        })?;
        let Operation::SaveFile { write, .. } = &request.operation else {
            return Ok(());
        };
        let old = (write.expected_revision != "unborn").then_some(write.expected_revision.as_str());
        let branch_write = BranchWrite {
            branch: &write.branch,
            old,
            new: Some(ANY_TIP),
            via: WriteVia::Direct,
        };
        match check_branch_write(
            pool,
            &repository,
            &user,
            &branch_write,
            &History::SingleCommit,
        )
        .await
        {
            Ok(_) => {}
            Err(Refusal::Blocked(_)) => return Err(RepoError::protected_branch()),
            Err(Refusal::Unavailable) => return Err(RepoError::authority_unavailable()),
        }
        Ok(())
    }

    pub(super) async fn read_admission(
        &self,
        request: &Request,
        caller: iroh::PublicKey,
    ) -> Result<ReadAdmission, RepoError> {
        if !request.operation.is_read_only() {
            return Err(invalid("read admission requires a read-only operation"));
        }
        let (store, _) = self.authorize_request(request, caller).await?;
        let guard = codeyard_git::locks::read(Path::new(store.location())).await;
        let (current, repo_id) = self.authorize_request(request, caller).await?;
        if current.location() != store.location() {
            return Err(RepoError::storage_node_unavailable());
        }
        Ok(ReadAdmission {
            store,
            repo_id,
            guard,
        })
    }

    pub(super) async fn execute(
        &self,
        request: Request,
        body: Vec<u8>,
        caller: iroh::PublicKey,
    ) -> Result<(Value, Vec<u8>), Failure> {
        if request.operation.is_object() {
            return self
                .execute_object(request, body, caller)
                .await
                .map_err(Into::into);
        }
        let received = std::time::Instant::now();
        // Gist reads own the same lock internally because primary reads may reconcile
        // a pending gist write. Replica reads never perform that reconciliation.
        let gist_read = matches!(
            &request.operation,
            Operation::GistSnapshot { .. } | Operation::GistHistory { .. }
        );
        let (store, repo_id, _read_guard) = if request.operation.is_read_only() && !gist_read {
            let admitted = self.read_admission(&request, caller).await?;
            (admitted.store, admitted.repo_id, Some(admitted.guard))
        } else {
            let (store, repo_id) = self.authorize_request(&request, caller).await?;
            (store, repo_id, None)
        };
        let read_authority = request.operation.is_read_only().then(|| request.clone());
        let pool = self.context.database()?;
        let actor = request.actor;
        let mut output = Vec::new();
        macro_rules! json {
            ($call:expr) => {
                serde_json::to_value($call.await?)
                    .map_err(|_| Failure::Repository(unavailable()))?
            };
        }
        let cached_languages = matches!(request.operation, Operation::PrimaryLanguages { .. });
        let value = match request.operation {
            Operation::Initialize {
                source,
                default_branch,
            } => match source {
                Some(source) => json!(self.initialize_copy(
                    &store,
                    repo_id,
                    actor.map(Into::into),
                    source,
                    &codeyard_repos_core::repositories::store::CopyOptions {
                        default_branch,
                        template: false,
                        default_branch_only: false
                    }
                )),
                None => json!(store.initialize(None, &default_branch)),
            },
            Operation::InitializeForeignCopy {
                mut admission,
                default_branch,
                default_branch_only,
            } => {
                if actor != Some(admission.actor.user_id) {
                    return Err(invalid("Fork actor mismatch").into());
                }
                admission
                    .bind_native_peer(&caller.to_string())
                    .map_err(RepoError::from)?;
                json!(store.initialize_foreign_copy(
                    pool,
                    repo_id.ok_or_else(unavailable)?,
                    &admission,
                    &codeyard_repos_core::repositories::store::CopyOptions {
                        default_branch,
                        template: false,
                        default_branch_only,
                    }
                ))
            }
            Operation::InitializeCopy {
                source,
                default_branch,
                template,
                default_branch_only,
            } => {
                let (authority, default_branch) = default_branch.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(self.initialize_copy(
                    &store,
                    repo_id,
                    Some(authority),
                    source,
                    &codeyard_repos_core::repositories::store::CopyOptions {
                        default_branch,
                        template,
                        default_branch_only,
                    }
                ))
            }
            Operation::Remove => json!(store.remove()),
            Operation::Retire => json!(store.retire()),
            Operation::GistSnapshot { revision } => json!(store.gist_snapshot(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                revision.as_deref()
            )),
            Operation::GistHistory {
                revision,
                offset,
                limit,
            } => json!(store.gist_history(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                revision.as_deref(),
                offset,
                limit
            )),
            Operation::GistWrite { credential } => {
                let input =
                    serde_json::from_slice(&body).map_err(|_| invalid("Invalid gist input"))?;
                json!(store.gist_write(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                    credential,
                    input
                ))
            }
            Operation::CompareForeignPullHead {
                mut admission,
                base,
                branch,
            } => {
                if actor != Some(admission.actor.user_id) {
                    return Err(invalid("Pull actor mismatch").into());
                }
                admission
                    .bind_native_peer(&caller.to_string())
                    .map_err(RepoError::from)?;
                json!(store.compare_foreign_pull_head(
                    pool,
                    repo_id.ok_or_else(unavailable)?,
                    &admission,
                    &base,
                    &branch
                ))
            }
            Operation::FetchForeignPullHead {
                mut admission,
                number,
                branch,
                head,
                expected_head,
            } => {
                if actor != Some(admission.actor.user_id) {
                    return Err(invalid("Pull actor mismatch").into());
                }
                admission
                    .bind_native_peer(&caller.to_string())
                    .map_err(RepoError::from)?;
                json!(store.fetch_foreign_pull_head(
                    pool,
                    repo_id.ok_or_else(unavailable)?,
                    &admission,
                    number,
                    &branch,
                    &head,
                    expected_head.as_deref()
                ))
            }
            Operation::FollowForeignPullHead {
                request,
                expected_head,
            } => {
                if actor != Some(request.publisher_id) {
                    return Err(invalid("Pull actor mismatch").into());
                }
                json!(store.follow_foreign_pull_head(
                    pool,
                    repo_id.ok_or_else(unavailable)?,
                    &request,
                    expected_head.as_deref()
                ))
            }
            Operation::FetchPullHead {
                token,
                source,
                number,
                head,
                expected_head,
            } => {
                let (source_repo_id, source_path) =
                    self.check_repository(source, false, false).await?;
                let source_repo_id =
                    source_repo_id.ok_or_else(|| invalid("repository unavailable"))?;
                let source = self
                    .admitted_store(Some(source_repo_id), &source_path, false)
                    .await?;
                json!(store.fetch_pull_head(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    source_repo_id,
                    codeyard_repos_core::store_types::PullHeadActor {
                        user: actor
                            .ok_or_else(|| invalid("repository mutation requires an actor"))?,
                        token,
                    },
                    &source,
                    number,
                    &head,
                    expected_head.as_deref()
                ))
            }
            Operation::UpdatePullBranch { source, request } => {
                let (source_repo_id, source_path) =
                    self.check_repository(source, false, false).await?;
                let source = self
                    .admitted_store(source_repo_id, &source_path, false)
                    .await?;
                json!(store.update_pull_branch_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                    (
                        source_repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                        &source
                    ),
                    &request
                ))
            }
            Operation::Exists => serde_json::json!(store.exists().await),
            Operation::SizeBytes => json!(store.size_bytes()),
            Operation::RefreshStorageSize => {
                json!(store.refresh_storage_size_now(pool, repo_id.ok_or_else(unavailable)?))
            }
            Operation::Refs | Operation::PrimaryRefs => json!(store.refs()),
            Operation::MatchingRefs { prefix } => json!(store.matching_refs(&prefix)),
            Operation::BranchPublication {
                repository_id,
                reference,
            } => {
                if repo_id != Some(repository_id) {
                    return Err(invalid("Repository identity changed").into());
                }
                json!(store.branch_publication(repository_id, &reference))
            }
            Operation::Info => json!(store.info()),
            Operation::CommitCount => json!(store.commit_count()),
            Operation::GitTree {
                revision,
                recursive,
                raw_json,
            } => encode_json(
                &store.git_tree(&revision, recursive).await?,
                raw_json,
                &mut output,
            )?,
            Operation::GitTag { sha, raw_json } => {
                encode_json(&store.git_tag(&sha).await?, raw_json, &mut output)?
            }
            Operation::GitObjectKind { sha } => json!(store.git_object_kind(&sha)),
            Operation::IntroducedBlobs { sha } => {
                encode_json(&store.introduced_blobs(&sha).await?, true, &mut output)?
            }
            Operation::UpdateReference {
                reference,
                sha,
                force,
            } => {
                let (authority, sha) = sha.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.update_reference_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &reference,
                    &sha,
                    force
                ))
            }
            Operation::ImportGit { import } => json!(store.import_git_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                import,
                body
            )),
            Operation::LockMigration { migration } => json!(store.lock_migration_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                migration
            )),
            Operation::MigrationArchive => {
                output = store
                    .migration_archive_as(
                        pool,
                        repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                        actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                    )
                    .await?;
                Value::Null
            }
            Operation::CancelImport => json!(store.cancel_import_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?
            )),
            Operation::WriteGitObject { input, token } => json!(store.write_git_object_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                token,
                input,
                &body
            )),
            Operation::Tree {
                git_ref,
                path,
                metadata_only,
            } => {
                if metadata_only {
                    json!(store.tree_metadata(&git_ref, &path))
                } else {
                    json!(store.tree(&git_ref, &path))
                }
            }
            Operation::SidebarTrees { git_ref, paths } => {
                json!(store.sidebar_trees(&git_ref, &paths))
            }
            Operation::TemplateTexts {
                revision,
                paths,
                raw_json,
            } => encode_json(
                &store.template_texts(&revision, &paths).await?,
                raw_json,
                &mut output,
            )?,
            Operation::WorkflowFiles { revision } => {
                let files = store.workflow_files(&revision).await?;
                let mut manifest = Vec::with_capacity(files.len());
                for file in files {
                    let bytes = file.content.into_bytes();
                    if bytes.len() > MAX_CI_WORKFLOW_BYTES
                        || output
                            .len()
                            .checked_add(bytes.len())
                            .is_none_or(|length| length > MAX_WORKFLOW_PAYLOAD_BYTES)
                    {
                        return Err(invalid("workflow payload exceeds its limit").into());
                    }
                    manifest.push(WorkflowPayloadEntry {
                        path: file.path,
                        length: bytes.len(),
                    });
                    output.extend_from_slice(&bytes);
                }
                serde_json::to_value(manifest).map_err(|_| Failure::Repository(unavailable()))?
            }
            Operation::Blob { git_ref, path } => {
                let mut blob = store.blob(&git_ref, &path).await?;
                output = std::mem::take(&mut blob.content).into_bytes();
                serde_json::to_value(blob).map_err(|_| Failure::Repository(unavailable()))?
            }
            Operation::BlobBytes {
                git_ref,
                path,
                range,
            } => blob_reply(store.blob_range(&git_ref, &path, range).await?, &mut output)?,
            Operation::BlobById { id } => {
                output = store.blob_by_id(&id).await?;
                Value::Null
            }
            Operation::BlobByIdRange { id, range } => {
                blob_reply(store.blob_by_id_range(&id, range).await?, &mut output)?
            }
            Operation::Commits {
                git_ref,
                limit,
                filter,
                walk_millis,
                raw_json,
            } => {
                let budget = walk_millis
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(codeyard_git::COMMIT_WALK_TIMEOUT)
                    .min(codeyard_git::COMMIT_WALK_TIMEOUT);
                let commits = store
                    .commits_until(&git_ref, 0, limit, &filter, received + budget)
                    .await?;
                encode_json(&commits, raw_json, &mut output)?
            }
            Operation::CommitPage {
                git_ref,
                offset,
                limit,
                filter,
                walk_millis,
                raw_json,
            } => {
                let budget = walk_millis
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(codeyard_git::COMMIT_WALK_TIMEOUT)
                    .min(codeyard_git::COMMIT_WALK_TIMEOUT);
                let commits = store
                    .commits_until(&git_ref, offset, limit, &filter, received + budget)
                    .await?;
                encode_json(&commits, raw_json, &mut output)?
            }
            Operation::SearchCode {
                git_ref,
                text,
                offset,
                limit,
                raw_json,
            } => {
                let matches = store.search_code(&git_ref, &text, offset, limit).await?;
                encode_json(&matches, raw_json, &mut output)?
            }
            Operation::PathCommits {
                git_ref,
                path,
                offset,
                limit,
                filter,
                raw_json,
            } => {
                let commits = store
                    .path_commits(&git_ref, &path, offset, limit, &filter)
                    .await?;
                encode_json(&commits, raw_json, &mut output)?
            }
            Operation::Commit { revision } | Operation::PrimaryCommit { revision } => {
                json!(store.commit(&revision))
            }
            Operation::ReleaseHistory {
                head,
                previous,
                exclude_tag,
            } => json!(store.release_history(&head, previous.as_deref(), &exclude_tag)),
            Operation::CommitDiff { revision } => json!(store.commit_diff(&revision)),
            Operation::Diff {
                base_ref,
                head_ref,
                stats,
            } => {
                if stats {
                    json!(store.diff_stats(&base_ref, &head_ref))
                } else {
                    json!(store.diff(&base_ref, &head_ref))
                }
            }
            Operation::CommitsBetween {
                base_ref,
                head_ref,
                offset,
                limit,
                raw_json,
                pull_request,
            } => {
                if pull_request {
                    let commits = store
                        .pull_request_commits(&base_ref, &head_ref, offset, limit)
                        .await?;
                    encode_json(&commits, raw_json, &mut output)?
                } else {
                    let commits = store
                        .commits_between_page(&base_ref, &head_ref, offset, limit)
                        .await?;
                    encode_json(&commits, raw_json, &mut output)?
                }
            }
            Operation::DiffFile {
                base_ref,
                head_ref,
                path,
            } => json!(store.diff_file(&base_ref, &head_ref, &path)),
            Operation::ChangedPaths {
                base_ref,
                head_ref,
                endpoints,
                renames,
            } => {
                if endpoints {
                    json!(store.push_changed_paths(&base_ref, &head_ref))
                } else if renames {
                    json!(store.diff_paths(&base_ref, &head_ref))
                } else {
                    json!(store.changed_paths(&base_ref, &head_ref))
                }
            }
            Operation::DiffFrom {
                source,
                base_ref,
                head_ref,
            } => {
                let (source_id, path) = self.check_repository(source, false, false).await?;
                let source = self.admitted_store(source_id, &path, false).await?;
                json!(store.diff_from(&source, &base_ref, &head_ref))
            }
            Operation::ComparisonFrom {
                source,
                base_ref,
                head_ref,
                offset,
                limit,
                raw_json,
            } => {
                let (source_id, path) = self.check_repository(source, false, false).await?;
                let source = self.admitted_store(source_id, &path, false).await?;
                let comparison = store
                    .comparison_from(&source, &base_ref, &head_ref, offset, limit)
                    .await?;
                encode_json(&comparison, raw_json, &mut output)?
            }
            Operation::Tags => json!(store.tags()),
            Operation::Archive {
                git_ref,
                prefix,
                format,
            } => {
                output = store.archive(&git_ref, &prefix, format).await?;
                Value::Null
            }
            Operation::Languages { git_ref } | Operation::PrimaryLanguages { git_ref } => {
                let repository = codeyard_db_repos::repositories::records::get_repository_by_id(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                )
                .await
                .map_err(|_| unavailable())?
                .ok_or_else(|| invalid("repository unavailable"))?;
                let default_branch = repository.default_branch.clone();
                // The cached computation follows the Git default itself, which an adopted
                // repository may name differently from the stored default.
                if repository.storage_node_id == Some(self.context.node_id)
                    && (cached_languages
                        || git_ref == default_branch
                        || git_ref == format!("refs/heads/{default_branch}"))
                {
                    json!(
                        codeyard_repos_core::repositories::languages::compute_and_store(
                            pool,
                            &store,
                            &repository,
                            &default_branch
                        )
                    )
                } else {
                    json!(store.languages(&git_ref))
                }
            }
            Operation::FilePaths {
                git_ref,
                limit,
                raw_json,
            } => {
                let (git_ref, query) = git_ref.into_parts();
                let paths = store.file_paths_matching(&git_ref, &query, limit).await?;
                encode_json(&paths, raw_json, &mut output)?
            }
            Operation::Blame { git_ref, path } => {
                output = serde_json::to_vec(&store.blame(&git_ref, &path).await?)
                    .map_err(|_| Failure::Repository(unavailable()))?;
                Value::Null
            }
            Operation::Contributors => json!(store.contributors()),
            Operation::BranchContributors { branch } => json!(store.branch_contributors(&branch)),
            Operation::CommitStatistics {
                branch,
                raw_json,
                mode,
            } => {
                let statistics = store.commit_statistics(&branch, mode).await?;
                encode_json(&statistics, raw_json, &mut output)?
            }
            Operation::CodeFrequency { branch } => json!(store.code_frequency(&branch)),
            Operation::IsAncestor {
                ancestor,
                descendant,
            } => json!(store.is_ancestor(&ancestor, &descendant)),
            Operation::BranchStatuses { base, heads } => {
                json!(store.branch_statuses(&base, &heads))
            }
            Operation::ResolveCiCommit {
                reference,
                sha,
                must_be_on_ref,
            } => json!(store.resolve_ci_commit(&reference, sha.as_deref(), must_be_on_ref)),
            Operation::CiWorkflowFile { sha, workflow_path } => {
                json!(store.ci_workflow_file(&sha, &workflow_path))
            }
            Operation::RenameBranch { old, new } => {
                let (authority, new) = new.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.rename_branch_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &old,
                    &new
                ))
            }
            Operation::MergeUpstream {
                source,
                branch,
                token,
            } => {
                let (source_id, path) = self.check_repository(source, false, false).await?;
                let from = self.admitted_store(source_id, &path, false).await?;
                json!(store.merge_upstream_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                    (
                        source_id.ok_or_else(|| invalid("repository unavailable"))?,
                        &from
                    ),
                    &branch,
                    token
                ))
            }
            Operation::CreateBranch { name, sha } => {
                let (authority, sha) = sha.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.create_branch_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &name,
                    &sha
                ))
            }
            Operation::SetDefaultBranch { name } => {
                let (authority, name) = name.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.set_default_branch_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &name
                ))
            }
            Operation::TransferRepository { new_owner } => {
                let (authority, new_owner) = new_owner.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.transfer_repository_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &new_owner
                ))
            }
            Operation::ConfigureDependencySecurity {
                credential,
                feature,
                enabled,
                private_consent,
            } => json!(store.configure_dependency_security_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                credential,
                feature,
                enabled,
                private_consent
            )),
            Operation::MutateRelease {
                mutation,
                attestation,
            } => json!(store.mutate_release_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                mutation,
                attestation
            )),
            Operation::PatchRepository {
                default_branch,
                changes,
            } => {
                let (authority, default_branch) = default_branch.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.patch_repository_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &default_branch,
                    &changes.borrowed()
                ))
            }
            Operation::DeleteBranch { name } => {
                let (authority, name) = name.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.delete_branch_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &name
                ))
            }
            Operation::CreateTag {
                name,
                sha,
                message,
                tagger_name,
                tagger_email,
            } => {
                let (authority, sha) = sha.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.create_tag_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &name,
                    &sha,
                    message.as_deref(),
                    &tagger_name,
                    &tagger_email
                ))
            }
            Operation::DeleteTag { name } => {
                let (authority, name) = name.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.delete_tag_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    &name
                ))
            }
            Operation::PrepareFile {
                write,
                author,
                email,
                identities,
            } => json!(store.prepare_file_with_identities(
                &write,
                &body,
                &author,
                &email,
                &identities
            )),
            Operation::PrepareUpload {
                write,
                author,
                email,
            } => json!(store.prepare_upload(&write, &body, &author, &email)),
            Operation::PublishFile { prepared } => json!(store.publish_file_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &prepared
            )),
            Operation::PublishDependencyUpdate {
                prepared,
                publication,
            } => json!(store.publish_dependency_update_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &prepared,
                &publication
            )),
            Operation::SaveFile {
                write,
                author,
                email,
            } => json!(store.save_file_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &write,
                &body,
                &author,
                &email
            )),
            Operation::Merge { request } => {
                let result = store.prepare_merge(&request).await.map_err(|_| {
                    Failure::Merge(codeyard_repos_core::store_types::MergeFailure::CannotMerge)
                })?;
                let repo_id = repo_id.ok_or_else(|| invalid("repository unavailable"))?;
                let repository =
                    codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo_id)
                        .await
                        .map_err(|_| RepoError::authority_unavailable())?
                        .ok_or_else(|| invalid("repository unavailable"))?;
                // Merging on a node is refused for any protected branch: the reviews and
                // checks that a merge needs are only judged where the pull request lives.
                if !matching_rules(pool, &repository, &request.base_branch)
                    .await
                    .map_err(|_| RepoError::authority_unavailable())?
                    .is_empty()
                {
                    return Err(Failure::Repository(RepoError::protected_branch()));
                }
                match store
                    .publish_merge_as(
                        pool,
                        repo_id,
                        actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                        &request.head_branch,
                        &request.base_branch,
                        &request.head,
                        &request.base,
                        &result,
                        request.method,
                        None,
                    )
                    .await
                {
                    Ok(()) => serde_json::json!(result),
                    Err(error)
                        if error.is_write_denied()
                            || error.is_protected_branch()
                            || error.is_authority_unavailable()
                            || error.is_storage_node_unavailable() =>
                    {
                        return Err(Failure::Repository(error));
                    }
                    Err(_) => {
                        return Err(Failure::Merge(
                            codeyard_repos_core::store_types::MergeFailure::BranchMoved,
                        ));
                    }
                }
            }
            Operation::PrepareMerge { request } => json!(store.prepare_merge(&request)),
            Operation::PrepareRevert { request } => json!(store.prepare_revert(&request)),
            Operation::PublishAsyncMerge { request } => json!(store.publish_async_merge_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &request
            )),
            Operation::PublishBranchMerge { publication } => json!(store.publish_branch_merge_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &publication
            )),
            Operation::PublishMerge {
                head_branch,
                base_branch,
                head,
                base,
                result,
                method,
                token,
                policy,
            } => json!(store.publish_merge_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                {
                    let actor =
                        actor.ok_or_else(|| invalid("repository mutation requires an actor"))?;
                    token
                        .map(|value| value.into_actor(actor))
                        .transpose()?
                        .unwrap_or_else(|| actor.into())
                },
                &head_branch,
                &base_branch,
                &head,
                &base,
                &result,
                method,
                policy
            )),
            Operation::PublishStackedMerge {
                heads,
                base_branch,
                base,
                result,
                token,
                policy,
            } => json!(store.publish_stacked_merge_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                {
                    let actor =
                        actor.ok_or_else(|| invalid("repository mutation requires an actor"))?;
                    token
                        .map(|value| value.into_actor(actor))
                        .transpose()?
                        .unwrap_or_else(|| actor.into())
                },
                &heads,
                &base_branch,
                &base,
                &result,
                policy,
            )),
            Operation::PrepareRestack {
                from,
                head,
                onto,
                name,
                email,
            } => json!(store.prepare_restack(&from, &head, &onto, &name, &email)),
            Operation::PublishRestack {
                stack_id,
                versions,
                base_branch,
                base,
                moves,
            } => {
                let (authority, base_branch) = base_branch.into_parts(
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                )?;
                json!(store.publish_restack_as(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    authority,
                    stack_id,
                    &versions,
                    &base_branch,
                    &base,
                    &moves
                ))
            }
            Operation::AdvertiseRefs { service } => {
                output = store.advertise_refs(service).await?;
                Value::Null
            }
            Operation::GitSession(_) => {
                return Err(invalid("Git session requires streaming").into())
            }
            Operation::PrepareWrite { id } => {
                let limits = codeyard_repos_core::git_transport::WriteLimits::load(pool)
                    .await
                    .map_err(RepoError::from)?;
                tokio::time::timeout(limits.timeout, self.prepare_write(&store, &id, limits))
                    .await
                    .map_err(|_| unavailable())?
                    .map_err(RepoError::from)?;
                Value::Null
            }
            Operation::ApplyWrite { id } => {
                let limits = codeyard_repos_core::git_transport::WriteLimits::load(pool)
                    .await
                    .map_err(RepoError::from)?;
                tokio::time::timeout(limits.timeout, self.apply_write(&store, &id, limits))
                    .await
                    .map_err(|_| unavailable())?
                    .map_err(RepoError::from)?;
                Value::Null
            }
            Operation::ObjectPut { .. }
            | Operation::ObjectGet { .. }
            | Operation::ObjectDelete { .. } => {
                return Err(invalid("object RPC requires object admission").into())
            }
            Operation::ForkPack { .. }
            | Operation::ReplicaPack { .. }
            | Operation::WritePack { .. } => {
                return Err(invalid("replica pack requires streaming").into())
            }
        };
        if output.len() > MAX_BYTES {
            return Err(invalid("RPC frame exceeds its limit").into());
        }
        if let Some(request) = read_authority {
            self.authorize_request(&request, caller).await?;
        }
        Ok((value, output))
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    #[test]
    fn raw_json_is_opt_in_for_same_generation_peers() {
        for operation in [
            r#"{"FilePaths":{"git_ref":"main","limit":10}}"#,
            r#"{"CommitsBetween":{"base_ref":"main","head_ref":"topic","limit":10}}"#,
        ] {
            let operation: Operation = serde_json::from_str(operation).unwrap();
            assert!(matches!(
                operation,
                Operation::FilePaths {
                    raw_json: false,
                    ..
                } | Operation::CommitsBetween {
                    raw_json: false,
                    ..
                }
            ));
        }
        let paths = vec!["one"];
        let mut body = Vec::new();
        assert_eq!(
            encode_json(&paths, false, &mut body).unwrap(),
            serde_json::json!(paths)
        );
        assert!(body.is_empty());
        assert_eq!(encode_json(&paths, true, &mut body).unwrap(), Value::Null);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            serde_json::json!(paths)
        );
    }
}
