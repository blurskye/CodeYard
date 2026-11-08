#[derive(Clone)]
pub enum CiCredential {
    Session(String),
    ForeignToken {
        id: i64,
        hash: String,
        permission: &'static str,
    },
    Token {
        id: i64,
        hash: String,
        permission: &'static str,
    },
}
