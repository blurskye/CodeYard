use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::fine_grained_tokens::Bounds;
use codeyard_db_core::queries::fine_grained_tokens::Policy;
use codeyard_db_core::queries::fine_grained_tokens::Token;
use codeyard_db_core::DbPool;

pub const MAX_REPOSITORIES: usize = 100;

pub struct RepositoryChoices {
    pub selected: Vec<(i64, String)>,
    pub matching: Vec<(i64, String)>,
}

native_operation!(crate::postgres::fine_grained_tokens::repository_choices, crate::sqlite::fine_grained_tokens::repository_choices,
    fn repository_choices(pool: &DbPool, actor: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, org: Option<i64>, query: &str, selected: &[i64]) -> Result<RepositoryChoices>);

pub struct NewToken<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub owner_org_id: Option<i64>,
    pub repository_access: &'a str,
    pub permissions: &'a str,
    pub repositories: &'a [i64],
    pub scope: &'a str,
    pub expires_at: Option<DateTime<Utc>>,
}

native_operation!(crate::postgres::fine_grained_tokens::create, crate::sqlite::fine_grained_tokens::create,
    /// Inserts the token while `session` is a live session of `user`. An organization owner
    /// needs the user's membership and a policy that allows fine-grained tokens; the token
    /// waits for review when the policy asks for approval and the user is not an owner.
    /// Returns the token id and its status.
    fn create(pool: &DbPool, user: i64, session: &str, hash: &str, token: &NewToken<'_>) -> Result<Option<(i64, String)>>);
native_operation!(crate::postgres::fine_grained_tokens::regenerate, crate::sqlite::fine_grained_tokens::regenerate,
    /// Replaces the hash and expiry while `session` is a live session of `user`.
    fn regenerate(pool: &DbPool, user: i64, session: &str, token: i64, hash: &str, expires_at: Option<DateTime<Utc>>) -> Result<bool>);
native_operation!(crate::postgres::fine_grained_tokens::get, crate::sqlite::fine_grained_tokens::get,
    fn get(pool: &DbPool, token: i64) -> Result<Option<Token>>);
native_operation!(crate::postgres::fine_grained_tokens::list_before, crate::sqlite::fine_grained_tokens::list_before,
    /// The user's newest 100 fine-grained tokens, older than `before` when it is given.
    fn list_before(pool: &DbPool, user: i64, before: Option<i64>) -> Result<Vec<Token>>);
native_operation!(codeyard_db_core::queries::postgres::fine_grained_tokens::bounds, codeyard_db_core::queries::sqlite::fine_grained_tokens::bounds,
    fn bounds(pool: &DbPool, token: i64) -> Result<Option<Bounds>>);
native_operation!(codeyard_db_core::queries::postgres::fine_grained_tokens::repositories, codeyard_db_core::queries::sqlite::fine_grained_tokens::repositories,
    /// The selected repositories' ids and names, at most 100, that are still in the token's
    /// owner domain and readable by `actor`. Only the token's user or an owner of its
    /// organization sees them, through a live `credential`.
    fn repositories(pool: &DbPool, token: i64, actor: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>) -> Result<Vec<(i64, String)>>);
pub async fn selected(pool: &DbPool, token: i64, repo: i64) -> Result<bool> {
    let native = match pool {
        DbPool::Postgres(pool) => {
            codeyard_db_core::queries::postgres::fine_grained_tokens::selected(pool, token, repo)
                .await?
        }
        DbPool::Sqlite(pool) => {
            codeyard_db_core::queries::sqlite::fine_grained_tokens::selected(pool, token, repo)
                .await?
        }
    };
    if native {
        return Ok(true);
    }
    let cached = codeyard_db_core::cells::authorization::lookup(
        pool,
        &codeyard_db_core::cells::authorization::Query {
            kind: "token",
            id: Some(token),
            hash: None,
            write: false,
        },
    )
    .await?;
    Ok(
        cached.is_some_and(|credential| match credential.answer.authority {
            codeyard_db_core::cells::authorization::Authority::FineGrained {
                repositories: Some(ids),
                ..
            } => ids.contains(&repo),
            _ => false,
        }),
    )
}
native_operation!(codeyard_db_core::queries::postgres::fine_grained_tokens::policy, codeyard_db_core::queries::sqlite::fine_grained_tokens::policy,
    /// The organization's policy, or the defaults when it has none.
    fn policy(pool: &DbPool, org: i64) -> Result<Policy>);
native_operation!(crate::postgres::fine_grained_tokens::set_policy, crate::sqlite::fine_grained_tokens::set_policy,
    /// Saves the policy while `actor` owns the organization; false when they do not.
    fn set_policy(pool: &DbPool, actor: impl Into<codeyard_db_core::queries::credentials::Actor>, org: i64, policy: &Policy) -> Result<bool>);
/// Which of an organization's tokens a listing shows: those in `status` whose users are still
/// members and that pass every filter given, newest first unless `ascending`.
#[derive(Default)]
pub struct TokenFilter<'a> {
    pub status: &'a str,
    /// The usernames of the tokens' users; any user when empty.
    pub owners: &'a [String],
    /// Any token when empty.
    pub token_ids: &'a [i64],
    /// A repository the token must reach.
    pub repository: Option<i64>,
    /// A catalog permission the token must hold, at this level when one is given.
    pub permission: Option<(&'a str, Option<&'a str>)>,
    pub last_used_before: Option<DateTime<Utc>>,
    pub last_used_after: Option<DateTime<Utc>>,
    pub ascending: bool,
    /// Only tokens with a lower id: the cursor of a newest-first page.
    pub before: Option<i64>,
}

native_operation!(crate::postgres::fine_grained_tokens::organization_tokens, crate::sqlite::fine_grained_tokens::organization_tokens,
    /// A page of the organization's tokens that `filter` shows.
    fn organization_tokens(pool: &DbPool, org: i64, filter: &TokenFilter<'_>, offset: i64, limit: i64) -> Result<Vec<Token>>);

/// One page of the owners' token tab, read in one snapshot.
pub struct OwnerTokens {
    pub policy: Policy,
    pub pending: Vec<Token>,
    pub approved: Vec<Token>,
    /// The listed tokens' selected repositories as (token id, name), at most 100 per token.
    pub repositories: Vec<(i64, String)>,
}

native_operation!(crate::postgres::fine_grained_tokens::owner_tokens, crate::sqlite::fine_grained_tokens::owner_tokens,
    /// None unless `actor` is an active owner of `org` through the live `session`. Each list
    /// holds at most `limit` tokens, newest first, below its cursor in `before` (pending,
    /// approved); a cursor of 0 starts at the newest.
    fn owner_tokens(pool: &DbPool, actor: i64, session: &str, org: i64, before: (i64, i64), limit: i64) -> Result<Option<OwnerTokens>>);
native_operation!(crate::postgres::fine_grained_tokens::review, crate::sqlite::fine_grained_tokens::review,
    /// Moves the organization's tokens in `from` to `status` while `reviewer` owns the
    /// organization, and returns how many changed.
    fn review(pool: &DbPool, org: i64, reviewer: impl Into<codeyard_db_core::queries::credentials::Actor>, tokens: &[i64], from: &str, status: &str, reason: Option<&str>) -> Result<u64>);
native_operation!(crate::postgres::fine_grained_tokens::review_all, crate::sqlite::fine_grained_tokens::review_all,
    /// `review` for every token in `from` whose user is still a member, in one statement.
    fn review_all(pool: &DbPool, org: i64, reviewer: impl Into<codeyard_db_core::queries::credentials::Actor>, from: &str, status: &str, reason: Option<&str>) -> Result<u64>);
native_operation!(crate::postgres::fine_grained_tokens::classic_refused, crate::sqlite::fine_grained_tokens::classic_refused,
    /// Whether `token` is a classic personal token and `org` refuses classic tokens. OAuth,
    /// App, job and fine-grained tokens are never classic.
    fn classic_refused(pool: &DbPool, token: i64, org: i64) -> Result<bool>);
native_operation!(crate::postgres::fine_grained_tokens::classic_refused_repository, crate::sqlite::fine_grained_tokens::classic_refused_repository,
    /// `classic_refused` for the organization that owns `repo`, if any.
    fn classic_refused_repository(pool: &DbPool, token: i64, repo: i64) -> Result<bool>);
