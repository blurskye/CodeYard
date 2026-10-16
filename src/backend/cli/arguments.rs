use crate::backend::{node, ssh};
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(name = "codeyard", version)]
#[command(about = "Self-hosted Git forge", long_about = None)]
pub(super) struct Cli {
    /// Server state: default SQLite database, local object storage
    #[arg(long, env = "GITSERV_STATE_DIR", default_value = "./data")]
    pub(super) state_dir: PathBuf,

    /// Path to store repositories
    #[arg(long, env = "REPOS_PATH", default_value = "./repos")]
    pub(super) repos_path: PathBuf,

    #[command(subcommand)]
    pub(super) command: Commands,
}

#[derive(Subcommand)]
pub(super) enum Commands {
    /// Start HTTP and the selected Git SSH transport
    Serve {
        /// Services to run on this node; all of them by default
        #[arg(
            long,
            env = "GITSERV_NODE_ROLES",
            default_value = "web,storage,ssh,actions-worker"
        )]
        roles: node::NodeRoles,

        /// SSH transport: auto, openssh, builtin or disabled
        #[arg(long, env = "SSH_MODE", value_enum, default_value = "auto")]
        ssh_mode: ssh::mode::SshMode,

        /// Private app broker socket for host OpenSSH helpers
        #[arg(long, env = "SSH_SOCKET_PATH", default_value = ssh::mode::DEFAULT_SOCKET)]
        ssh_socket: PathBuf,

        /// Dedicated host SSH login, distinct from any existing Git login
        #[arg(long, env = "SSH_GIT_USER", default_value = "git")]
        ssh_git_user: String,

        /// Root-owned marker created by the opt-in OpenSSH installer
        #[arg(long, env = "SSH_INSTALL_MANIFEST", default_value = ssh::mode::DEFAULT_MANIFEST)]
        ssh_install_manifest: PathBuf,

        /// Built-in SSH bind address (unused in openssh/disabled mode)
        #[arg(long, env = "SSH_ADDR", default_value = "0.0.0.0:2222")]
        ssh_addr: SocketAddr,

        /// HTTP server bind address
        #[arg(long, env = "HTTP_ADDR", default_value = "0.0.0.0:3000")]
        http_addr: SocketAddr,

        /// Path to host key (will be generated if missing)
        #[arg(long, env = "HOST_KEY_PATH", default_value = "./host_key")]
        host_key: PathBuf,
    },

    /// Host OpenSSH key lookup, forced command and receive hooks (no DB credentials)
    #[cfg(unix)]
    Ssh {
        #[command(subcommand)]
        action: ssh::openssh::Commands,
    },

    /// User management
    User {
        #[command(subcommand)]
        action: UserAction,
    },

    /// SSH key management
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },

    /// Repository management
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },

    /// Cluster membership: keys, invites and approvals
    Cluster {
        #[command(subcommand)]
        action: super::cluster::Command,
    },

    /// Database tools that do not start the server
    Db {
        #[command(subcommand)]
        action: DbAction,
    },

    /// Create the database and run migrations
    Init,
}

#[derive(Subcommand)]
pub(super) enum DbAction {
    /// Copy every table to an empty database of the other engine. Stop the server
    /// first. Repository directories, uploaded files and key files are not copied.
    Copy {
        /// Source database: sqlite://PATH or postgres://... (or COPY_FROM_DATABASE_URL)
        // Optional here because the first parse runs before .env is loaded. The URLs may
        // hold passwords, so help names the variables without printing their values.
        #[arg(long, env = "COPY_FROM_DATABASE_URL", hide_env_values = true)]
        from: Option<String>,
        /// Empty destination database, migrated first if it has no schema yet
        /// (or COPY_TO_DATABASE_URL)
        #[arg(long, env = "COPY_TO_DATABASE_URL", hide_env_values = true)]
        to: Option<String>,
    },
}

#[derive(Subcommand)]
pub(super) enum UserAction {
    /// Create a new user
    Create {
        /// Username
        username: String,
        /// Email address
        #[arg(long)]
        email: Option<String>,
    },
    /// Complete setup with a new administrator, preserving any existing accounts
    Bootstrap {
        /// New administrator username (existing accounts are never adopted)
        username: String,
        /// Read the password from standard input, not command-line arguments
        #[arg(long, required = true)]
        password_stdin: bool,
        /// Node access mode
        #[arg(long, default_value = "public", value_parser = ["public", "private"])]
        mode: String,
    },
    /// List all users
    List,
}

#[derive(Subcommand)]
pub(super) enum KeyAction {
    /// Add an SSH key to a user
    Add {
        /// Username
        username: String,
        /// Key name/label
        name: String,
        /// Public key (or use --file)
        #[arg(long)]
        key: Option<String>,
        /// Path to public key file
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// List SSH keys for a user
    List {
        /// Username
        username: String,
    },
}

#[derive(Subcommand)]
pub(super) enum RepoAction {
    /// Create a new repository
    Create {
        /// Owner username
        owner: String,
        /// Repository name
        name: String,
        /// Description
        #[arg(long)]
        description: Option<String>,
        /// Private repository
        #[arg(long)]
        private: bool,
    },
    /// List repositories for a user
    List {
        /// Owner username
        owner: String,
    },
    /// Show repository info
    Info {
        /// Owner username
        owner: String,
        /// Repository name
        name: String,
    },
    /// Grant access to a repository
    Grant {
        /// Owner username
        owner: String,
        /// Repository name
        repo: String,
        /// Username to grant access to
        username: String,
        /// Access level: read, write, admin
        #[arg(long, default_value = "read")]
        level: String,
    },
}
