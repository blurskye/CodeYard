//! Host OpenSSH authenticates; these credential-free helpers only talk to the
//! persistent application's private broker. The broker UID (and root) is trusted.
//! CI retains the existing trusted-workflow-only runner assumption: write access
//! must not be granted to untrusted workflows on a host-executing runner.
use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod cli;

/// Nested under the application's `ssh` CLI command by the startup owner.
#[derive(Debug, Subcommand)]
pub enum Commands {
    Keys {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long, default_value = "git")]
        expected_user: String,
        #[arg(long)]
        serve_command: PathBuf,
        user: String,
        key_type: String,
        base64_key: String,
    },
    Serve {
        #[arg(long, value_parser = positive_id)]
        key_id: i64,
        #[arg(long)]
        socket: PathBuf,
    },
    PreReceive {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long, value_parser = positive_id)]
        key_id: i64,
        #[arg(long, value_parser = positive_id)]
        repo_id: i64,
    },
    PostReceive {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long, value_parser = positive_id)]
        key_id: i64,
        #[arg(long, value_parser = positive_id)]
        repo_id: i64,
    },
}

fn positive_id(raw: &str) -> std::result::Result<i64, String> {
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| "expected a positive key/repository ID".to_owned())
}

/// Call before dotenv, DB initialization, migrations, or repository creation.
/// Diagnostics go to stderr; the caller must honor the returned process status.
pub async fn run_command(command: Commands) -> Result<i32> {
    #[cfg(target_os = "linux")]
    {
        cli::run(command).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
        anyhow::bail!("OpenSSH gateway is currently available only on Linux Unix sockets")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct FixtureCli {
        #[command(subcommand)]
        command: Commands,
    }

    #[test]
    fn frozen_cli_commands_and_positive_ids() {
        for arguments in [
            vec![
                "fixture",
                "keys",
                "--socket",
                "/run/git/broker.sock",
                "--expected-user",
                "git",
                "--serve-command",
                "/opt/git/helper",
                "git",
                "ssh-ed25519",
                "blob",
            ],
            vec![
                "fixture",
                "serve",
                "--socket",
                "/run/git/broker.sock",
                "--key-id",
                "1",
            ],
            vec![
                "fixture",
                "pre-receive",
                "--socket",
                "/run/git/broker.sock",
                "--key-id",
                "1",
                "--repo-id",
                "2",
            ],
            vec![
                "fixture",
                "post-receive",
                "--socket",
                "/run/git/broker.sock",
                "--key-id",
                "1",
                "--repo-id",
                "2",
            ],
        ] {
            assert!(FixtureCli::try_parse_from(arguments).is_ok());
        }
        for id in ["0", "-1", "9223372036854775808", "not-an-id"] {
            assert!(FixtureCli::try_parse_from([
                "fixture",
                "serve",
                "--socket",
                "/run/git/broker.sock",
                "--key-id",
                id
            ])
            .is_err());
        }
    }
}
