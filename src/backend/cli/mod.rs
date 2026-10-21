pub mod commands;

pub use commands::*;

pub mod arguments;
pub mod cluster;
use arguments::{Cli, Commands, DbAction, KeyAction, RepoAction, UserAction};

use crate::backend::cluster as cluster_runtime;
use crate::backend::db;
use crate::backend::node;
use crate::backend::ssh;

use anyhow::{bail, Result};
use clap::Parser;
use std::{ffi::OsString, path::Path, time::Duration};
use tracing::info;
use tracing_subscriber::EnvFilter;

fn load_dotenv() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(error) if error.not_found() => Ok(()),
        // dotenvy's parse error includes the source line, which may contain a secret.
        Err(_) => bail!("could not load .env: the file is malformed or unreadable"),
    }
}

#[cfg(unix)]
fn set_private_umask() {
    // This is the native process boundary, before any normal application files or
    // listeners are created. OpenSSH helpers return above and keep their host policy.
    unsafe {
        libc::umask(0o077);
    }
}

#[cfg(not(unix))]
fn set_private_umask() {}

fn prepare_state_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn database_url_from_env() -> Result<Option<String>> {
    match std::env::var("DATABASE_URL") {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => bail!("DATABASE_URL is not valid UTF-8"),
    }
}

fn cluster_join_ticket_from_env() -> Result<String> {
    match std::env::var("CLUSTER_JOIN_TOKEN") {
        Ok(value) => Ok(value),
        Err(std::env::VarError::NotPresent) => Ok(String::new()),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("CLUSTER_JOIN_TOKEN is not valid UTF-8")
        }
    }
}

fn root_command_index(arguments: &[OsString]) -> Option<usize> {
    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        if matches!(argument.as_ref(), "--state-dir" | "--repos-path") {
            index += 2;
        } else if argument.starts_with("--state-dir=") || argument.starts_with("--repos-path=") {
            index += 1;
        } else if argument.starts_with('-') {
            return None;
        } else {
            return Some(index);
        }
    }
    None
}

/// `db copy` takes its URLs as arguments, and a password in argv is visible to other
/// users, so such a URL must come from COPY_FROM_DATABASE_URL or COPY_TO_DATABASE_URL.
fn copy_url_argument_has_password(arguments: &[OsString]) -> bool {
    let mut values = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        let argument = argument.to_string_lossy();
        for flag in ["--from", "--to"] {
            if argument == flag {
                values.extend(
                    arguments
                        .get(index + 1)
                        .map(|value| value.to_string_lossy().into_owned()),
                );
            } else if let Some(value) = argument
                .strip_prefix(flag)
                .and_then(|rest| rest.strip_prefix('='))
            {
                values.push(value.to_owned());
            }
        }
    }
    values.iter().any(|value| {
        url::Url::parse(value).is_ok_and(|url| {
            url.password().is_some()
                || url
                    .query_pairs()
                    .any(|(name, _)| name.eq_ignore_ascii_case("password"))
        })
    })
}

fn join_ticket_is_in_argv(arguments: &[OsString]) -> bool {
    let Some(cluster) = root_command_index(arguments) else {
        return false;
    };
    if arguments[cluster] != "cluster"
        || arguments
            .get(cluster + 1)
            .is_none_or(|command| command != "join")
    {
        return false;
    }
    let mut index = cluster + 2;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        if argument == "--name" {
            index += 2;
        } else if argument.starts_with("--name=") || matches!(argument.as_ref(), "--help" | "-h") {
            index += 1;
        } else {
            return true;
        }
    }
    false
}

pub fn run() -> Result<()> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments.iter().skip(1).any(|argument| {
        let argument = argument.to_string_lossy();
        argument == "--database-url" || argument.starts_with("--database-url=")
    }) {
        bail!(
            "--database-url is refused because command-line arguments are visible to other users; set DATABASE_URL in the environment or .env"
        );
    }
    if copy_url_argument_has_password(&arguments) {
        bail!(
            "a database URL with a password is refused on the command line because arguments are visible to other users; set COPY_FROM_DATABASE_URL or COPY_TO_DATABASE_URL in the environment or .env"
        );
    }
    if join_ticket_is_in_argv(&arguments) {
        bail!(
            "command-line input is not a Codeyard join ticket source; clustering needs PostgreSQL and tickets must be supplied through CLUSTER_JOIN_TOKEN in the environment or .env, then run `codeyard cluster join`"
        );
    }

    // Parse once before dotenv so help, argument errors and sshd helpers never need
    // application configuration. Normal commands are parsed again after dotenv.
    let early_cli = Cli::parse_from(arguments.clone());
    #[cfg(unix)]
    if let Commands::Ssh { action } = early_cli.command {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(EnvFilter::new("warn"))
            .init();
        // Key lookup is a short-lived client, not another application runtime
        // with one worker thread per host CPU for each authentication attempt.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let status = runtime.block_on(ssh::openssh::run_command(action))?;
        std::process::exit(status.clamp(0, 255));
    }
    #[cfg(not(unix))]
    drop(early_cli);
    set_private_umask();
    load_dotenv()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(run_application(arguments));
    // Blocking tasks such as workspace removal cannot be cancelled; do not wait for them past the shutdown grace.
    runtime.shutdown_timeout(Duration::from_secs(5));
    result
}

async fn run_application(arguments: Vec<OsString>) -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mut cli = Cli::parse_from(arguments);
    if let Commands::Cluster {
        action: self::cluster::Command::Join { ticket, .. },
    } = &mut cli.command
    {
        *ticket = cluster_join_ticket_from_env()?;
    }
    // Copying takes its own URLs and must not read DATABASE_URL.
    if let Commands::Db { action } = &cli.command {
        return match action {
            DbAction::Copy {
                from: Some(from),
                to: Some(to),
            } => self::copy_database_cmd(from, to).await,
            DbAction::Copy { .. } => bail!(
                "db copy needs --from and --to, or COPY_FROM_DATABASE_URL and COPY_TO_DATABASE_URL"
            ),
        };
    }
    let configured_database_url = database_url_from_env()?;
    // Making a key and joining a cluster work on a machine with no database.
    if let Commands::Cluster { action } = &cli.command {
        if !action.needs_database() {
            return self::cluster::run_offline(
                action,
                &cli.state_dir,
                configured_database_url.as_deref(),
            )
            .await;
        }
    }
    if let Commands::Serve { roles, .. } = &cli.command {
        if roles.worker_only() && configured_database_url.is_none() {
            prepare_state_directory(&cli.state_dir)?;
            return node::serve_worker(&cli.state_dir).await;
        }
        if configured_database_url.is_none() {
            anyhow::ensure!(roles == &node::NodeRoles::default() && cluster_runtime::Settings::from_env()?.is_none(),
                "web, SSH and storage roles require DATABASE_URL (PostgreSQL for a cluster, sqlite://... for a single node); only actions-worker can run without a database");
        }
    }
    prepare_state_directory(&cli.state_dir)?;
    let database_url = match configured_database_url {
        Some(url) => url,
        None => {
            let path = std::path::absolute(cli.state_dir.join("gitserv.db"))?;
            let file_url = url::Url::from_file_path(path)
                .map_err(|_| anyhow::anyhow!("invalid state directory path"))?;
            format!("sqlite://{}?mode=rwc", file_url.path())
        }
    };
    let pool = db::init_db(&database_url)
        .await
        .map_err(|_| anyhow::anyhow!("could not open the configured application database"))?;
    db::run_migrations(&pool).await?;
    let command = async {
        match cli.command {
            Commands::Init => {
                info!("Database initialized successfully");
                println!("Database initialized successfully.");
            }

            #[cfg(unix)]
            Commands::Ssh { .. } => {
                unreachable!("OpenSSH commands are handled before store initialization")
            }

            Commands::Db { .. } => {
                unreachable!("database copy runs before the application database opens")
            }

            Commands::Serve {
                roles,
                ssh_mode,
                ssh_socket,
                ssh_git_user,
                ssh_install_manifest,
                ssh_addr,
                http_addr,
                host_key,
            } => {
                node::serve(
                    pool.clone(),
                    node::ServeConfig {
                        roles,
                        repos_path: cli.repos_path,
                        state_dir: cli.state_dir,
                        ssh_mode,
                        ssh_socket,
                        ssh_git_user,
                        ssh_install_manifest,
                        ssh_addr,
                        http_addr,
                        host_key,
                    },
                )
                .await?;
            }

            Commands::User { action } => match action {
                UserAction::Create { username, email } => {
                    self::create_user_cmd(&pool, &username, email.as_deref()).await?;
                }
                UserAction::Bootstrap {
                    username,
                    password_stdin: _,
                    mode,
                } => {
                    self::bootstrap_user_cmd(&pool, &username, &mode).await?;
                }
                UserAction::List => {
                    self::list_users_cmd(&pool).await?;
                }
            },

            Commands::Key { action } => match action {
                KeyAction::Add {
                    username,
                    name,
                    key,
                    file,
                } => {
                    if let Some(key_path) = file {
                        self::add_key_from_file_cmd(&pool, &username, &name, &key_path).await?;
                    } else if let Some(public_key) = key {
                        self::add_key_cmd(&pool, &username, &name, &public_key).await?;
                    } else {
                        return Err(anyhow::anyhow!("Must provide --key or --file"));
                    }
                }
                KeyAction::List { username } => {
                    self::list_keys_cmd(&pool, &username).await?;
                }
            },

            Commands::Cluster { action } => {
                self::cluster::run(&action, &pool, &cli.state_dir, &cli.repos_path).await?;
            }

            Commands::Repo { action } => match action {
                RepoAction::Create {
                    owner,
                    name,
                    description,
                    private,
                } => {
                    self::create_repo_cmd(
                        &pool,
                        &cli.state_dir,
                        &cli.repos_path,
                        &owner,
                        &name,
                        description.as_deref(),
                        private,
                    )
                    .await?;
                }
                RepoAction::List { owner } => {
                    self::list_repos_cmd(&pool, &owner).await?;
                }
                RepoAction::Info { owner, name } => {
                    self::repo_info_cmd(&pool, &cli.state_dir, &cli.repos_path, &owner, &name)
                        .await?;
                }
                RepoAction::Grant {
                    owner,
                    repo,
                    username,
                    level,
                } => {
                    self::grant_access_cmd(&pool, &owner, &repo, &username, &level).await?;
                }
            },
        }
        Ok(())
    };
    let result = command.await;
    pool.close().await;
    result
}
