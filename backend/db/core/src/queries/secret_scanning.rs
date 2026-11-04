use codeyard_support::secret_detection::patterns::Pattern;
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Secret scanning access denied")]
    Denied,
    #[error("Secret scanning resource not found")]
    Missing,
    #[error("Invalid secret scanning settings")]
    Invalid,
    #[error("Custom pattern version changed")]
    Version,
    #[error("Pattern configuration version changed")]
    Conflict,
}

#[derive(Clone, Default, sqlx::FromRow)]
pub struct Settings {
    pub enabled: bool,
    pub push_protection: bool,
}
