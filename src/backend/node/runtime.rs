//! `codeyard serve`: starts the HTTP server, the selected SSH transport and the
//! CI service, then waits for a shutdown signal.
use super::{config::ServeConfig, NodeRole};
use crate::backend::{api, cluster, db::DbPool, ssh};
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

fn prepare_repository_root(path: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
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
    let secret_encryptor = SecretEncryptor::configured_from_env()?.map(Arc::new);
    if secret_encryptor.is_none() {
        tracing::warn!("SECRETS_MASTER_KEY not set, secrets API disabled");
    }
    let has_s3 = [
        "S3_ENDPOINT",
        "S3_PUBLIC_ENDPOINT",
        "S3_ACCESS_KEY",
        "S3_SECRET_KEY",
        "S3_BUCKET",
        "S3_REGION",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some());
    let s3 = if has_s3 {
        let config = S3Config::from_env()?;
        if serves_web {
            config.validate_browser_cookie_scope_from_env()?;
        }
        Some(config)
    } else {
        None
    };
    let storage = Arc::new(ObjectStore::open(state_dir, s3).await?);
    storage.prepare().await?;
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
/// old transfer accounting, expired identity rows and staged uploads a crash left
/// behind. Safe to run on several nodes at once: each row is removed by only one.
async fn remove_expired_rows(
    pool: DbPool,
    storage: std::sync::Arc<crate::backend::storage::ObjectStore>,
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
        match crate::backend::files::remove_expired_uploads(&pool, &storage).await {
            Ok(0) => {}
            Ok(removed) => info!(removed, "removed expired uploads"),
            Err(_) => warn!("could not remove expired uploads"),
        }
        match crate::backend::db::files::remove_expired_transfer_rows(&pool).await {
            Ok(0) => {}
            Ok(removed) => info!(removed, "removed expired transfer accounting"),
            Err(_) => warn!("could not remove expired transfer accounting"),
        }
        match crate::backend::db::users::sessions::remove_expired_identity_rows(&pool).await {
            Ok(0) => {}
            Ok(changed) => info!(changed, "cleaned up expired identity rows"),
            Err(_) => warn!("could not clean up expired identity rows"),
        }
        match storage.remove_stale_staging().await {
            Ok(0) => {}
            Ok(removed) => info!(removed, "removed stale upload staging files"),
            Err(error) => warn!(%error, "could not remove stale upload staging files"),
        }
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
    let settings = cluster::Settings::from_lookup(&|key| {
        // Empty counts as unset, as in `Settings::from_lookup`, so the fallback below applies.
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| (key == "IROH_BIND").then(|| "127.0.0.1:0".into()))
    })?
    .context("actions-worker needs cluster settings")?;
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
            context: context.clone(),
            peers,
        },
        config,
        true,
        state_dir,
    );
    workers.startup().await?;
    info!("database-free actions-worker ready; no web, SSH or cluster listener");
    let result = signal.await;
    info!("shutting down");
    let stopped = workers.shutdown().await;
    context.iroh.close().await;
    result.and(stopped)
}

pub async fn serve(pool: DbPool, config: ServeConfig) -> Result<()> {
    let repos_path = prepare_repository_root(&config.repos_path)?;
    // Cluster mode is opt-in. Without cluster settings no cluster socket is opened
    // and this node behaves as a single machine.
    let member = match cluster::Settings::from_env()? {
        Some(settings) => Some(
            cluster::start(
                &pool,
                &config.state_dir,
                &repos_path,
                &config.roles,
                settings,
            )
            .await?,
        ),
        None => None,
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
    state.notification_broadcaster.start().await;
    Ok::<_, anyhow::Error>((transport, repos_path, state))
    }.await;
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
        config.roles.contains(NodeRole::ActionsWorker),
        ci_runs.queued.clone(),
        &config.state_dir,
    );
    workers.startup().await?;
    let (stop_expiry, expiry_stop) = tokio::sync::watch::channel(false);
    let expiry = config.roles.contains(NodeRole::Web).then(|| {
        tokio::spawn(remove_expired_rows(
            pool.clone(),
            state.storage.clone(),
            expiry_stop,
        ))
    });
    // Published only after the services above are set up. The listeners start next; if
    // one fails, the node shuts down and withdraws.
    if let Some(member) = &member {
        member.publish().await?;
    }
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
        result = shutdown_signal() => (result, true),
    };
    info!("shutting down");
    // Every local owner is told to stop before anything waits on one of them.
    if signalled {
        stop_listeners.send_replace(true);
    }
    stop_expiry.send_replace(true);
    stop_replicas.send_replace(true);
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
    if let Some(mut expiry) = expiry {
        let finished = tokio::time::timeout_at(deadline, &mut expiry).await;
        if finished.is_err() {
            expiry.abort();
            let _ = expiry.await;
        }
    }
    if let Some(mut replicas) = replicas {
        let finished = tokio::time::timeout_at(deadline, &mut replicas).await;
        if finished.is_err() {
            replicas.abort();
            let _ = replicas.await;
        }
    }
    let stopped = workers.shutdown().await;
    if let Some(member) = &member {
        member.shutdown().await;
    }
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
        let sweep = tokio::spawn(remove_expired_rows(pool.clone(), storage, receiver));
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
