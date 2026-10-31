//! Admission snapshots called workflows so workers never fetch a moving ref or another repository.
use super::admission::{is_safe_ref, is_safe_workflow_path, AdmissionError};
use crate::backend::{
    db,
    repositories::store::{repo_store, ErrorKind},
};
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn authorize(
    pool: &db::DbPool,
    caller: &db::models::Repository,
    actor: i64,
    called: &db::models::Repository,
) -> Result<(), AdmissionError> {
    if caller.id == called.id || !called.is_private {
        return Ok(());
    }
    // Without a cross-repository sharing setting, private workflows stay within a private namespace.
    if !caller.is_private
        || caller.owner_kind != called.owner_kind
        || caller.owner_user_id != called.owner_user_id
        || caller.owner_org_id != called.owner_org_id
        || !db::repositories::access::get_user_repo_access(pool, actor, called.id)
            .await?
            .can_read()
    {
        return Err(AdmissionError::Denied("Called workflow access denied"));
    }
    // The called repository's Actions access setting decides whether other repositories in its
    // namespace may call it. As on GitHub, an unset setting shares nothing.
    let (called_settings, _) = db::action_settings::effective(pool, called.id).await?;
    let access = db::action_settings::Setting::Access;
    let level = called_settings
        .get(access.key())
        .unwrap_or(&access.defaults())
        .get("access_level")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("none")
        .to_owned();
    if !matches!(level.as_str(), "user" | "organization") {
        return Err(AdmissionError::Denied("Called workflow access denied"));
    }
    Ok(())
}

pub(super) struct Resolver<'a> {
    pub pool: &'a db::DbPool,
    pub context: Option<&'a crate::backend::cluster::NodeContext>,
    pub caller: &'a db::models::Repository,
    pub actor: i64,
    pub files: BTreeMap<String, serde_json::Value>,
    pub environment: Option<String>,
    pub scheduling: db::runners::Scheduling,
    active: BTreeSet<String>,
    called: BTreeSet<String>,
    /// Each called ref's commit and the full ref name OIDC claims carry.
    refs: BTreeMap<(i64, String), (String, String)>,
    pinned: BTreeMap<(i64, String, String), (String, String)>,
    snapshots: BTreeMap<String, (serde_json::Value, usize)>,
    snapshot_bytes: usize,
}

/// Called-workflow commits a run's stored snapshots pin, keyed by repository, path and the ref
/// the call named, so calls of one file at two refs keep their own commits.
pub(super) fn pinned_commits(
    blobs: &serde_json::Value,
) -> BTreeMap<(i64, String, String), (String, String)> {
    blobs
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(path, _)| path.starts_with(".github/workflows/codeyard-call-"))
        .filter_map(|(_, snapshot)| {
            let called_ref = snapshot.get("ref")?.as_str()?;
            let full_ref = snapshot
                .get("full_ref")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(called_ref);
            Some((
                (
                    snapshot.get("repository_id")?.as_i64()?,
                    snapshot.get("source_path")?.as_str()?.to_owned(),
                    called_ref.to_owned(),
                ),
                (
                    snapshot.get("sha")?.as_str()?.to_owned(),
                    full_ref.to_owned(),
                ),
            ))
        })
        .collect()
}

impl<'a> Resolver<'a> {
    pub fn new(
        pool: &'a db::DbPool,
        context: Option<&'a crate::backend::cluster::NodeContext>,
        caller: &'a db::models::Repository,
        actor: i64,
        environment: Option<String>,
        pinned: BTreeMap<(i64, String, String), (String, String)>,
    ) -> Self {
        Self {
            pool,
            context,
            caller,
            actor,
            files: BTreeMap::new(),
            environment,
            scheduling: db::runners::Scheduling::default(),
            active: BTreeSet::new(),
            called: BTreeSet::new(),
            refs: BTreeMap::new(),
            pinned,
            snapshots: BTreeMap::new(),
            snapshot_bytes: 0,
        }
    }

    #[expect(clippy::too_many_arguments, reason = "A called workflow's snapshot takes its source, contents, depth and calling ref.")]
    pub fn snapshot<'b>(
        &'b mut self,
        repository: db::models::Repository,
        sha: String,
        path: String,
        content: String,
        yaml: Option<Value>,
        depth: usize,
        // The ref this workflow was called at, as written and as a full ref name. None for a
        // root, which runs at the run's ref.
        called_ref: Option<(String, String)>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, AdmissionError>> + Send + 'b,
        >,
    > {
        Box::pin(async move {
            // The ref is part of the identity, so calls at two refs of one commit keep their own pins.
            let identity = format!(
                "{}:{sha}:{path}:{}",
                repository.id,
                called_ref.as_ref().map_or("", |(name, _)| name.as_str())
            );
            if depth >= codeyard_actions::workflow::MAX_REUSABLE_LEVELS || !self.active.insert(identity.clone()) {
                return Err(AdmissionError::Invalid(
                    "Called workflows exceed four levels or contain a cycle",
                ));
            }
            if let Some((snapshot, height)) = self.snapshots.get(&identity) {
                if depth + height >= codeyard_actions::workflow::MAX_REUSABLE_LEVELS {
                    return Err(AdmissionError::Invalid("Called workflows exceed four levels"));
                }
                self.active.remove(&identity);
                return Ok(snapshot.clone());
            }
            let mut yaml = if let Some(yaml) = yaml {
                yaml
            } else {
                let parsed = super::admission::parse_admission_workflow(path.clone(), content, self.environment.clone()).await?;
                codeyard_actions::workflow::validate_called_concurrency(&parsed.yaml)
                    .map_err(|_| AdmissionError::Invalid("Called workflow concurrency is unsupported"))?;
                super::admission::merge_scheduling(&mut self.scheduling, parsed.scheduling)?;
                self.environment = parsed.environment;
                parsed.yaml
            };
            if depth > 0 {
                self.called.insert(identity.clone());
                if self.called.len() > codeyard_actions::workflow::MAX_CALLED_WORKFLOWS {
                    return Err(AdmissionError::Invalid("More than twenty called workflows"));
                }
                if !codeyard_actions::workflow::events::declares_event(yaml.get("on"), "workflow_call") {
                    return Err(AdmissionError::Invalid(
                        "Called workflow does not declare workflow_call",
                    ));
                }
            }
            let mut height = 0;
            let jobs = yaml
                .get_mut("jobs")
                .and_then(Value::as_mapping_mut)
                .ok_or(AdmissionError::Invalid("Called workflow needs jobs"))?;
            for job in jobs.values_mut() {
                let Some(reference) = job.get("uses").and_then(Value::as_str).map(str::to_owned)
                else {
                    continue;
                };
                let (target, target_sha, target_path, target_ref) = if let Some(path) =
                    reference.strip_prefix("./")
                {
                    // A local call runs at the ref its containing workflow was called at.
                    (
                        repository.clone(),
                        sha.clone(),
                        path.to_owned(),
                        called_ref.clone(),
                    )
                } else {
                    let (source, reference) = reference
                        .split_once('@')
                        .ok_or(AdmissionError::Invalid("Invalid called workflow reference"))?;
                    let mut parts = source.splitn(3, '/');
                    let owner = parts.next().unwrap_or("");
                    let name = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");
                    if !super::admission::is_safe_owner(owner)
                        || crate::backend::repositories::lifecycle::validate_name(name).is_err()
                        || !is_safe_workflow_path(path)
                        || !(is_safe_ref(reference)
                            || crate::backend::git::is_full_commit_id(reference))
                    {
                        return Err(AdmissionError::Invalid("Invalid called workflow reference"));
                    }
                    let target = db::repositories::records::get_repository(self.pool, owner, name)
                        .await?
                        .ok_or(AdmissionError::Denied("Called workflow access denied"))?;
                    authorize(self.pool, self.caller, self.actor, &target).await?;
                    let key = (target.id, reference.to_owned());
                    let (target_sha, full_ref) = if let Some(found) = self.refs.get(&key) {
                        found.clone()
                    } else if let Some(found) =
                        self.pinned
                            .get(&(target.id, path.to_owned(), reference.to_owned()))
                    {
                        found.clone()
                    } else {
                        let store = repo_store(&target, self.context);
                        let full_sha = crate::backend::git::is_full_commit_id(reference);
                        // GitHub resolves a called workflow's tag before a branch of the same name.
                        let tagged = if full_sha || reference.starts_with("refs/") {
                            None
                        } else {
                            match store
                                .resolve_ci_commit(&format!("refs/tags/{reference}"), None, false)
                                .await
                            {
                                Ok(commit) => Some(commit),
                                Err(error) if error.kind == ErrorKind::NotFound => None,
                                Err(error) => return Err(error.into()),
                            }
                        };
                        let commit = match tagged {
                            Some(commit) => commit,
                            None => {
                                store
                                    .resolve_ci_commit(
                                        reference,
                                        full_sha.then_some(reference),
                                        false,
                                    )
                                    .await?
                            }
                        };
                        // GitHub names a commit pin by its SHA and a ref by its full name.
                        let full_ref = if full_sha {
                            commit.sha.clone()
                        } else {
                            commit.full_ref
                        };
                        self.refs
                            .insert(key, (commit.sha.clone(), full_ref.clone()));
                        (commit.sha, full_ref)
                    };
                    (
                        target,
                        target_sha,
                        path.to_owned(),
                        Some((reference.to_owned(), full_ref)),
                    )
                };
                if !is_safe_workflow_path(&target_path) {
                    return Err(AdmissionError::Invalid("Invalid called workflow path"));
                }
                // Check local calls too: a local call inside a remote workflow still belongs to that repository.
                authorize(self.pool, self.caller, self.actor, &target).await?;
                let key = format!(
                    "{}:{target_sha}:{target_path}:{}",
                    target.id,
                    target_ref.as_ref().map_or("", |(name, _)| name.as_str())
                );
                let file_name = format!(
                    ".github/workflows/codeyard-call-{:x}.yml",
                    Sha256::digest(key.as_bytes())
                );
                let mut snapshot = if let Some((snapshot, height)) = self.snapshots.get(&key) {
                    if self.active.contains(&key) || depth + 1 + height >= codeyard_actions::workflow::MAX_REUSABLE_LEVELS {
                        return Err(AdmissionError::Invalid("Called workflows exceed four levels or contain a cycle"));
                    }
                    snapshot.clone()
                } else {
                    let file = repo_store(&target, self.context).ci_workflow_file(&target_sha, &target_path).await?;
                    self.snapshot(target, target_sha, target_path, file.content, None, depth + 1, target_ref.clone()).await?
                };
                // The ref a call names is stored with its snapshot, so a called job's OIDC identity can name it.
                if let (Some((called_ref, full_ref)), Some(stored)) =
                    (target_ref, snapshot.as_object_mut())
                {
                    stored.insert("ref".into(), called_ref.into());
                    stored.insert("full_ref".into(), full_ref.into());
                }
                height = height.max(self.snapshots[&key].1 + 1);
                self.files.insert(file_name.clone(), snapshot);
                job.as_mapping_mut()
                    .ok_or(AdmissionError::Invalid("Invalid called job"))?
                    .insert("uses".into(), format!("./{file_name}").into());
            }
            self.active.remove(&identity);
            let content = serde_yaml::to_string(&yaml).map_err(|_| AdmissionError::Unavailable)?;
            self.snapshot_bytes = self.snapshot_bytes.saturating_add(content.len());
            if self.snapshot_bytes > 2 * 1024 * 1024 {
                return Err(AdmissionError::Invalid("Called workflow snapshots exceed size limit"));
            }
            let stored = serde_json::json!({
                "content": content,
                "repository_id": repository.id, "sha": sha, "source_path": path,
            });
            self.snapshots.insert(identity, (stored.clone(), height));
            Ok(stored)
        })
    }
}

pub(super) fn content(value: &serde_json::Value) -> Option<&str> {
    value.get("content").and_then(serde_json::Value::as_str)
}
