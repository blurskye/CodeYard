//! Optional operator-owned machine provisioning. Commands come only from the environment.
use anyhow::{ensure, Context, Result};
use codeyard_cluster_core::{
    pairing::{create_invite, InviteOptions},
    transport::NodeAddress,
};
use codeyard_db_core::queries::credentials::Actor;
use codeyard_repos_core::context::NodeContext;
use serde::Serialize;
use std::{process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Serialize)]
pub struct Added {
    pub ticket: String,
    /// A non-successful command may already have created machines; the ticket stays usable.
    pub provisioning: &'static str,
}

pub async fn add(
    context: &NodeContext,
    authority: Option<&Actor>,
    role: &str,
    count: i32,
) -> Result<Added> {
    ensure!(
        matches!(role, "web" | "ssh" | "storage" | "runner"),
        "invalid node role"
    );
    ensure!((1..=1000).contains(&count), "invalid node count");
    let command = std::env::var("NODE_PROVISION_COMMAND")
        .ok()
        .filter(|command| !command.is_empty());
    ensure!(
        command
            .as_ref()
            .is_none_or(|command| command.len() <= 16_384 && !command.contains('\0')),
        "invalid provisioning command"
    );
    let seconds: u64 = std::env::var("NODE_PROVISION_TIMEOUT_SECONDS")
        .map(|value| value.parse())
        .unwrap_or(Ok(300))?;
    ensure!(
        (1..=3600).contains(&seconds),
        "invalid provisioning timeout"
    );
    let pool = context.database()?;
    ensure!(
        role == "runner" || codeyard_db_cluster::cluster::cluster_enabled(pool).await?,
        "cluster mode is required for this role"
    );
    let node = codeyard_db_cluster::cluster::get_node_by_id(pool, context.node_id)
        .await?
        .context("node unavailable")?;
    ensure!(
        node.public_key == context.key.public_hex() && node.is_online() && node.state == "active",
        "node unavailable"
    );
    let address: NodeAddress = serde_json::from_str(
        node.addr_hint
            .as_deref()
            .context("node address unavailable")?,
    )?;
    let options = InviteOptions {
        leader: authority.is_none().then_some(context.node_id),
        authority,
        max_uses: Some(count),
        roles: role.parse()?,
        reusable: count > 1,
        requires_approval: false,
        expires_in: Duration::from_secs(3600),
    };
    let ticket = create_invite(pool, context.key.public_key(), address, &options)
        .await?
        .encode();
    let provisioning = if let Some(command) = command {
        if let Some(actor) = authority {
            let user =
                codeyard_identity::repository_access::credential_user(pool, actor, true).await?;
            ensure!(user.is_admin, codeyard_db_cluster::drain::Refusal::Denied);
        }
        match run(&command, role, count, &ticket, Duration::from_secs(seconds)).await {
            Ok(true) => "completed",
            Ok(false) => "failed",
            Err(error) => {
                tracing::warn!(%error, role, count, "node provisioning outcome unknown");
                "unknown"
            }
        }
    } else {
        "not_configured"
    };
    tracing::info!(role, count, provisioning, "node invitation created");
    Ok(Added {
        ticket,
        provisioning,
    })
}

async fn run(
    command: &str,
    role: &str,
    count: i32,
    ticket: &str,
    timeout: Duration,
) -> Result<bool> {
    use codeyard_actions::process::ProcessGroup;
    static SLOTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let _permit = SLOTS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
        .context("node provisioning is busy")?;
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", command])
        .env("CODEYARD_NODE_ROLE", role)
        .env("CODEYARD_NODE_COUNT", count.to_string())
        .env("CLUSTER_JOIN_TOKEN", ticket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("node provisioning could not start")?;
    let mut group = ProcessGroup::new(child.id(), Duration::ZERO);
    let stdout = child
        .stdout
        .take()
        .context("provisioning stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("provisioning stderr unavailable")?;
    const LIMIT: u64 = 64 * 1024;
    let read = |stream: tokio::process::ChildStdout| async move {
        let mut bytes = Vec::new();
        stream.take(LIMIT + 1).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() as u64 <= LIMIT,
            "provisioning output limit exceeded"
        );
        Ok::<_, anyhow::Error>(bytes)
    };
    let work = async {
        let errors = async {
            let mut bytes = Vec::new();
            stderr.take(LIMIT + 1).read_to_end(&mut bytes).await?;
            ensure!(
                bytes.len() as u64 <= LIMIT,
                "provisioning output limit exceeded"
            );
            Ok::<_, anyhow::Error>(bytes)
        };
        let (stdout, stderr) = tokio::try_join!(read(stdout), errors)?;
        let status = child.wait().await?;
        group.disarm();
        for bytes in [stdout, stderr] {
            let mut output = String::from_utf8_lossy(&bytes).replace(ticket, "[redacted]");
            for (key, value) in std::env::vars_os() {
                let key = key.to_string_lossy();
                let value = value.to_string_lossy();
                if !value.is_empty()
                    && (key.contains("SECRET")
                        || key.contains("TOKEN")
                        || key.contains("PASSWORD")
                        || key.ends_with("KEY")
                        || key == "DATABASE_URL")
                {
                    output = output.replace(value.as_ref(), "[redacted]");
                }
            }
            let output: String = output
                .chars()
                .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
                .collect();
            tracing::info!(role, count, output, "node provisioning output");
        }
        Ok(status.success())
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => {
            group.kill();
            let _ = child.wait().await;
            anyhow::bail!("node provisioning timed out")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn provisioning_receives_role_count_and_ticket_without_shell_interpolation() {
        let _lock = TEST_LOCK.lock().await;
        assert!(run(
            "test \"$CODEYARD_NODE_ROLE\" = runner && test \"$CODEYARD_NODE_COUNT\" = 2 && test \"$CLUSTER_JOIN_TOKEN\" = 'ticket; false'",
            "runner", 2, "ticket; false", Duration::from_secs(2),
        ).await.unwrap());
        assert!(
            !run("exit 7", "runner", 1, "ticket", Duration::from_secs(2))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn provisioning_bounds_output_and_cleans_up_on_timeout() {
        let _lock = TEST_LOCK.lock().await;
        assert!(run(
            "yes oversized",
            "runner",
            1,
            "ticket",
            Duration::from_secs(2)
        )
        .await
        .is_err());
        assert!(run(
            "tail -f /dev/null",
            "runner",
            1,
            "ticket",
            Duration::from_millis(20)
        )
        .await
        .is_err());
    }
}
