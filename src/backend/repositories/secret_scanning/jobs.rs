//! Durable scan work survives process restarts; Git reads go through the repository store.
use super::{scan_with_patterns, MAX_BLOB_BYTES};
use crate::backend::{
    app::AppState,
    db::{self, secret_scanning as records},
    repositories::store::repo_store,
    webhooks::{self, payloads},
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc, time::Duration};

pub async fn document(
    state: &AppState,
    alert: &records::Alert,
    owner: &str,
    repo: &str,
    hide_secret: bool,
) -> Result<Value> {
    let secret = if hide_secret {
        String::new()
    } else {
        let encryptor = state
            .secret_encryptor
            .as_ref()
            .context("Secret encryption is unavailable")?;
        String::from_utf8(encryptor.decrypt(&alert.encrypted_secret, &alert.nonce)?)?
    };
    let resolved_by = match alert.resolved_by {
        Some(id) => db::users::accounts::get_user_by_id(&state.pool, id).await?,
        None => None,
    };
    let bypass = records::alert_bypass(&state.pool, alert.repo_id, &alert.secret_hash).await?;
    let bypassed_by = match &bypass {
        Some(bypass) => db::users::accounts::get_user_by_id(&state.pool, bypass.actor_id).await?,
        None => None,
    };
    let mut document = payloads::secret_scanning_alert(
        alert,
        &secret,
        owner,
        repo,
        resolved_by.as_ref(),
        bypass.as_ref(),
        bypassed_by.as_ref(),
    );
    let assignee = match alert.assigned_to {
        Some(id) => db::users::accounts::get_user_by_id(&state.pool, id).await?,
        None => None,
    };
    document["assigned_to"] = assignee.as_ref().map(payloads::user).unwrap_or(Value::Null);
    Ok(document)
}

pub async fn emit_alert(
    state: &AppState,
    repo: &db::models::Repository,
    alert: &records::Alert,
    action: &'static str,
    actor: i64,
) -> Result<()> {
    let owner = db::repositories::records::repository_owner_name(&state.pool, repo.id)
        .await?
        .context("Repository owner unavailable")?;
    let document = document(state, alert, &owner, &repo.name, true).await?;
    let repository = payloads::repository(repo, &owner);
    webhooks::emit(
        &state.pool,
        repo,
        "secret_scanning_alert",
        vec![actor],
        move |_, people| json!({"action": action,"alert": document,"repository": repository,"sender":payloads::person(&people.get(actor))}),
    )
    .await;
    Ok(())
}

async fn scan_job(state: &AppState, job: &records::ScanJob) -> Result<()> {
    let repo = db::repositories::records::get_repository_by_id(&state.pool, job.repo_id)
        .await?
        .context("Repository unavailable")?;
    let encryptor = state
        .secret_encryptor
        .as_ref()
        .context("Secret encryption unavailable")?;
    let store = repo_store(&repo, state.cluster.as_ref());
    let (patterns, _) = records::effective_patterns(&state.pool, repo.id).await?;
    let patterns = Arc::new(patterns);
    let mut pending = match &job.head_sha {
        Some(head) => vec![store.commit(head).await?.sha],
        None => {
            let mut heads = Vec::new();
            for sha in store.refs_from_primary().await?.into_values() {
                // Only a ref that peels to a commit has history. A tag, or a chain of tags, may
                // end at a tree or a blob.
                let (mut object, mut kind) = (sha.clone(), store.git_object_kind(&sha).await?);
                while kind == "tag" {
                    let tag = store.git_tag(&object).await?;
                    (object, kind) = (tag.object, tag.kind);
                }
                if kind == "commit" {
                    heads.push(object);
                }
            }
            heads
        }
    };
    let mut visited = HashSet::new();
    // The claim lapses after five minutes without a heartbeat, so a long commit or a long run of
    // scanned commits renews it on the way.
    let mut beat = std::time::Instant::now();
    let renew = |beat: &mut std::time::Instant| {
        let due = beat.elapsed() > std::time::Duration::from_secs(60);
        if due {
            *beat = std::time::Instant::now();
        }
        due
    };
    while let Some(sha) = pending.pop() {
        if !visited.insert(sha.clone()) {
            continue;
        }
        if renew(&mut beat) {
            ensure!(
                records::heartbeat(&state.pool, repo.id, job.id, job.attempt, None).await?,
                "Another worker took over this secret scan"
            );
        }
        if !records::settings(&state.pool, repo.id).await?.enabled {
            return Ok(());
        }
        let commit = store.commit(&sha).await?;
        pending.extend(commit.parents);
        if job.scan_type == "incremental" && records::scanned(&state.pool, repo.id, &sha).await? {
            continue;
        }
        // Each directory is listed on its own, so only a single directory over the tree limit
        // stops the scan.
        let mut directories = vec![(sha.clone(), String::new())];
        while let Some((directory, prefix)) = directories.pop() {
            let tree = store.git_tree(&directory, false).await?;
            ensure!(!tree.truncated, "Secret scan tree exceeds the tree limit");
            for mut entry in tree.tree {
                entry.path = format!("{prefix}{}", entry.path);
                if entry.kind == "tree" {
                    directories.push((entry.sha, format!("{}/", entry.path)));
                    continue;
                }
                if entry.kind != "blob" || entry.size.is_some_and(|size| size > MAX_BLOB_BYTES) {
                    continue;
                }
                if renew(&mut beat) {
                    ensure!(
                        records::heartbeat(&state.pool, repo.id, job.id, job.attempt, None).await?,
                        "Another worker took over this secret scan"
                    );
                }
                let blob = store.blob_bytes(&sha, &entry.path).await?;
                let path = entry.path.clone();
                let commit_sha = sha.clone();
                let patterns = patterns.clone();
                let detections = tokio::task::spawn_blocking(move || {
                    scan_with_patterns(&path, &commit_sha, &blob.bytes, &patterns, false)
                })
                .await??;
                for detection in detections {
                    let location = records::Location {
                        commit_sha: sha.clone(),
                        blob_sha: entry.sha.clone(),
                        path: entry.path.clone(),
                        start_line: detection.start_line,
                        end_line: detection.end_line,
                        start_column: detection.start_column,
                        end_column: detection.end_column,
                    };
                    let hash = crate::backend::identity::tokens::token_hash(&detection.secret);
                    let (ciphertext, nonce) = encryptor.encrypt(detection.secret.as_bytes())?;
                    let stored = records::record(
                        &state.pool,
                        repo.id,
                        &records::Finding {
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
                            if let Some(actor) = job.actor_id {
                                emit_alert(state, &repo, &alert, "created", actor).await?;
                            }
                        }
                        if let Some(actor) = job.actor_id.filter(|_| added) {
                            let owner = db::repositories::records::repository_owner_name(
                                &state.pool,
                                repo.id,
                            )
                            .await?
                            .context("Repository owner unavailable")?;
                            let document =
                                document(state, &alert, &owner, &repo.name, true).await?;
                            let repository = payloads::repository(&repo, &owner);
                            let location =
                                payloads::secret_scanning_location(&location, &owner, &repo.name);
                            webhooks::emit(&state.pool, &repo, "secret_scanning_alert_location", vec![actor], move |_, people| json!({"action":"created","alert":document,"location":location,"repository":repository,"sender":payloads::person(&people.get(actor))})).await;
                        }
                    }
                }
            }
        }
        beat = std::time::Instant::now();
        ensure!(
            records::heartbeat(&state.pool, repo.id, job.id, job.attempt, Some(&sha)).await?,
            "Another worker took over this secret scan"
        );
    }
    Ok(())
}

/// Runs one queued scan. Failed scans are recorded as failed, never reported as completed.
pub async fn run_next(state: &AppState) -> Result<bool> {
    let Some(job) = records::claim(&state.pool).await? else {
        return Ok(false);
    };
    let result = scan_job(state, &job).await;
    records::finish(&state.pool, job.id, job.attempt, result.is_ok()).await?;
    result?;
    Ok(true)
}

pub async fn run(state: Arc<AppState>, mut stop: tokio::sync::watch::Receiver<bool>) {
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
