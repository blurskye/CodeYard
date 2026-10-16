//! Shared named operation surface; native transactions stay inside db.
use crate::backend::db::models::{SignupRequest, User};
use crate::backend::db::DbPool;
use anyhow::Result;

#[derive(Debug)]
pub enum ReviewError {
    Denied,
    Missing,
    Processed,
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Denied => "Current administrator authority required",
            Self::Missing => "Signup request not found",
            Self::Processed => "Signup request already processed",
        })
    }
}

impl std::error::Error for ReviewError {}

native_operation!(crate::backend::db::postgres::users::signup_requests::create_signup_request,
    crate::backend::db::sqlite::users::signup_requests::create_signup_request,
    fn create_signup_request(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
) -> Result<SignupRequest>);

native_operation!(crate::backend::db::postgres::users::signup_requests::list_signup_requests,
    crate::backend::db::sqlite::users::signup_requests::list_signup_requests,
    fn list_signup_requests(pool: &DbPool, status: &str) -> Result<Vec<SignupRequest>>);

native_operation!(crate::backend::db::postgres::users::signup_requests::get_pending_signup_request_by_identity,
    crate::backend::db::sqlite::users::signup_requests::get_pending_signup_request_by_identity,
    fn get_pending_signup_request_by_identity(
    pool: &DbPool,
    username: &str,
    email: Option<&str>,
) -> Result<Option<SignupRequest>>);

native_operation!(crate::backend::db::postgres::users::signup_requests::approve_signup_request,
    crate::backend::db::sqlite::users::signup_requests::approve_signup_request,
    fn approve_signup_request(
    pool: &DbPool,
    request_id: i64,
    reviewer_user_id: i64,
) -> Result<User>);

native_operation!(crate::backend::db::postgres::users::signup_requests::reject_signup_request,
    crate::backend::db::sqlite::users::signup_requests::reject_signup_request,
    fn reject_signup_request(
    pool: &DbPool,
    request_id: i64,
    reviewer_user_id: i64,
) -> Result<()>);
