use codeyard_db_core::queries::users::accounts::AccountCredential;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub enum GistCredential {
    Session(String),
    Token(i64, String),
}
impl std::fmt::Debug for GistCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GistCredential([redacted])")
    }
}
impl GistCredential {
    pub fn borrowed(&self) -> AccountCredential<'_> {
        match self {
            Self::Session(session) => AccountCredential::Session(session),
            Self::Token(id, hash) => AccountCredential::Token(*id, hash),
        }
    }
    pub fn from_credential(credential: &AccountCredential<'_>) -> Self {
        match credential {
            AccountCredential::Session(session) => Self::Session((*session).into()),
            AccountCredential::Token(id, hash) => Self::Token(*id, (*hash).into()),
        }
    }
}
