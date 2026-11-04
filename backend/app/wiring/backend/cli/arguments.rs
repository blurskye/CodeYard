use clap::Parser;
use clap::Subcommand;
use std::net::SocketAddr;
use std::path::PathBuf;

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
        /// Roles of this machine: server (web pages, API, Git storage and SSH) and runner
        /// (Actions jobs). A runner may run alone on another machine. In cluster mode the
        /// server's parts are the web, storage and ssh roles, on separate machines.
        #[arg(long, env = "GITSERV_NODE_ROLES", default_value = "server,runner")]
        roles: codeyard_model::node_roles::NodeRoles,

        /// Turn cluster mode on first (PostgreSQL only), so this node can take the web,
        /// storage and ssh roles. Same as running `cluster enable` before `serve`.
        #[arg(long)]
        cluster: bool,

        /// SSH transport: auto, openssh, builtin or disabled
        #[arg(long, env = "SSH_MODE", value_enum, default_value = "auto")]
        ssh_mode: codeyard_support::ssh_mode::SshMode,

        /// Private app broker socket for host OpenSSH helpers
        #[arg(long, env = "SSH_SOCKET_PATH", default_value = codeyard_support::ssh_mode::DEFAULT_SOCKET)]
        ssh_socket: PathBuf,

        /// Dedicated host SSH login, distinct from any existing Git login
        #[arg(long, env = "SSH_GIT_USER", default_value = "git")]
        ssh_git_user: String,

        /// Root-owned marker created by the opt-in OpenSSH installer
        #[arg(long, env = "SSH_INSTALL_MANIFEST", default_value = codeyard_support::ssh_mode::DEFAULT_MANIFEST)]
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
        action: codeyard_ssh::openssh::Commands,
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
        action: crate::backend::cli::cluster::Command,
    },

    /// Database tools that do not start the server
    Db {
        #[command(subcommand)]
        action: DbAction,
    },

    /// Create the database and run migrations
    Init,

    /// Copy the database, then every repository, then local uploads into a directory.
    /// Runs while the server serves; a second run into the same directory copies only
    /// what changed. Single-node installations only.
    Backup {
        /// Backup directory, created if missing
        dir: PathBuf,
    },

    /// Restore a backup into an empty database, REPOS_PATH and upload directory, then
    /// run `repo verify --repair` and `repo rebuild`
    Restore {
        /// Backup directory written by `codeyard backup`
        dir: PathBuf,
    },
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
    /// Check every repository's Git data and pull request refs against the database
    Verify {
        /// Bring the database and pull request refs in line with Git, and put back
        /// repositories the database still lists from the kept deleted copies. Stop the
        /// server first.
        #[arg(long)]
        repair: bool,
    },
    /// Rebuild pack indexes, commit graphs and language statistics. Stop the server first.
    Rebuild,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_takes_the_cluster_flag_with_cluster_roles() {
        let cli =
            Cli::try_parse_from(["codeyard", "serve", "--cluster", "--roles", "web,ssh"]).unwrap();
        let Commands::Serve { cluster, roles, .. } = cli.command else {
            panic!("serve expected");
        };
        assert!(cluster);
        assert_eq!(roles.to_string(), "web,ssh");
    }
}
