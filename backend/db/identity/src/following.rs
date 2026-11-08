use anyhow::Result;
use codeyard_database::models::User;
use codeyard_database::native_operation;
use codeyard_database::DbPool;

pub(crate) fn connections_sql(followers: bool) -> &'static str {
    if followers {
        "SELECT u.* FROM follows f JOIN users u ON u.id=f.follower_id WHERE f.following_id=$1 ORDER BY f.follower_id LIMIT $2 OFFSET $3"
    } else {
        "SELECT u.* FROM follows f JOIN users u ON u.id=f.following_id WHERE f.follower_id=$1 ORDER BY f.following_id LIMIT $2 OFFSET $3"
    }
}

native_operation!(crate::postgres::following::list_connections,
    crate::sqlite::following::list_connections,
    fn list_connections(pool: &DbPool, user_id: i64, followers: bool, limit: i64, offset: i64) -> Result<Vec<User>>);

#[derive(Debug, thiserror::Error)]
#[error("An unrestricted account is required")]
pub struct FollowRestricted;

native_operation!(crate::postgres::following::follow_user,
    crate::sqlite::following::follow_user,
    fn follow_user(pool: &DbPool, follower_id: i64, following_id: i64, credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>) -> Result<bool>);

native_operation!(crate::postgres::following::unfollow_user,
    crate::sqlite::following::unfollow_user,
    fn unfollow_user(pool: &DbPool, follower_id: i64, following_id: i64, credential: &codeyard_database::queries::users::accounts::AccountCredential<'_>) -> Result<bool>);

native_operation!(crate::postgres::following::is_user_following,
    crate::sqlite::following::is_user_following,
    fn is_user_following(pool: &DbPool, follower_id: i64, following_id: i64) -> Result<bool>);

native_operation!(crate::postgres::following::count_user_followers,
    crate::sqlite::following::count_user_followers,
    fn count_user_followers(pool: &DbPool, user_id: i64) -> Result<i64>);

native_operation!(crate::postgres::following::count_user_following,
    crate::sqlite::following::count_user_following,
    fn count_user_following(pool: &DbPool, user_id: i64) -> Result<i64>);
