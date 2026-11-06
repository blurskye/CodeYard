use codeyard_model::node_roles::NodeRoles;
use std::net::SocketAddr;
use std::path::PathBuf;

pub struct ServeConfig {
    pub directory: Option<super::router::DirectorySettings>,
    /// The search index, when it is served apart from the directory.
    pub search: Option<super::router::DirectorySettings>,
    pub cell_rpc_address: Option<SocketAddr>,
    pub roles: NodeRoles,
    pub repos_path: PathBuf,
    pub state_dir: PathBuf,
    pub ssh_mode: codeyard_support::ssh_mode::SshMode,
    pub ssh_socket: PathBuf,
    pub ssh_git_user: String,
    pub ssh_install_manifest: PathBuf,
    pub ssh_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub host_key: PathBuf,
}
