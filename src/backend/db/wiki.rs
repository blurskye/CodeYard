//! Repository wiki pages, their revisions and the redirects left by renamed pages. Writers change
//! pages; readers read them. A write checks the actor, that the repository is not archived and that
//! its wiki is on, in the same transaction as the change.
use crate::backend::db::models::{WikiPage, WikiPageSummary, WikiRevision};
use crate::backend::db::DbPool;
use anyhow::Result;

/// Pages a repository may hold.
pub const PAGE_LIMIT: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WikiRefusal {
    #[error("Wiki change denied")]
    Denied,
    #[error("Wiki page not found")]
    NotFound,
    #[error("A page with this address already exists")]
    SlugTaken,
    #[error("The page changed since it was opened")]
    Stale,
    #[error("The wiki has reached its page limit")]
    PageLimit,
}

/// The content a writer saves. The slug follows the title; the caller computes it.
#[derive(Debug, Clone, Copy)]
pub struct WikiContent<'a> {
    pub slug: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub message: &'a str,
}

/// The LIKE pattern for a case-insensitive substring search. `%`, `_` and `\` match themselves.
pub fn like_pattern(query: &str) -> String {
    let mut pattern = String::from("%");
    for ch in query.to_lowercase().chars() {
        if matches!(ch, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

native_operation!(crate::backend::db::postgres::wiki::create_page,
    crate::backend::db::sqlite::wiki::create_page,
    fn create_page(pool: &DbPool, actor_id: i64, repo_id: i64, content: &WikiContent<'_>) -> Result<WikiPage>);

// `expected_revision` is the head the writer opened. Any other head refuses the save.
native_operation!(crate::backend::db::postgres::wiki::edit_page,
    crate::backend::db::sqlite::wiki::edit_page,
    fn edit_page(pool: &DbPool, actor_id: i64, repo_id: i64, page_id: i64, expected_revision: i64, content: &WikiContent<'_>) -> Result<WikiPage>);

native_operation!(crate::backend::db::postgres::wiki::delete_page,
    crate::backend::db::sqlite::wiki::delete_page,
    fn delete_page(pool: &DbPool, actor_id: i64, repo_id: i64, page_id: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::wiki::set_wiki_enabled,
    crate::backend::db::sqlite::wiki::set_wiki_enabled,
    fn set_wiki_enabled(pool: &DbPool, actor_id: i64, repo_id: i64, enabled: bool) -> Result<()>);

native_operation!(crate::backend::db::postgres::wiki::get_page_by_slug,
    crate::backend::db::sqlite::wiki::get_page_by_slug,
    fn get_page_by_slug(pool: &DbPool, repo_id: i64, slug: &str) -> Result<Option<WikiPage>>);

// The current slug of the page an old slug was renamed to, if any.
native_operation!(crate::backend::db::postgres::wiki::redirect_target,
    crate::backend::db::sqlite::wiki::redirect_target,
    fn redirect_target(pool: &DbPool, repo_id: i64, old_slug: &str) -> Result<Option<String>>);

// Alphabetical by title. `query` filters by a case-insensitive match on title or slug.
native_operation!(crate::backend::db::postgres::wiki::list_pages,
    crate::backend::db::sqlite::wiki::list_pages,
    fn list_pages(pool: &DbPool, repo_id: i64, query: Option<&str>) -> Result<Vec<WikiPageSummary>>);

// Newest first, without bodies.
native_operation!(crate::backend::db::postgres::wiki::list_revisions,
    crate::backend::db::sqlite::wiki::list_revisions,
    fn list_revisions(pool: &DbPool, page_id: i64) -> Result<Vec<WikiRevision>>);

native_operation!(crate::backend::db::postgres::wiki::get_revision,
    crate::backend::db::sqlite::wiki::get_revision,
    fn get_revision(pool: &DbPool, page_id: i64, number: i64) -> Result<Option<WikiRevision>>);
