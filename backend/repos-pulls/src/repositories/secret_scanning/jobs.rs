//! Durable scan work survives process restarts; Git reads go through the repository store.
use crate::services::RepositoryServices;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use codeyard_repos_core::repositories::store::repo_store;
use codeyard_support::secret_detection::scan_with_patterns;
use codeyard_support::secret_detection::MAX_BLOB_BYTES;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

pub async fn document(
    state: &dyn RepositoryServices,
    alert: &codeyard_db_security::secret_scanning::Alert,
    owner: &str,
    repo: &str,
    hide_secret: bool,
) -> Result<Value> {
    Documents::load(state, std::slice::from_ref(alert))
        .await?
        .document(state, alert, owner, repo, hide_secret)
}

/// Page enrichment has one bounded database read per related record kind.
pub struct Documents {
    users: HashMap<i64, codeyard_db_core::models::User>,
    bypasses: HashMap<(i64, String), codeyard_db_security::secret_scanning::Bypass>,
}
impl Documents {
    pub async fn load(
        state: &dyn RepositoryServices,
        alerts: &[codeyard_db_security::secret_scanning::Alert],
    ) -> Result<Self> {
        ensure!(alerts.len() <= 100, "Too many alert enrichments");
        let keys = alerts
            .iter()
            .map(|alert| (alert.repo_id, alert.secret_hash.clone()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let bypasses =
            codeyard_db_security::secret_scanning::alert_bypasses(state.pool(), &keys).await?;
        let ids = alerts
            .iter()
            .flat_map(|alert| [alert.resolved_by, alert.assigned_to])
            .flatten()
            .chain(bypasses.iter().map(|bypass| bypass.actor_id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let users = codeyard_db_identity::users::accounts::get_users_by_ids(state.pool(), &ids)
            .await?
            .into_iter()
            .map(|user| (user.id, user))
            .collect();
        Ok(Self {
            users,
            bypasses: bypasses
                .into_iter()
                .map(|bypass| ((bypass.repo_id, bypass.secret_hash.clone()), bypass))
                .collect(),
        })
    }

    pub fn document(
        &self,
        state: &dyn RepositoryServices,
        alert: &codeyard_db_security::secret_scanning::Alert,
        owner: &str,
        repo: &str,
        hide_secret: bool,
    ) -> Result<Value> {
        let secret = if hide_secret {
            String::new()
        } else {
            let encryptor = state
                .secret_encryptor()
                .as_ref()
                .context("Secret encryption is unavailable")?;
            String::from_utf8(encryptor.decrypt(&alert.encrypted_secret, &alert.nonce)?)?
        };
        let resolved_by = alert.resolved_by.and_then(|id| self.users.get(&id));
        let bypass = self
            .bypasses
            .get(&(alert.repo_id, alert.secret_hash.clone()));
        let bypassed_by = bypass.and_then(|bypass| self.users.get(&bypass.actor_id));
        let mut document = codeyard_webhooks::payloads::secret_scanning_alert(
            alert,
            &secret,
            owner,
            repo,
            resolved_by,
            bypass,
            bypassed_by,
        );
        document["assigned_to"] = alert
            .assigned_to
            .and_then(|id| self.users.get(&id))
            .map(codeyard_webhooks::payloads::user)
            .unwrap_or(Value::Null);
        Ok(document)
    }
}

pub async fn emit_alert(
    state: &dyn RepositoryServices,
    repo: &codeyard_db_core::models::Repository,
    alert: &codeyard_db_security::secret_scanning::Alert,
    action: &'static str,
    actor: impl Into<Option<i64>>,
) -> Result<()> {
    let actor = actor.into();
    let owner =
        codeyard_db_repos::repositories::records::repository_owner_name(state.pool(), repo.id)
            .await?
            .context("Repository owner unavailable")?;
    let document = document(state, alert, &owner, &repo.name, true).await?;
    let repository = codeyard_webhooks::payloads::repository(repo, &owner);
    codeyard_repos_core::webhooks::emit(
        state.pool(),
        repo,
        "secret_scanning_alert",
        actor.into_iter().collect(),
        move |_, people| json!({"action": action,"alert": document,"repository": repository,"sender":actor.map(|actor| codeyard_webhooks::payloads::person(&people.get(actor))).unwrap_or(Value::Null)}),
    )
    .await;
    Ok(())
}

async fn commit_target(
    store: &codeyard_repos_core::repositories::store::RepoStore,
    sha: &str,
) -> Result<Option<String>> {
    let mut object = sha.to_owned();
    let mut seen = HashSet::new();
    loop {
        ensure!(
            seen.len() < 32 && seen.insert(object.clone()),
            "Secret scan tag chain exceeds its limit"
        );
        match store.git_object_kind(&object).await?.as_str() {
            "commit" => return Ok(Some(object)),
            "tree" | "blob" => return Ok(None),
            "tag" => object = store.git_tag(&object).await?.object,
            _ => anyhow::bail!("Unsupported secret scan target"),
        }
    }
}

async fn scan_job(
    state: &dyn RepositoryServices,
    job: &codeyard_db_security::secret_scanning::ScanJob,
) -> Result<()> {
    let repo =
        codeyard_db_repos::repositories::records::get_repository_by_id(state.pool(), job.repo_id)
            .await?
            .context("Repository unavailable")?;
    let encryptor = state
        .secret_encryptor()
        .as_ref()
        .context("Secret encryption unavailable")?;
    let store = repo_store(&repo, state.cluster());
    let (patterns, _) =
        codeyard_db_security::secret_scanning::effective_patterns(state.pool(), repo.id).await?;
    let patterns = Arc::new(patterns);
    let heads: Vec<String> = match &job.head_sha {
        Some(head) => commit_target(&store, head).await?.into_iter().collect(),
        None => {
            let mut heads = Vec::new();
            for sha in store.refs_from_primary().await?.into_values() {
                if let Some(commit) = commit_target(&store, &sha).await? {
                    heads.push(commit);
                }
            }
            heads
        }
    };
    let mut pending = heads
        .into_iter()
        .map(|sha| (sha, false))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    let cacheable = codeyard_support::secret_detection::scanner::cache_across_commits()?;
    let mut cache = std::collections::HashMap::<
        (String, String),
        Arc<Vec<codeyard_support::secret_detection::Detection>>,
    >::new();
    let mut cache_bytes = 0usize;
    // The claim lapses after five minutes without a heartbeat, so a long commit or a long run of
    // scanned commits renews it on the way.
    let mut beat = std::time::Instant::now();
    let renew = |beat: &mut std::time::Instant| {
        let due = beat.elapsed() > HEARTBEAT_INTERVAL;
        if due {
            *beat = std::time::Instant::now();
        }
        due
    };
    while let Some((sha, finishing)) = pending.pop() {
        if finishing {
            ensure!(
                codeyard_db_security::secret_scanning::heartbeat(
                    state.pool(),
                    repo.id,
                    job.id,
                    job.attempt,
                    Some(&sha)
                )
                .await?,
                "Another worker took over this secret scan"
            );
            beat = std::time::Instant::now();
            continue;
        }
        if !visited.insert(sha.clone()) {
            continue;
        }
        if renew(&mut beat) {
            ensure!(
                codeyard_db_security::secret_scanning::heartbeat(
                    state.pool(),
                    repo.id,
                    job.id,
                    job.attempt,
                    None
                )
                .await?,
                "Another worker took over this secret scan"
            );
        }
        if !codeyard_db_security::secret_scanning::settings(state.pool(), repo.id)
            .await?
            .enabled
        {
            return Ok(());
        }
        if job.scan_type == "incremental"
            && codeyard_db_security::secret_scanning::scanned(state.pool(), repo.id, &sha).await?
        {
            continue;
        }
        let commit = store.commit(&sha).await?;
        // First-parent changes introduce a blob once; root commits introduce the whole tree.
        // The ancestry walk still visits every parent, including side branches of merges.
        for entry in store.introduced_blobs(&sha).await? {
            if entry.kind != "blob" || entry.size.is_some_and(|size| size > MAX_BLOB_BYTES) {
                continue;
            }
            if renew(&mut beat) {
                ensure!(
                    codeyard_db_security::secret_scanning::heartbeat(
                        state.pool(),
                        repo.id,
                        job.id,
                        job.attempt,
                        None
                    )
                    .await?,
                    "Another worker took over this secret scan"
                );
            }
            let key = (entry.path.clone(), entry.sha.clone());
            let detections = if let Some(detections) = cache.get(&key) {
                detections.clone()
            } else {
                // Paths may be non-UTF-8; object identity, not the display path, selects bytes.
                let bytes = store.blob_by_id(&entry.sha).await?;
                let path = entry.path.clone();
                let commit_sha = sha.clone();
                let patterns = patterns.clone();
                let detections = Arc::new(
                    tokio::task::spawn_blocking(move || {
                        scan_with_patterns(&path, &commit_sha, &bytes, &patterns, false)
                    })
                    .await??,
                );
                if cacheable {
                    let bytes = detections
                        .iter()
                        .map(|detection| detection.secret.len())
                        .sum::<usize>()
                        + key.0.len()
                        + key.1.len();
                    if cache.len() >= 1024 || cache_bytes.saturating_add(bytes) > 8 * 1024 * 1024 {
                        cache.clear();
                        cache_bytes = 0;
                    }
                    cache_bytes += bytes;
                    cache.insert(key, detections.clone());
                }
                detections
            };
            for detection in detections.iter() {
                let location = codeyard_git::secret_scanning::Location {
                    commit_sha: sha.clone(),
                    blob_sha: entry.sha.clone(),
                    path: entry.path.clone(),
                    start_line: detection.start_line,
                    end_line: detection.end_line,
                    start_column: detection.start_column,
                    end_column: detection.end_column,
                };
                let hash = codeyard_support::token_hash::token_hash(&detection.secret);
                let (ciphertext, nonce) = encryptor.encrypt(detection.secret.as_bytes())?;
                let stored = codeyard_db_security::secret_scanning::record(
                    state.pool(),
                    repo.id,
                    &codeyard_db_security::secret_scanning::Finding {
                        hash: &hash,
                        secret_type: &detection.secret_type,
                        display_name: &detection.secret_type_display_name,
                        encrypted_secret: &ciphertext,
                        nonce: &nonce,
                        location: &location,
                    },
                )
                .await?;
                if let Some((alert, created, added)) = stored {
                    if created {
                        emit_alert(state, &repo, &alert, "created", job.actor_id).await?;
                    }
                    if added {
                        let actor = job.actor_id;
                        let owner =
                            codeyard_db_repos::repositories::records::repository_owner_name(
                                state.pool(),
                                repo.id,
                            )
                            .await?
                            .context("Repository owner unavailable")?;
                        let document = document(state, &alert, &owner, &repo.name, true).await?;
                        let repository = codeyard_webhooks::payloads::repository(&repo, &owner);
                        let location = codeyard_webhooks::payloads::secret_scanning_location(
                            &location, &owner, &repo.name,
                        );
                        codeyard_repos_core::webhooks::emit(state.pool(), &repo, "secret_scanning_alert_location", actor.into_iter().collect(), move |_, people| json!({"action":"created","alert":document,"location":location,"repository":repository,"sender":actor.map(|actor| codeyard_webhooks::payloads::person(&people.get(actor))).unwrap_or(Value::Null)})).await;
                    }
                }
            }
        }
        // A persistent marker means this commit AND its ancestors completed, so pruning a
        // later incremental walk cannot strand work after cancellation or a storage failure.
        pending.push((sha, true));
        pending.extend(commit.parents.into_iter().map(|sha| (sha, false)));
    }
    Ok(())
}

/// Runs one queued scan. Failed scans are recorded as failed, never reported as completed.
pub async fn run_next(state: &dyn RepositoryServices) -> Result<bool> {
    let Some(job) = codeyard_db_security::secret_scanning::claim(state.pool()).await? else {
        return Ok(false);
    };
    // Renew while initial ref enumeration, tag peeling or any individual storage read waits,
    // not only after traversal has yielded another scannable entry.
    let result = async {
        let scan = scan_job(state, &job);
        tokio::pin!(scan);
        let mut beats = tokio::time::interval(HEARTBEAT_INTERVAL);
        beats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                result = &mut scan => break result,
                _ = beats.tick() => {
                    let renewed = tokio::time::timeout(Duration::from_secs(5), codeyard_db_security::secret_scanning::heartbeat(state.pool(), job.repo_id, job.id, job.attempt, None)).await
                        .map_err(|_| codeyard_git::error::RepoError::new(codeyard_git::error::ErrorKind::Unavailable, "Secret scan lease renewal unavailable"))??;
                    ensure!(renewed, "Another worker took over this secret scan");
                }
            }
        }
    }.await;
    let completion = match &result {
        Ok(()) => codeyard_db_security::secret_scanning::ScanCompletion::Completed,
        Err(error)
            if error
                .downcast_ref::<codeyard_git::error::RepoError>()
                .is_some_and(|error| error.kind == codeyard_git::error::ErrorKind::Unavailable)
                || error.is::<sqlx::Error>() =>
        {
            codeyard_db_security::secret_scanning::ScanCompletion::Retry
        }
        Err(_) => codeyard_db_security::secret_scanning::ScanCompletion::Failed,
    };
    codeyard_db_security::secret_scanning::finish(state.pool(), job.id, job.attempt, completion)
        .await?;
    result?;
    Ok(true)
}

pub async fn run(
    state: Arc<impl RepositoryServices + 'static>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            result = run_next(&state) => match result {
                Ok(true) => continue,
                Ok(false) => {},
                Err(_) => tracing::warn!("Secret scanning job failed"),
            }
        }
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}
