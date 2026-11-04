use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

#[derive(sqlx::FromRow)]
pub struct Author {
    pub id: i64,
    pub repo_id: i64,
    pub author_name: String,
    pub author_email: String,
    pub mapped_name: Option<String>,
    pub mapped_email: Option<String>,
}

pub(crate) fn identities(authors: &[(String, String)]) -> Vec<(String, &str, &str)> {
    use sha2::{Digest, Sha256};
    let mut rows = std::collections::BTreeMap::new();
    for (name, email) in authors {
        let mut digest = Sha256::new();
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update(email.as_bytes());
        rows.insert(
            format!("{:x}", digest.finalize()),
            (name.as_str(), email.as_str()),
        );
    }
    rows.into_iter()
        .map(|(digest, (name, email))| (digest, name, email))
        .collect()
}

native_operation!(crate::postgres::import_authors::record,
    crate::sqlite::import_authors::record,
    fn record(pool: &DbPool, actor: i64, repo: i64, authors: &[(String, String)]) -> Result<Vec<Author>>);
native_operation!(crate::postgres::import_authors::map,
    crate::sqlite::import_authors::map,
    fn map(pool: &DbPool, actor: i64, repo: i64, author: i64, name: Option<&str>, email: Option<&str>) -> Result<Option<Author>>);
