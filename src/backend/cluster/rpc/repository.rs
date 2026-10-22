use super::{wire::*, RpcHandler};
use crate::backend::{
    cluster::replicas::copy_path,
    db,
    repositories::{
        access,
        protection::{
            check_branch_write, matching_rules, BranchWrite, History, Refusal, WriteVia, ANY_TIP,
        },
        store::{
            BlobMetadata, GitService, RepoError, RepoStore, WorkflowPayloadEntry,
            MAX_CI_WORKFLOW_BYTES, MAX_WORKFLOW_PAYLOAD_BYTES,
        },
    },
};
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
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
        Operation::Remove => ("Remove", false),
        Operation::FetchPullHead { .. } => ("FetchPullHead", false),
        Operation::Exists => ("Exists", false),
        Operation::SizeBytes => ("SizeBytes", false),
        Operation::Refs => ("Refs", false),
        Operation::PrimaryRefs => ("PrimaryRefs", false),
        Operation::Info => ("Info", false),
        Operation::CommitCount => ("CommitCount", false),
        Operation::Tree { .. } => ("Tree", false),
        Operation::SidebarTrees { .. } => ("SidebarTrees", false),
        Operation::WorkflowFiles { .. } => ("WorkflowFiles", false),
        Operation::TemplateTexts { .. } => ("TemplateTexts", false),
        Operation::Blob { .. } => ("Blob", false),
        Operation::BlobBytes { .. } => ("BlobBytes", false),
        Operation::BlobById { .. } => ("BlobById", true),
        Operation::Commits { .. } => ("Commits", false),
        Operation::CommitPage { .. } => ("CommitPage", false),
        Operation::SearchCode { .. } => ("SearchCode", false),
        Operation::PathCommits { .. } => ("PathCommits", false),
        Operation::Commit { .. } => ("Commit", false),
        Operation::PrimaryCommit { .. } => ("PrimaryCommit", false),
        Operation::CommitDiff { .. } => ("CommitDiff", false),
        Operation::Diff { .. } => ("Diff", false),
        Operation::CommitsBetween { .. } => ("CommitsBetween", false),
        Operation::DiffFile { .. } => ("DiffFile", false),
        Operation::ChangedPaths { .. } => ("ChangedPaths", false),
        Operation::DiffFrom { .. } => ("DiffFrom", false),
        Operation::Tags => ("Tags", false),
        Operation::Archive { .. } => ("Archive", false),
        Operation::Languages { .. } => ("Languages", false),
        Operation::FilePaths { .. } => ("FilePaths", false),
        Operation::Blame { .. } => ("Blame", false),
        Operation::Contributors => ("Contributors", false),
        Operation::CodeFrequency { .. } => ("CodeFrequency", false),
        Operation::IsAncestor { .. } => ("IsAncestor", false),
        Operation::BranchStatuses { .. } => ("BranchStatuses", false),
        Operation::ResolveCiCommit { .. } => ("ResolveCiCommit", false),
        Operation::CiWorkflowFile { .. } => ("CiWorkflowFile", false),
        Operation::CreateBranch { .. } => ("CreateBranch", false),
        Operation::DeleteBranch { .. } => ("DeleteBranch", false),
        Operation::SetDefaultBranch { .. } => ("SetDefaultBranch", false),
        Operation::CreateTag { .. } => ("CreateTag", false),
        Operation::DeleteTag { .. } => ("DeleteTag", false),
        Operation::PrepareFile { .. } => ("PrepareFile", false),
        Operation::PrepareUpload { .. } => ("PrepareUpload", false),
        Operation::PublishFile { .. } => ("PublishFile", false),
        Operation::SaveFile { .. } => ("SaveFile", false),
        Operation::Merge { .. } => ("Merge", false),
        Operation::PrepareMerge { .. } => ("PrepareMerge", false),
        Operation::PublishMerge { .. } => ("PublishMerge", false),
        Operation::PublishStackedMerge { .. } => ("PublishStackedMerge", false),
        Operation::PrepareRestack { .. } => ("PrepareRestack", false),
        Operation::PublishRestack { .. } => ("PublishRestack", false),
        Operation::AdvertiseRefs { .. } => ("AdvertiseRefs", false),
        Operation::GitSession(session) => ("GitSession", session.service == GitService::UploadPack),
        Operation::ReplicaPack { .. } => ("ReplicaPack", false),
    };
    // A copy holder asks only for packs of its own copies; authorize_request checks the copy.
    if matches!(operation, Operation::ReplicaPack { .. }) {
        return if roles.iter().any(|role| role == "storage") {
            Ok(name)
        } else {
            Err(invalid(
                "repository RPC ReplicaPack requires a storage node",
            ))
        };
    }
    if roles.iter().any(|role| role == "web" || role == "ssh")
        || (worker_read && roles.iter().any(|role| role == "actions-worker"))
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
        } | Operation::FetchPullHead { .. }
            | Operation::CreateBranch { .. }
            | Operation::DeleteBranch { .. }
            | Operation::SetDefaultBranch { .. }
            | Operation::CreateTag { .. }
            | Operation::DeleteTag { .. }
            | Operation::PublishFile { .. }
            | Operation::SaveFile { .. }
            | Operation::Merge { .. }
            | Operation::PublishMerge { .. }
            | Operation::PublishStackedMerge { .. }
            | Operation::PublishRestack { .. }
    )
}

const NOT_STORED: &str = "repository is not stored on this node";

fn store_at(path: &Path, node_id: i64) -> Result<RepoStore, RepoError> {
    Ok(RepoStore::on_node(
        path.to_str().ok_or_else(unavailable)?,
        node_id,
    ))
}

/// Whether `node` holds a copy row for the repository.
async fn holds_copy(pool: &db::DbPool, repository: i64, node: i64) -> Result<bool, RepoError> {
    Ok(db::cluster::list_replicas(pool, repository)
        .await
        .map_err(|_| unavailable())?
        .iter()
        .any(|(holder, _)| *holder == node))
}

impl RpcHandler {
    fn primary_path(&self, uuid: Uuid) -> PathBuf {
        self.context.repos_path.join(format!("{uuid}.git"))
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
        let repository = db::cluster::get_repository_by_uuid(pool, &uuid.to_string())
            .await
            .map_err(|_| unavailable())?;
        let primary = self.primary_path(uuid);
        let Some(repo) = repository else {
            // Deletion removes the row before its directory.
            if removing {
                return Ok((None, primary));
            }
            return Err(invalid(NOT_STORED));
        };
        // A repository from before the cluster has no node; it is on the node that
        // has its directory.
        let primary_here = repo.storage_node_id == Some(self.context.node_id)
            || (repo.storage_node_id.is_none() && primary.exists());
        if primary_here {
            let copy = copy_path(&self.context.repos_path, uuid);
            if !primary.exists() && copy.exists() {
                // Promotion changes the row before the periodic replica pass relocates Git.
                if tokio::fs::rename(&copy, &primary).await.is_err() && !primary.exists() {
                    return Err(unavailable());
                }
            }
            return Ok((Some(repo.id), primary));
        }
        if read_only && holds_copy(pool, repo.id, self.context.node_id).await? {
            let copy = copy_path(&self.context.repos_path, uuid);
            if !copy.exists() {
                return Err(unavailable());
            }
            return Ok((Some(repo.id), copy));
        }
        Err(invalid(NOT_STORED))
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
        db::ci::ci_run_allows_repository_read(
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
        let serving = db::cluster::get_node_by_id(pool, self.context.node_id)
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
        let node = db::cluster::get_node_by_public_key(pool, &caller.to_string())
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
        let (repo_id, directory) = self
            .check_repository(
                request.repository,
                matches!(request.operation, Operation::Remove),
                request.operation.is_read_only(),
            )
            .await?;
        if matches!(request.operation, Operation::ReplicaPack { .. }) {
            let copy_held = match repo_id {
                Some(repo_id) => holds_copy(pool, repo_id, node.id).await?,
                None => false,
            };
            if !copy_held {
                tracing::warn!(caller = %node.name, "repository RPC refused: no copy on the caller");
                return Err(invalid("the calling node holds no copy of this repository"));
            }
        } else if !node.roles.iter().any(|role| role == "web" || role == "ssh") {
            let claimed = match (repo_id, request.ci_run.as_ref()) {
                (Some(repo_id), Some(run)) => db::ci::ci_run_allows_repository_read(
                    pool,
                    run.id,
                    repo_id,
                    &run.worker,
                    &node.public_key,
                )
                .await
                .map_err(|_| unavailable())?,
                _ => false,
            };
            if !claimed {
                tracing::warn!(operation, caller = %node.name, "repository RPC refused: no running CI claim");
                return Err(invalid("repository RPC requires a running CI claim for this repository and worker node"));
            }
        }
        if let Operation::GitSession(session) = &request.operation {
            let repo = db::repositories::records::get_repository_by_id(
                pool,
                repo_id.ok_or_else(unavailable)?,
            )
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
            if let Some(credential) = session.credential {
                credential
                    .authorize(
                        pool,
                        &repo,
                        session.service
                            == crate::backend::repositories::store::GitService::ReceivePack,
                    )
                    .await
                    .map_err(|_| {
                        RepoError::new(
                            crate::backend::repositories::store::ErrorKind::Unavailable,
                            "Git credential or repository access denied",
                        )
                    })?;
            } else if request.ci_run.is_none() && repo.is_private {
                return Err(invalid("Git credential required"));
            }
        }
        Ok((store_at(&directory, self.context.node_id)?, repo_id))
    }

    /// Performs the admission checks needed before accepting an editor body.
    /// `execute` repeats live authorization after the complete body arrives.
    pub(super) async fn authorize_body_request(
        &self,
        request: &Request,
        caller: iroh::PublicKey,
    ) -> Result<(), RepoError> {
        let (_, repo_id) = self.authorize_request(request, caller).await?;
        let Operation::SaveFile { write, .. } = &request.operation else {
            return Ok(());
        };
        let actor = request
            .actor
            .ok_or_else(|| invalid("repository mutation requires an actor"))?;
        let pool = self.context.database()?;
        let (repository, user) = access::check_write(
            pool,
            repo_id.ok_or_else(|| invalid("repository unavailable"))?,
            actor,
        )
        .await
        .map_err(|error| match error {
            access::AccessError::Unavailable => RepoError::authority_unavailable(),
            _ => RepoError::write_denied(),
        })?;
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

    pub(super) async fn execute(
        &self,
        request: Request,
        body: Vec<u8>,
        caller: iroh::PublicKey,
    ) -> Result<(Value, Vec<u8>), Failure> {
        let received = std::time::Instant::now();
        let (store, repo_id) = self.authorize_request(&request, caller).await?;
        let pool = self.context.database()?;
        let actor = request.actor;
        let mut output = Vec::new();
        macro_rules! json {
            ($call:expr) => {
                serde_json::to_value($call.await?)
                    .map_err(|_| Failure::Repository(unavailable()))?
            };
        }
        let value = match request.operation {
            Operation::Initialize {
                source,
                default_branch,
            } => match source {
                Some(uuid) => {
                    let (source_repo_id, path) = self.check_repository(uuid, false, false).await?;
                    let source = store_at(&path, self.context.node_id)?;
                    json!(store.initialize_fork(
                        pool,
                        repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                        source_repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                        actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                        &source,
                        &default_branch
                    ))
                }
                None => json!(store.initialize(None, &default_branch)),
            },
            Operation::Remove => json!(store.remove()),
            Operation::FetchPullHead {
                source,
                number,
                head,
                expected_head,
            } => {
                let (source_repo_id, _) = self.check_repository(source, false, false).await?;
                let source_repo_id =
                    source_repo_id.ok_or_else(|| invalid("repository unavailable"))?;
                let source = store_at(&self.primary_path(source), self.context.node_id)?;
                json!(store.fetch_pull_head(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                    source_repo_id,
                    actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                    &source,
                    number,
                    &head,
                    expected_head.as_deref()
                ))
            }
            Operation::Exists => serde_json::json!(store.exists().await),
            Operation::SizeBytes => json!(store.size_bytes()),
            Operation::Refs | Operation::PrimaryRefs => json!(store.refs()),
            Operation::Info => json!(store.info()),
            Operation::CommitCount => json!(store.commit_count()),
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
            Operation::TemplateTexts { revision, paths } => {
                json!(store.template_texts(&revision, &paths))
            }
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
            } => {
                let blob = store.blob_range(&git_ref, &path, range).await?;
                output = blob.bytes;
                serde_json::to_value(BlobMetadata {
                    id: blob.id,
                    is_binary: blob.is_binary,
                    size: blob.size,
                    offset: blob.offset,
                    mode: blob.mode,
                })
                .map_err(|_| Failure::Repository(unavailable()))?
            }
            Operation::BlobById { id } => {
                output = store.blob_by_id(&id).await?;
                Value::Null
            }
            Operation::Commits {
                git_ref,
                limit,
                filter,
                walk_millis,
            } => {
                let budget = walk_millis
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(crate::backend::git::COMMIT_WALK_TIMEOUT)
                    .min(crate::backend::git::COMMIT_WALK_TIMEOUT);
                json!(store.commits_until(&git_ref, 0, limit, &filter, received + budget))
            }
            Operation::CommitPage {
                git_ref,
                offset,
                limit,
                filter,
                walk_millis,
            } => {
                let budget = walk_millis
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(crate::backend::git::COMMIT_WALK_TIMEOUT)
                    .min(crate::backend::git::COMMIT_WALK_TIMEOUT);
                json!(store.commits_until(&git_ref, offset, limit, &filter, received + budget))
            }
            Operation::SearchCode {
                git_ref,
                text,
                offset,
                limit,
            } => json!(store.search_code(&git_ref, &text, offset, limit)),
            Operation::PathCommits {
                git_ref,
                path,
                offset,
                limit,
                filter,
            } => json!(store.path_commits(&git_ref, &path, offset, limit, &filter)),
            Operation::Commit { revision } | Operation::PrimaryCommit { revision } => {
                json!(store.commit(&revision))
            }
            Operation::CommitDiff { revision } => json!(store.commit_diff(&revision)),
            Operation::Diff { base_ref, head_ref } => json!(store.diff(&base_ref, &head_ref)),
            Operation::CommitsBetween {
                base_ref,
                head_ref,
                limit,
                raw_json,
            } => {
                let commits = store.commits_between(&base_ref, &head_ref, limit).await?;
                encode_json(&commits, raw_json, &mut output)?
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
                self.check_repository(source, false, false).await?;
                let source = store_at(&self.primary_path(source), self.context.node_id)?;
                json!(store.diff_from(&source, &base_ref, &head_ref))
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
            Operation::Languages { git_ref } => {
                let repository = db::repositories::records::get_repository_by_id(
                    pool,
                    repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                )
                .await
                .map_err(|_| unavailable())?
                .ok_or_else(|| invalid("repository unavailable"))?;
                let default_branch = store.info().await?.default_branch;
                if repository.storage_node_id == Some(self.context.node_id)
                    && (git_ref == default_branch
                        || git_ref == format!("refs/heads/{default_branch}"))
                {
                    json!(crate::backend::repositories::languages::compute_and_store(
                        pool,
                        &store,
                        &repository,
                        &default_branch
                    ))
                } else {
                    json!(store.languages(&git_ref))
                }
            }
            Operation::FilePaths {
                git_ref,
                limit,
                raw_json,
            } => {
                let paths = store.file_paths(&git_ref, limit).await?;
                encode_json(&paths, raw_json, &mut output)?
            }
            Operation::Blame { git_ref, path } => {
                output = serde_json::to_vec(&store.blame(&git_ref, &path).await?)
                    .map_err(|_| Failure::Repository(unavailable()))?;
                Value::Null
            }
            Operation::Contributors => json!(store.contributors()),
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
            Operation::CreateBranch { name, sha } => json!(store.create_branch_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &name,
                &sha
            )),
            Operation::SetDefaultBranch { name } => json!(store.set_default_branch_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &name
            )),
            Operation::DeleteBranch { name } => json!(store.delete_branch_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &name
            )),
            Operation::CreateTag {
                name,
                sha,
                message,
                tagger_name,
                tagger_email,
            } => json!(store.create_tag_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &name,
                &sha,
                message.as_deref(),
                &tagger_name,
                &tagger_email
            )),
            Operation::DeleteTag { name } => json!(store.delete_tag_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &name
            )),
            Operation::PrepareFile {
                write,
                author,
                email,
            } => json!(store.prepare_file(&write, &body, &author, &email)),
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
                    Failure::Merge(crate::backend::repositories::store::MergeFailure::CannotMerge)
                })?;
                let repo_id = repo_id.ok_or_else(|| invalid("repository unavailable"))?;
                let repository = db::repositories::records::get_repository_by_id(pool, repo_id)
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
                            crate::backend::repositories::store::MergeFailure::BranchMoved,
                        ));
                    }
                }
            }
            Operation::PrepareMerge { request } => json!(store.prepare_merge(&request)),
            Operation::PublishMerge {
                head_branch,
                base_branch,
                head,
                base,
                result,
                method,
            } => json!(store.publish_merge_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &head_branch,
                &base_branch,
                &head,
                &base,
                &result,
                method
            )),
            Operation::PublishStackedMerge {
                heads,
                base_branch,
                base,
                result,
            } => json!(store.publish_stacked_merge_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                &heads,
                &base_branch,
                &base,
                &result,
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
            } => json!(store.publish_restack_as(
                pool,
                repo_id.ok_or_else(|| invalid("repository unavailable"))?,
                actor.ok_or_else(|| invalid("repository mutation requires an actor"))?,
                stack_id,
                &versions,
                &base_branch,
                &base,
                &moves
            )),
            Operation::AdvertiseRefs { service } => {
                output = store.advertise_refs(service).await?;
                Value::Null
            }
            Operation::GitSession(_) => {
                return Err(invalid("Git session requires streaming").into())
            }
            Operation::ReplicaPack { .. } => {
                return Err(invalid("replica pack requires streaming").into())
            }
        };
        if output.len() > MAX_BYTES {
            return Err(invalid("RPC frame exceeds its limit").into());
        }
        Ok((value, output))
    }
}

pub fn repository_uuid(path: &str) -> Result<Uuid, RepoError> {
    let filename = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("invalid repository UUID"))?;
    let uuid = filename
        .strip_suffix(".git")
        .ok_or_else(|| invalid("invalid repository UUID"))?;
    Uuid::parse_str(uuid).map_err(|_| invalid("invalid repository UUID"))
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
