/// A settings operation retains its originating credential alongside account data.
pub enum AccountCredential<'a> {
    Session(&'a str),
    Token(i64, &'a str),
    ForeignToken(i64, &'a str),
}

/// An account credential retained across queues, owned tasks and authenticated RPC.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum OwnedAccountCredential {
    Session(String),
    /// The token's id and stored hash, so a regenerated token stops the work it started.
    Token(i64, String),
    ForeignToken(i64, String),
}

impl std::fmt::Debug for OwnedAccountCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(_) => f.write_str("Session(<redacted>)"),
            Self::ForeignToken(id, _) => f.debug_tuple("ForeignToken").field(id).finish(),
            Self::Token(id, _) => f.debug_tuple("Token").field(id).finish(),
        }
    }
}

impl OwnedAccountCredential {
    pub(crate) fn as_credential(&self) -> AccountCredential<'_> {
        match self {
            Self::Session(session) => AccountCredential::Session(session),
            Self::Token(id, hash) => AccountCredential::Token(*id, hash),
            Self::ForeignToken(id, hash) => AccountCredential::ForeignToken(*id, hash),
        }
    }
}

impl From<&str> for OwnedAccountCredential {
    fn from(session: &str) -> Self {
        Self::Session(session.to_owned())
    }
}

impl From<&String> for OwnedAccountCredential {
    fn from(session: &String) -> Self {
        Self::Session(session.clone())
    }
}

impl From<&AccountCredential<'_>> for OwnedAccountCredential {
    fn from(credential: &AccountCredential<'_>) -> Self {
        match credential {
            AccountCredential::Session(value) => Self::Session((*value).to_owned()),
            AccountCredential::Token(id, hash) => Self::Token(*id, (*hash).to_owned()),
            AccountCredential::ForeignToken(id, hash) => {
                Self::ForeignToken(*id, (*hash).to_owned())
            }
        }
    }
}
