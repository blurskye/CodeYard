//! `codeyard serve`: starts the HTTP server, the selected SSH transport and the
//! CI service, then waits for a shutdown signal.
use super::{config::ServeConfig, NodeRole};
use crate::backend::{api, cluster, db::DbPool, fs::lock_state_dir, ssh};
use crate::backend::{
    app::AppState,
    ci::CiConfig,
    crypto::SecretEncryptor,
    storage::{s3::S3Config, ObjectStore},
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::{path::Path, sync::Arc};
use tracing::{info, warn};

fn validate_cluster_encryptor(
    clustered: bool,
    encryptor: Option<&SecretEncryptor>,
    sample: Option<(&[u8], &[u8])>,
) -> Result<()> {
    if !clustered {
        return Ok(());
    }
    let encryptor = encryptor
        .context("Cluster core nodes require the same SECRETS_MASTER_KEY for encrypted secrets")?;
    if let Some((ciphertext, nonce)) = sample {
        encryptor
            .decrypt(ciphertext, nonce)
            .context("SECRETS_MASTER_KEY cannot open existing encrypted secrets")?;
    }
    Ok(())
}

/// All background owners use the same shutdown deadline and abort fallback.
async fn drain_worker<T>(task: Option<tokio::task::JoinHandle<T>>, deadline: tokio::time::Instant) {
    if let Some(task) = task {
        let mut task = tokio_util::task::AbortOnDropHandle::new(task);
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
}

fn prepare_repository_root(path: &Path) -> Result<PathBuf> {
    crate::backend::fs::private_dir(path)?;
    Ok(path.canonicalize()?)
}

/// Builds the state from environment configuration before listeners start.
async fn prepare_application(
    pool: DbPool,
    repos_path: PathBuf,
    state_dir: &Path,
    serves_web: bool,
    cluster: Option<crate::backend::cluster::NodeContext>,
) -> Result<AppState> {
    crate::backend::api::http::config::public_url()?;
    if cluster.is_none() {
        let (state_dir, repos) = (state_dir.to_owned(), repos_path.clone());
        tokio::task::spawn_blocking(move || {
            crate::backend::repositories::releases::immutable::configure_local_identity(
                &state_dir, &repos,
            )
        })
        .await??;
    }
    let secret_encryptor = SecretEncryptor::configured_from_env()?.map(Arc::new);
    validate_cluster_encryptor(cluster.is_some(), secret_encryptor.as_deref(), None)?;
    if cluster.is_some() {
        for (text, nonce) in crate::backend::db::secrets::encryption_samples(&pool).await? {
            validate_cluster_encryptor(true, secret_encryptor.as_deref(), Some((&text, &nonce)))?;
        }
    }
    if secret_encryptor.is_none() {
        tracing::warn!("SECRETS_MASTER_KEY not set, secrets API disabled");
    }
    let s3 = S3Config::optional_from_env()?;
    if let Some(config) = s3.as_ref().filter(|_| serves_web) {
        config.validate_browser_cookie_scope_from_env()?;
    }
    let storage = Arc::new(ObjectStore::open(state_dir, s3).await?);
    // Only web nodes take uploads, so an object storage outage does not keep a Git-only
    // node from starting.
    if serves_web {
        storage.prepare().await?;
    }
    crate::backend::repositories::pull_requests::async_merge::recover(
        &pool,
        cluster.as_ref().map(|n| n.node_id),
    )
    .await?;
    Ok(AppState::new(
        pool,
        repos_path,
        storage,
        secret_encryptor,
        CiConfig::from_env()?,
        cluster,
    ))
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// How long shutdown waits for listeners and Git sessions to drain, then for the upload sweep
/// and replica loop to stop. These share this one deadline.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Runs the expiry sweeps every few minutes: pending uploads whose window passed,
/// old transfer accounting, expired identity rows, staged uploads a crash left
/// behind and, on a node that stores repositories, deleted repositories kept past the
/// restore window. Safe to run on several nodes at once: each row is removed by only one.
async fn remove_expired_rows(
    pool: DbPool,
    storage: std::sync::Arc<crate::backend::storage::ObjectStore>,
    web: bool,
    repos_path: Option<PathBuf>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
    // A sweep that outlasts the interval must not be followed by a burst of catch-up sweeps.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = interval.tick() => {}
        }
        // Uploads and account rows belong to web nodes; deleted repositories to the node holding them.
        if web {
            remove_expired_web_rows(&pool, &storage).await;
        }
        if let Some(root) = repos_path.clone() {
            let window = std::time::Duration::from_secs(
                crate::backend::db::files::RESTORE_WINDOW_SECONDS as u64,
            );
            let sweep = tokio::task::spawn_blocking(move || {
                let mirrors = root.join(crate::backend::cluster::replicas::MIRRORS);
                let quarantines = mirrors.join(crate::backend::cluster::replicas::QUARANTINES);
                let mut removed = 0;
                for root in [root, mirrors, quarantines] {
                    removed += crate::backend::git::mirror::remove_expired_deleted(&root, window)?;
                }
                anyhow::Ok(removed)
            });
            match sweep.await {
                Ok(Ok(0)) => {}
                Ok(Ok(removed)) => info!(
                    removed,
                    "removed deleted repositories past the restore window"
                ),
                Ok(Err(error)) => warn!(%error, "could not remove old deleted repositories"),
                Err(error) => warn!(%error, "deleted repository cleanup stopped"),
            }
        }
    }
}

async fn remove_expired_web_rows(pool: &DbPool, storage: &crate::backend::storage::ObjectStore) {
    match tokio::time::timeout(
        std::time::Duration::from_secs(12),
        crate::backend::files::remove_expired_uploads(pool, storage),
    )
    .await
    {
        Ok(Ok(0)) => {}
        Ok(Ok(removed)) => info!(removed, "removed expired uploads"),
        Ok(Err(error)) => warn!(%error, "could not remove expired uploads"),
        Err(_) => warn!("upload cleanup budget exhausted; remaining work stays queued"),
    }
    // Object families have independent budgets; a slow package delete cannot cancel uploads.
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        crate::backend::packages::collect(pool, storage),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!(%error, "package objects were not collected"),
        Err(_) => warn!("package object collection timed out"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), async {
        crate::backend::db::attestations::collect(pool).await?;
        crate::backend::db::action_objects::expire_objects(pool).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    {
        Ok(Ok(())) => {}
        _ => warn!("attestation or Actions object collection was deferred"),
    }
    match crate::backend::db::files::remove_expired_transfer_rows(pool).await {
        Ok(0) => {}
        Ok(removed) => info!(removed, "removed expired transfer accounting"),
        Err(_) => warn!("could not remove expired transfer accounting"),
    }
    match crate::backend::db::users::sessions::remove_expired_identity_rows(pool).await {
        Ok(0) => {}
        Ok(changed) => info!(changed, "cleaned up expired identity rows"),
        Err(_) => warn!("could not clean up expired identity rows"),
    }
    if let Err(error) = crate::backend::db::pull_requests::async_merge::prune(pool).await {
        warn!(%error,"could not prune asynchronous merge results");
    }
    if let Err(error) = crate::backend::db::events::prune_events(pool).await {
        warn!(%error,"could not prune old repository activity");
    }
    if let Err(error) = crate::backend::db::webhooks::prune_expired_deliveries(pool).await {
        warn!(%error, "could not prune expired webhook deliveries");
    }
    if let Err(error) = crate::backend::db::dependency_graph::prune_expired_reports(pool).await {
        warn!(%error, "could not prune expired SBOM reports");
    }
    if let Err(error) = crate::backend::db::secret_scanning::prune_expired_bypasses(pool).await {
        warn!(%error, "could not prune expired unused secret scanning bypasses");
    }
    match storage.remove_stale_staging().await {
        Ok(0) => {}
        Ok(removed) => info!(removed, "removed stale upload staging files"),
        Err(error) => warn!(%error, "could not remove stale upload staging files"),
    }
}

fn select_transport(config: &ServeConfig) -> Result<ssh::mode::Selection> {
    if !config.roles.contains(NodeRole::Ssh) {
        return Ok(ssh::mode::Selection {
            mode: ssh::mode::SshMode::Disabled,
            setup_required: false,
            git_user: "git".into(),
            socket: PathBuf::from(ssh::mode::DEFAULT_SOCKET),
            builtin_port: config.ssh_addr.port(),
        });
    }
    ssh::mode::select(
        config.ssh_mode,
        config.ssh_socket.clone(),
        config.ssh_git_user.clone(),
        &config.ssh_install_manifest,
        config.ssh_addr.port(),
    )
}

/// A worker-only process has no application state, database or inbound listeners.
pub async fn serve_worker(state_dir: &std::path::Path) -> Result<()> {
    let _state_dir_lock = lock_state_dir(state_dir)?;
    let settings = cluster::Settings::from_lookup(&|key| {
        // Empty counts as unset, as in `Settings::from_lookup`, so the fallback below applies.
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| (key == "IROH_BIND").then(|| "127.0.0.1:0".into()))
    })?
    .context("runner needs cluster settings")?;
    let signal = shutdown_signal();
    tokio::pin!(signal);
    // Polling the signal first installs its handlers before anything else runs; the worker
    // announces itself ready and may be stopped right away.
    let (context, peers) = tokio::select! {
        biased;
        result = &mut signal => return result,
        result = cluster::worker_context(state_dir, settings) => result?,
    };
    let config = std::sync::Arc::new(crate::backend::ci::CiConfig::from_env()?);
    let workers = crate::backend::ci::CiWorkers::new(
        crate::backend::ci::queue::RunQueue::Remote {
            preferred: Default::default(),
            context: context.clone(),
            peers,
        },
        config,
        true,
        state_dir,
    );
    let started = tokio::select! {
        biased;
        result = &mut signal => { let stopped = workers.shutdown().await; context.iroh.close().await; return result.and(stopped); },
        result = workers.startup() => result,
    };
    if let Err(error) = started {
        let _ = workers.shutdown().await;
        context.iroh.close().await;
        return Err(error);
    }
    info!("database-free runner ready; no web, SSH or cluster listener");
    let result = tokio::select! {
        result = &mut signal => result,
        () = workers.stopped() => Ok(()),
    };
    info!("shutting down");
    let stopped = workers.shutdown().await;
    context.iroh.close().await;
    result.and(stopped)
}

pub async fn serve(pool: DbPool, config: ServeConfig) -> Result<()> {
    let signal = shutdown_signal();
    tokio::pin!(signal);
    let clustered = crate::backend::db::cluster::cluster_enabled(&pool).await?;
    let _installation_lock = if !clustered {
        Some(crate::backend::db::cluster::serving_lock(&pool, "installation").await?)
    } else {
        None
    };
    if !config.roles.single_node() && !clustered {
        pool.close().await;
        anyhow::bail!(super::SINGLE_NODE_ROLES);
    }
    let repos_path = if config.roles.contains(NodeRole::Storage) {
        prepare_repository_root(&config.repos_path)?
    } else {
        std::path::absolute(&config.repos_path)?
    };
    let _state_dir_lock = match lock_state_dir(&config.state_dir) {
        Ok(lock) => lock,
        Err(error) => {
            pool.close().await;
            return Err(error);
        }
    };
    // The cluster transport is opt-in. Without cluster settings no cluster socket is
    // opened. With them, single-node mode still admits only runners.
    let member = match cluster::Settings::from_env()? {
        Some(settings) => Some(tokio::select! {
            biased;
            result = &mut signal => { pool.close().await; return result; }
            result = cluster::start(&pool, &config.state_dir, &repos_path, &config.roles, settings) => result?,
        }),
        None => None,
    };
    if member.is_none() && crate::backend::db::cluster::cluster_enabled(&pool).await? {
        pool.close().await;
        anyhow::bail!(
            "this database is in cluster mode; set the CLUSTER_* settings so this node can serve it"
        );
    }
    let _repository_lock = if member.is_none() && config.roles.contains(NodeRole::Storage) {
        Some(crate::backend::locks::exclusive_file(
            &repos_path.join("storage.lock"),
        )?)
    } else {
        None
    };
    let prepared = async {
        anyhow::ensure!(
        config.roles.faces_users() || member.is_some(),
        "A node without web or ssh only serves other nodes: join a cluster first (see `codeyard cluster join`)"
    );
        let transport = select_transport(&config)?;
        ssh::mode::publish(transport.clone())?;
        info!(ssh_mode = transport.mode.as_str(), roles = ?config.roles, "starting codeyard");
        if transport.setup_required {
            warn!("SSH setup required: no SSH listener will be opened. Run the OpenSSH installer, or set SSH_MODE=builtin or SSH_MODE=disabled.");
        }
        let state = prepare_application(
            pool.clone(),
            repos_path.clone(),
            &config.state_dir,
            config.roles.contains(NodeRole::Web),
            member.as_ref().map(|node| node.context.clone()),
        )
        .await?;
        // Only the web socket subscribes to inbound notifications.
        if config.roles.contains(NodeRole::Web) {
            state.notification_broadcaster.start().await;
        }
        Ok::<_, anyhow::Error>((transport, repos_path, state))
    };
    let prepared = tokio::select! {
        biased;
        result = &mut signal => {
            if let Some(member) = &member { member.shutdown().await; }
            pool.close().await;
            return result;
        }
        result = prepared => result,
    };
    let (transport, repos_path, state) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(member) = &member {
                member.shutdown().await;
            }
            return Err(error);
        }
    };
    let ci_runs = state.ci_runs.clone();
    let notifications = state.notification_broadcaster.clone();
    let git_tasks = state.git_tasks.clone();
    let workers = crate::backend::ci::CiWorkers::with_wakeup(
        crate::backend::ci::queue::RunQueue::Database {
            pool: pool.clone(),
            config: ci_runs.config.clone(),
            secrets: state.secret_encryptor.clone(),
            context: state.cluster.clone(),
            notifications: notifications.clone(),
        },
        ci_runs.config.clone(),
        config.roles.contains(NodeRole::Runner),
        ci_runs.queued.clone(),
        &config.state_dir,
    );
    let started = async {
        // Claims need this node's published storage address, including for unplaced repositories.
        if let Some(member) = &member {
            member.publish().await?;
        }
        workers.startup().await?;
        Ok::<_, anyhow::Error>(true)
    };
    let started = tokio::select! {
        biased;
        result = &mut signal => result.map(|()| false),
        result = started => result,
    };
    if !matches!(&started, Ok(true)) {
        workers.request_stop();
        if let Some(member) = &member {
            member.withdraw().await;
        }
        let stopped = workers.shutdown().await;
        if let Some(member) = &member {
            member.shutdown().await;
        }
        notifications.shutdown().await;
        pool.close().await;
        return started.map(|_| ()).and(stopped);
    }
    let (stop_traffic, traffic_stop) = tokio::sync::watch::channel(false);
    let traffic = tokio::spawn(crate::backend::repositories::traffic::maintain(
        pool.clone(),
        traffic_stop,
    ));
    let (stop_vulnerabilities, vulnerabilities_stop) = tokio::sync::watch::channel(false);
    let vulnerabilities = config.roles.contains(NodeRole::Web).then(|| {
        tokio::spawn(crate::backend::vulnerabilities::run(
            Arc::new(state.clone()),
            vulnerabilities_stop,
        ))
    });
    let (stop_scanning, scanning_stop) = tokio::sync::watch::channel(false);
    let scanning = config.roles.contains(NodeRole::Web).then(|| {
        tokio::spawn(crate::backend::code_scanning::run(
            pool.clone(),
            scanning_stop,
        ))
    });
    let (stop_schedules, schedules_stop) = tokio::sync::watch::channel(false);
    let schedules = config.roles.contains(NodeRole::Web).then(|| {
        tokio::spawn(crate::backend::ci::schedule::run(
            ci_runs.clone(),
            schedules_stop,
        ))
    });
    let (stop_expiry, expiry_stop) = tokio::sync::watch::channel(false);
    let expiry = (config.roles.contains(NodeRole::Web) || config.roles.contains(NodeRole::Storage))
        .then(|| {
            tokio::spawn(remove_expired_rows(
                pool.clone(),
                state.storage.clone(),
                config.roles.contains(NodeRole::Web),
                config
                    .roles
                    .contains(NodeRole::Storage)
                    .then(|| repos_path.clone()),
                expiry_stop,
            ))
        });
    // Every web node sends queued email; each claims its own messages.
    let (stop_secret_scanning, secret_scanning_stop) = tokio::sync::watch::channel(false);
    let secret_scanning = config.roles.contains(NodeRole::Web).then(|| {
        tokio::spawn(crate::backend::repositories::secret_scanning::jobs::run(
            Arc::new(state.clone()),
            secret_scanning_stop,
        ))
    });
    let (stop_auto_merge, auto_merge_stop) = tokio::sync::watch::channel(false);
    let auto_merge = config.roles.contains(NodeRole::Web).then(|| {
        tokio::spawn(
            crate::backend::repositories::pull_requests::auto_merge::run(
                Arc::new(state.clone()),
                auto_merge_stop,
            ),
        )
    });
    let (stop_email, email_stop) = tokio::sync::watch::channel(false);
    let email = config.roles.contains(NodeRole::Web).then(|| {
        tokio::spawn(crate::backend::email::outbox::run(
            pool.clone(),
            state.secret_encryptor.clone(),
            email_stop,
        ))
    });
    // Durable inventory jobs are claimed transactionally; every web node may drain the queue.
    let (stop_metadata, metadata_stop) = tokio::sync::watch::channel(false);
    let metadata = config.roles.contains(NodeRole::Web).then(|| {
        tokio::spawn(crate::backend::artifact_metadata::run(
            pool.clone(),
            metadata_stop,
        ))
    });
    // Storage nodes keep the replica copies of their repositories current.
    let (stop_replicas, replica_stop) = tokio::sync::watch::channel(false);
    let replicas = member
        .as_ref()
        .filter(|_| config.roles.contains(NodeRole::Storage))
        .map(|member| tokio::spawn(cluster::replicas::run(member.context.clone(), replica_stop)));

    // Listeners stop accepting when `stop_listeners` is set, then finish what is in progress.
    let (stop_listeners, stop) = tokio::sync::watch::channel(false);
    let web_server = async {
        if config.roles.contains(NodeRole::Web) {
            api::run_web_server(config.http_addr, state, stop.clone()).await
        } else {
            // Without a web listener, this future ends with the shutdown signal.
            let _ = stop.clone().wait_for(|stopped| *stopped).await;
            Ok(())
        }
    };
    let servers = async {
        match transport.mode {
            ssh::mode::SshMode::Builtin => {
                let host_key = config
                    .host_key
                    .to_str()
                    .context("host key path is not UTF-8")?;
                let mut server =
                    ssh::SshServer::new(pool.clone(), Some(host_key), ci_runs.clone()).await?;
                info!(address = %config.ssh_addr, "built-in SSH listening");
                tokio::try_join!(server.run(config.ssh_addr, stop.clone()), web_server).map(|_| ())
            }
            #[cfg(unix)]
            ssh::mode::SshMode::Openssh if !transport.setup_required => {
                info!("using host OpenSSH for Git over SSH");
                let gateway = crate::backend::repositories::git_sessions::run_gateway(
                    transport.socket,
                    repos_path,
                    pool.clone(),
                    ci_runs.clone(),
                    transport.git_user,
                    stop.clone(),
                );
                tokio::try_join!(gateway, web_server).map(|_| ())
            }
            _ => web_server.await,
        }
    };
    let mut servers = Box::pin(servers);
    let (result, signalled) = tokio::select! {
        result = &mut servers => (
            result.and_then(|()| Err(anyhow::anyhow!("a listener stopped unexpectedly"))),
            false,
        ),
        result = &mut signal => (result, true),
    };
    info!("shutting down");
    // Every local owner is told to stop before anything waits on one of them.
    if signalled {
        stop_listeners.send_replace(true);
    }
    stop_traffic.send_replace(true);
    stop_vulnerabilities.send_replace(true);
    stop_scanning.send_replace(true);
    stop_schedules.send_replace(true);
    stop_expiry.send_replace(true);
    stop_email.send_replace(true);
    stop_metadata.send_replace(true);
    stop_secret_scanning.send_replace(true);
    stop_auto_merge.send_replace(true);
    stop_replicas.send_replace(true);
    workers.request_stop();
    // Stop being a placement target before the node drains its work. The withdrawal is
    // bounded, so a row lock held elsewhere cannot stall the drain.
    if let Some(member) = &member {
        member.withdraw().await;
    }
    // The listeners, the upload sweep and the replica loop share one deadline.
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    if signalled {
        info!("draining listeners");
        match tokio::time::timeout_at(deadline, servers).await {
            Ok(Err(error)) => warn!("listener failed while draining: {error}"),
            Ok(Ok(())) => {}
            Err(_) => {
                warn!("open connections did not finish within {SHUTDOWN_GRACE:?}; closing them");
            }
        }
    }
    // A failed listener is dropped before it can drain the Git pushes it already accepted.
    // A clean stop has drained them, so this returns at once.
    let _ = tokio::time::timeout_at(deadline, git_tasks.drain()).await;
    // Repository sessions from other nodes are waited for to the same deadline. One still
    // running after it ends with its connection in `member.shutdown()`.
    if let Some(member) = &member {
        let _ = tokio::time::timeout_at(deadline, member.drain_sessions()).await;
    }
    drain_worker(Some(traffic), deadline).await;
    drain_worker(vulnerabilities, deadline).await;
    drain_worker(scanning, deadline).await;
    drain_worker(schedules, deadline).await;
    drain_worker(expiry, deadline).await;
    drain_worker(email, deadline).await;
    drain_worker(metadata, deadline).await;
    drain_worker(secret_scanning, deadline).await;
    // A merge may already have published Git. Stop between PRs, but never abort
    // its authoritative post-publication bookkeeping on the listener deadline.
    if let Some(auto_merge) = auto_merge {
        if let Err(error) = auto_merge.await {
            warn!(%error, "auto-merge stopped unexpectedly");
        }
    }
    // Replica downloads cancel on stop; an installation already mutating Git must
    // be joined before directory ownership and the storage-root lock can be released.
    if let Some(replicas) = replicas {
        if let Err(error) = replicas.await {
            warn!(%error, "replica worker stopped unexpectedly");
        }
    }
    let stopped = workers.shutdown().await;
    if let Some(member) = &member {
        member.shutdown().await;
        // Listener shutdown cancels network waiters, not admitted mutation tasks.
        // Keep the pool and event sinks alive until their effects have finished.
        member.drain_sessions().await;
    }
    crate::backend::webhooks::shutdown(tokio::time::Instant::now() + SHUTDOWN_GRACE).await;
    notifications.shutdown().await;
    pool.close().await;
    result.and(stopped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The longest a clean stop may take. `compose.yaml` allows the server a little longer.
    const MAX_SHUTDOWN: Duration = Duration::from_secs(90);

    #[tokio::test]
    async fn expiry_sweep_reclaims_transfer_accounting() {
        use crate::backend::db;

        let directory = tempfile::tempdir().unwrap();
        let pool = db::init_db(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        db::run_migrations(&pool).await.unwrap();
        let owner = db::users::accounts::create_user(&pool, "owner", None)
            .await
            .unwrap();
        let DbPool::Sqlite(raw) = &pool else {
            unreachable!()
        };
        sqlx::query("INSERT INTO upload_rate_limit(user_id,file_size,uploaded_at) VALUES (?1,10,strftime('%Y-%m-%dT%H:%M:%f000Z','now','-731 days'))")
            .bind(owner.id).execute(raw).await.unwrap();
        let storage = Arc::new(ObjectStore::open(directory.path(), None).await.unwrap());
        storage.prepare().await.unwrap();
        let (stop, receiver) = tokio::sync::watch::channel(false);
        let sweep = tokio::spawn(remove_expired_rows(
            pool.clone(),
            storage,
            true,
            None,
            receiver,
        ));
        let removed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_rate_limit")
                    .fetch_one(raw)
                    .await
                    .unwrap();
                if count == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), sweep)
            .await
            .unwrap()
            .unwrap();
        pool.close().await;
        removed.expect("the scheduled sweep must reclaim expired transfer accounting");
    }

    #[test]
    fn every_cluster_core_requires_a_webhook_encryptor() {
        assert!(validate_cluster_encryptor(false, None, None).is_ok());
        assert!(validate_cluster_encryptor(true, None, None).is_err());
        let key = SecretEncryptor::new(&[7; 32]);
        assert!(validate_cluster_encryptor(true, Some(&key), None).is_ok());
        let (text, nonce) = key.encrypt(b"hook secret").unwrap();
        assert!(validate_cluster_encryptor(true, Some(&key), Some((&text, &nonce))).is_ok());
        assert!(validate_cluster_encryptor(
            true,
            Some(&SecretEncryptor::new(&[8; 32])),
            Some((&text, &nonce))
        )
        .is_err());
    }

    #[test]
    fn shutdown_phases_fit_inside_the_stop_grace() {
        // The withdrawal runs twice: before the drain, and again in `Cluster::shutdown`.
        let phases = SHUTDOWN_GRACE
            + crate::backend::ci::workers::SHUTDOWN_GRACE
            + crate::backend::cluster::WITHDRAW_TIMEOUT * 2;
        assert!(
            phases < MAX_SHUTDOWN,
            "the shutdown phases add up to {phases:?}"
        );
    }
}
