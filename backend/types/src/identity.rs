pub const CLI_CLIENT: &str = "178c6fc778ccc68e1d6a";
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SshKeyWriteRefusal {
    #[error("Invalid SSH public key")]
    InvalidKey,
    #[error("This SSH public key is already registered")]
    AlreadyUsed,
    #[error("SSH keys cannot be changed while the account is restricted")]
    AccountRestricted,
    #[error("SSH key enrollment requires a live session")]
    InvalidSession,
}
