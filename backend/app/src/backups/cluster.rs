//! Cluster backups reuse replica streams. Restore keeps nodes and voters fenced until installed.
use super::{private_file, reject_symlink, sync_dir, Places, REPOSITORIES_DIR, UPLOADS_DIR};
use anyhow::{ensure, Context, Result};
use codeyard_database::{models::RefValue, DbPool};
use codeyard_git_storage::{context::NodeContext, repositories::store::RepoStore};
use codeyard_storage::nodes::{Declaration, NodeStore};
use std::{io::Write, path::Path, sync::Arc, time::Duration};
use tokio::io::AsyncWriteExt;

pub(super) async fn save_objects(
    context: &NodeContext,
    objects: Vec<codeyard_db_placement::objects::Object>,
    root: &Path,
    lock: Arc<std::fs::File>,
) -> Result<()> {
    let store = codeyard_git_storage::objects::Store::new(context.clone());
    for object in objects {
        let expected = Declaration {
            key: object.object_key,
            digest: object.digest,
            bytes: object.bytes,
            content_type: object.content_type,
        };
        ensure!(expected.valid(), "invalid backup object declaration");
        let (actual, bytes) = store.get(&expected.key, 0, None).await?;
        ensure!(
            actual.digest == expected.digest
                && actual.bytes == expected.bytes
                && actual.content_type == expected.content_type,
            "object changed while being backed up"
        );
        let target = root.join(expected.address()?);
        reject_symlink(&target)?;
        let guard = lock.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let parent = target
                .parent()
                .context("object backup path has no parent")?;
            codeyard_support::fs::private_dir(parent)?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            temporary.write_all(&bytes)?;
            temporary.as_file().sync_all()?;
            temporary.persist(&target).map_err(|error| error.error)?;
            sync_dir(parent)?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
    }
    Ok(())
}

// pg_restore and this fencing statement run in the same psql transaction. A failed
// import leaves the destination empty; no copied live address or claim can become active.
const FENCE: &str = r#"DO $$ BEGIN
IF EXISTS(SELECT 1 FROM node_configuration WHERE cluster_enabled) THEN
    UPDATE node_configuration SET restore_pending=true,restore_digest='__RESTORE_DIGEST__' WHERE cluster_enabled;
    UPDATE nodes SET state='drained',last_seen=NULL,addr_hint=NULL,drain_started_at=NULL,drain_total=0,drain_completed=0,drain_error=NULL;
    UPDATE repository_write_control SET serving=0;
    UPDATE repository_replicas SET applied_version=-1,synced_at=NULL;
    UPDATE repository_voters SET applied_version=-1;
    DELETE FROM node_drain_repositories;
    DELETE FROM repository_moves;
    DELETE FROM cluster_leases;
    DELETE FROM join_tokens;
    UPDATE cluster_scaling_policies SET configuration=jsonb_set(jsonb_set(configuration,'{paused}','true'),'{dry_run}','true'),trigger_since=NULL,last_action_at=NULL;
    UPDATE ci_runs SET status='cancelled',cancel_requested=true,error_message='Interrupted by cluster restore',completed_at=clock_timestamp(),updated_at=clock_timestamp() WHERE status IN ('queued','running');
    UPDATE actions_jobs SET status='completed',conclusion='cancelled',completed_at=clock_timestamp() WHERE status<>'completed';
    DELETE FROM personal_access_tokens WHERE user_id IN(SELECT id FROM users WHERE username='github-actions[bot]');
    DELETE FROM storage_objects WHERE confirmed=0;
    DELETE FROM storage_object_copies WHERE object_key IN(SELECT object_key FROM storage_objects WHERE confirmed=-1);
    UPDATE storage_object_copies SET ready=0;
END IF;
END $$;"#;

pub(super) fn command_timeout() -> Result<Duration> {
    let seconds = match std::env::var("BACKUP_COMMAND_TIMEOUT_SECONDS") {
        Ok(raw) => raw.parse::<u64>()?,
        Err(std::env::VarError::NotPresent) => 3600,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        (1..=86400).contains(&seconds),
        "invalid backup command timeout"
    );
    Ok(Duration::from_secs(seconds))
}

fn database_limit() -> Result<u64> {
    let limit = match std::env::var("BACKUP_MAX_DATABASE_BYTES") {
        Ok(raw) => raw.parse::<u64>()?,
        Err(std::env::VarError::NotPresent) => 10 * 1024 * 1024 * 1024,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        (1..=10 * 1024 * 1024 * 1024 * 1024).contains(&limit),
        "invalid backup database limit"
    );
    Ok(limit)
}

/// A retry owns the same archive and data paths; a pending restore never authorizes
/// replacement of an unrelated installation's files.
pub(super) async fn restore_fingerprint(places: &Places<'_>, archive: &Path) -> Result<String> {
    use sha2::Digest;
    use tokio::io::AsyncReadExt;
    let limit = database_limit()?;
    let mut input = tokio::fs::File::open(archive).await?.take(limit + 1);
    let mut hash = sha2::Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0; 64 * 1024];
    tokio::time::timeout(command_timeout()?, async {
        loop {
            let count = input.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            ensure!(
                bytes <= limit,
                "backup archive exceeds the database size limit"
            );
            hash.update(&buffer[..count]);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    for path in [places.repos_path, places.state_dir] {
        let path = super::resolved(path)?;
        let bytes = path.as_os_str().as_encoded_bytes();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    Ok(hex::encode(hash.finalize()))
}

pub(super) async fn restore_postgres(
    places: &Places<'_>,
    archive: &Path,
    schema: &str,
) -> Result<()> {
    let url = places.database_url;
    let limit = database_limit()?;
    let fingerprint = restore_fingerprint(places, archive).await?;
    let sql = tempfile::NamedTempFile::new()?;
    private_file(sql.path())?;
    let mut output = tokio::fs::File::from_std(sql.reopen()?);
    let mut stderr = Vec::new();
    let status = {
        let mut bounded =
            codeyard_git_storage::git_transport::LimitedWriter::new(&mut output, limit);
        let mut errors =
            codeyard_git_storage::git_transport::LimitedWriter::new(&mut stderr, 64 * 1024);
        let mut command = tokio::process::Command::new("pg_restore");
        command
            .args([
                "--no-owner",
                "--no-privileges",
                "--clean",
                "--if-exists",
                "--file=-",
            ])
            // pg_restore matches the literal archive name; pg_dump uses a pattern.
            .arg(format!("--schema={schema}"))
            .arg("--")
            .arg(archive)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        tokio::time::timeout(
            command_timeout()?,
            codeyard_git::repository::process::stream(
                command,
                tokio::io::empty(),
                &mut bounded,
                &mut errors,
                false,
            ),
        )
        .await??
    };
    ensure!(status.success(), "pg_restore could not read this archive");
    output.flush().await?;
    drop(output);
    super::postgres_tool(
        "psql",
        url,
        &[
            "--no-psqlrc",
            "--quiet",
            "--set=ON_ERROR_STOP=1",
            "--single-transaction",
            &format!("--file={}", sql.path().display()),
            "--command",
            &format!(
                "SET search_path TO \"{}\"; {}",
                schema.replace('"', "\"\""),
                FENCE.replace("__RESTORE_DIGEST__", &fingerprint)
            ),
        ],
    )
    .await
}

pub(super) async fn restore_refs(pool: &DbPool, root: &Path, force: bool) -> Result<()> {
    for repository in codeyard_db_placement::backups::list_repositories(pool).await? {
        if repository.storage_node_id.is_none() {
            continue;
        }
        let uuid = codeyard_git_storage::wire::repository_uuid(&repository.disk_path)?;
        let path = root.join(format!("{uuid}.git"));
        let store = RepoStore::from_disk_path(path.to_str().context("restore path is not text")?);
        let (refs, head) = store.replica_listing().await?;
        let mut values: Vec<_> = refs
            .into_iter()
            .map(|(name, target)| RefValue {
                name,
                kind: "oid".into(),
                target,
            })
            .collect();
        if let Some(head) = head {
            values.push(RefValue {
                name: "HEAD".into(),
                kind: "symbolic".into(),
                target: head,
            });
        }
        ensure!(
            values.iter().any(|value| value.name == "HEAD"),
            "restored repository has no symbolic HEAD"
        );
        codeyard_db_placement::backups::restore_refs(pool, repository.id, &values, force).await?;
    }
    Ok(())
}

/// Installs this storage identity's assigned copies into an empty or interrupted restore.
pub async fn restore_node(pool: &DbPool, places: &Places<'_>, dir: &Path) -> Result<()> {
    ensure!(
        codeyard_db_placement::cluster::cluster_enabled(pool).await?,
        "node restore requires a restored cluster database"
    );
    let maintenance = codeyard_db_placement::cluster::maintenance_lock(pool).await?;
    ensure!(
        codeyard_db_placement::backups::restore_pending(pool).await?,
        "node restore requires a pending cluster restore"
    );
    codeyard_support::fs::private_dir(places.repos_path)?;
    codeyard_support::fs::private_dir(places.state_dir)?;
    let _root = codeyard_support::locks::exclusive_file(&places.repos_path.join("storage.lock"))?;
    let _identity = codeyard_support::locks::exclusive_file(&places.state_dir.join("serve.lock"))?;
    let key = codeyard_support::node_identity::load_existing(places.state_dir)?
        .context("supply this enrolled storage node's original NODE_KEY or NODE_KEY_FILE")?;
    let node = codeyard_db_placement::cluster::get_node_by_public_key(pool, &key.public_hex())
        .await?
        .context("node key is not in the restored database")?;
    ensure!(
        node.status == "active"
            && node.last_seen.is_none()
            && node.roles.iter().any(|role| role == "storage"),
        "node restore requires a stopped, enrolled storage identity"
    );
    let repositories = codeyard_db_placement::backups::list_repositories(pool).await?;
    let mut installed = Vec::new();
    for repository in repositories {
        let Some(primary) = repository.storage_node_id else {
            continue;
        };
        let view = codeyard_db_placement::cluster::writes::write_settings(pool).await?;
        let quorum = if view.write_quorum.is_some() {
            Some(codeyard_db_placement::cluster::writes::recovery_view(pool, repository.id).await?)
        } else {
            None
        };
        let assigned = primary == node.id
            || quorum
                .as_ref()
                .is_some_and(|view| view.voters.iter().any(|voter| voter.node_id == node.id))
            || codeyard_db_placement::cluster::replica_applied_version(
                pool,
                repository.id,
                node.id,
            )
            .await?
            .is_some();
        if !assigned {
            continue;
        }
        if let Some(view) = &quorum {
            ensure!(
                !view.serving,
                "stop and fence every repository before node restore"
            );
        }
        let uuid = codeyard_git_storage::wire::repository_uuid(&repository.disk_path)?;
        let source = dir.join(REPOSITORIES_DIR).join(format!("{uuid}.git"));
        reject_symlink(&source)?;
        ensure!(
            source.join("HEAD").is_file(),
            "backup repository {uuid} is missing"
        );
        let target = if primary == node.id {
            codeyard_git_storage::repositories::store::primary_path(places.repos_path, uuid)
        } else {
            codeyard_git_storage::cluster::replicas::copy_path(places.repos_path, uuid)
        };
        reject_symlink(&target)?;
        codeyard_git::mirror::mirror(&source, &target).await?;
        if let Some(view) = &quorum {
            let store =
                RepoStore::from_disk_path(target.to_str().context("restore path is not text")?);
            store.restore_refs(&view.refs).await?;
            if view.voters.iter().any(|voter| voter.node_id == node.id) {
                RepoStore::recover_term(pool, repository.id, node.id, places.repos_path).await?;
            }
        }
        let version = codeyard_db_placement::cluster::get_write_version(pool, repository.id)
            .await?
            .context("restored repository disappeared")?;
        installed.push((repository.id, primary, version));
    }
    let objects = codeyard_db_placement::backups::node_objects(pool, node.id).await?;
    let local =
        codeyard_storage::local::LocalStore::open(&places.repos_path.join(UPLOADS_DIR)).await?;
    let io = Arc::new(tokio::sync::Semaphore::new(1));
    for object in &objects {
        let declaration = Declaration {
            key: object.object_key.clone(),
            digest: object.digest.clone(),
            bytes: object.bytes,
            content_type: object.content_type.clone(),
        };
        let address = declaration.address()?;
        let source = dir.join(UPLOADS_DIR).join(&address);
        reject_symlink(&source)?;
        let reader = tokio::fs::File::open(source).await?;
        local
            .put(
                &codeyard_storage::Upload {
                    key: &address,
                    ..declaration.upload()
                },
                reader,
                Arc::new(io.clone().try_acquire_owned()?),
            )
            .await?;
    }
    codeyard_db_placement::backups::restore_node_ready(pool, node.id, &key.public_hex()).await?;
    for object in &objects {
        codeyard_db_placement::objects::ready(
            pool,
            &object.object_key,
            node.id,
            &key.public_hex(),
            &key.public_hex(),
        )
        .await?;
    }
    for (repository, primary, version) in installed {
        if primary != node.id {
            codeyard_db_placement::cluster::set_applied_version(
                pool, repository, node.id, primary, version,
            )
            .await?;
        }
    }
    maintenance.release().await;
    println!("Storage node {} restored. Keep all cores stopped until every storage node has completed restore --node.",node.name);
    Ok(())
}
