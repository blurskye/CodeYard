#[derive(Clone)]
pub enum CiCredential {
    Session(String),
    Token {
        id: i64,
        hash: String,
        permission: &'static str,
    },
}
