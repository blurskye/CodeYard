use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Debug, thiserror::Error)]
#[error("An unrestricted account is required")]
pub struct FollowRestricted;

native_operation!(crate::backend::db::postgres::following::follow_user,
    crate::backend::db::sqlite::following::follow_user,
    fn follow_user(pool: &DbPool, follower_id: i64, following_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::following::unfollow_user,
    crate::backend::db::sqlite::following::unfollow_user,
    fn unfollow_user(pool: &DbPool, follower_id: i64, following_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::following::is_user_following,
    crate::backend::db::sqlite::following::is_user_following,
    fn is_user_following(pool: &DbPool, follower_id: i64, following_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::following::count_user_followers,
    crate::backend::db::sqlite::following::count_user_followers,
    fn count_user_followers(pool: &DbPool, user_id: i64) -> Result<i64>);

native_operation!(crate::backend::db::postgres::following::count_user_following,
    crate::backend::db::sqlite::following::count_user_following,
    fn count_user_following(pool: &DbPool, user_id: i64) -> Result<i64>);
