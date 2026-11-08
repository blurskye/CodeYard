use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Autolink {
    pub id: i64,
    pub repo_id: i64,
    pub key_prefix: String,
    pub url_template: String,
    pub is_alphanumeric: bool,
}
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Repository administration required")]
    Denied,
    #[error("Autolink not found")]
    Missing,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("That key prefix already has an autolink")]
    Duplicate,
    #[error("A repository may have at most 50 autolinks")]
    Limit,
}
pub struct NewAutolink {
    pub key_prefix: String,
    pub url_template: String,
    pub is_alphanumeric: bool,
}
impl NewAutolink {
    pub fn new(
        prefix: &str,
        url: &str,
        is_alphanumeric: bool,
    ) -> std::result::Result<Self, Refusal> {
        if prefix.is_empty()
            || prefix.chars().count() > 20
            || prefix
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(Refusal::Invalid(
                "The key prefix must contain 1 to 20 non-whitespace characters",
            ));
        }
        if url.len() > 4096
            || !url.contains("<num>")
            || url.chars().any(char::is_control)
            || !codeyard_types::links::valid_autolink_template(url)
        {
            return Err(Refusal::Invalid(
                "The URL template must contain <num> and be at most 4096 bytes",
            ));
        }
        let parsed = reqwest::Url::parse(&url.replace("<num>", "1"))
            .map_err(|_| Refusal::Invalid("Invalid autolink URL template"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(Refusal::Invalid(
                "Autolink URLs must use HTTP or HTTPS without credentials",
            ));
        }
        Ok(Self {
            key_prefix: prefix.to_owned(),
            url_template: url.to_owned(),
            is_alphanumeric,
        })
    }
}

native_operation!(crate::postgres::repositories::autolinks::create,crate::sqlite::repositories::autolinks::create,
    fn create(pool:&DbPool,actor:impl Into<codeyard_db_core::queries::credentials::Actor>,repo:i64,input:&NewAutolink)->Result<Autolink>);
native_operation!(crate::postgres::repositories::autolinks::list,crate::sqlite::repositories::autolinks::list,
    fn list(pool:&DbPool,actor:i64,repo:i64,limit:i64,offset:i64)->Result<Vec<Autolink>>);
native_operation!(crate::postgres::repositories::autolinks::get,crate::sqlite::repositories::autolinks::get,
    fn get(pool:&DbPool,actor:i64,repo:i64,id:i64)->Result<Autolink>);
native_operation!(crate::postgres::repositories::autolinks::remove,crate::sqlite::repositories::autolinks::remove,
    fn remove(pool:&DbPool,actor:impl Into<codeyard_db_core::queries::credentials::Actor>,repo:i64,id:i64)->Result<()>);
// Trusted Markdown callers have already authorized their repository; no target is resolved here.
native_operation!(crate::postgres::repositories::autolinks::for_rendering,crate::sqlite::repositories::autolinks::for_rendering,
    fn for_rendering(pool:&DbPool,repo:i64)->Result<Vec<Autolink>>);
