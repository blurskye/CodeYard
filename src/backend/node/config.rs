use super::NodeRoles;
use crate::backend::ssh;
use std::{net::SocketAddr, path::PathBuf};

pub struct ServeConfig {
    pub roles: NodeRoles,
    pub repos_path: PathBuf,
    pub state_dir: PathBuf,
    pub ssh_mode: ssh::mode::SshMode,
    pub ssh_socket: PathBuf,
    pub ssh_git_user: String,
    pub ssh_install_manifest: PathBuf,
    pub ssh_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub host_key: PathBuf,
}
