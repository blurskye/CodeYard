#[derive(Debug, thiserror::Error)]
pub enum ImportRefusal {
    #[error("Repository administration denied")]
    Denied,
    #[error("An import is already running")]
    Running,
}
