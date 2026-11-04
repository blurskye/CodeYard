//! Shared named operation surface; native transactions stay inside db.
use anyhow::Result;
use codeyard_db_core::models::SignupRequest;
use codeyard_db_core::models::User;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

#[derive(Debug)]
pub enum ReviewError {
    Denied,
    Missing,
    Processed,
    Conflict,
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Denied => "Current administrator authority required",
            Self::Missing => "Signup request not found",
            Self::Processed => "Signup request already processed",
            Self::Conflict => "Account identity already in use",
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

/// Review metadata only; password verifiers never belong in a listing.
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct SignupSummary {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub struct AdminSignupPage {
    pub requests: Vec<SignupSummary>,
    pub total: i64,
    pub page: usize,
}

native_operation!(crate::backend::db::postgres::users::signup_requests::admin_pending_page,
    crate::backend::db::sqlite::users::signup_requests::admin_pending_page,
    fn admin_pending_page(pool: &DbPool, administrator: &crate::backend::db::users::transitions::AdministratorActor, limit: i64, page: usize) -> Result<AdminSignupPage>);

native_operation!(crate::backend::db::postgres::users::signup_requests::count_signup_requests,
    crate::backend::db::sqlite::users::signup_requests::count_signup_requests,
    fn count_signup_requests(pool: &DbPool, status: &str) -> Result<i64>);

native_operation!(crate::backend::db::postgres::users::signup_requests::list_signup_requests,
    crate::backend::db::sqlite::users::signup_requests::list_signup_requests,
    fn list_signup_requests(pool: &DbPool, status: &str, limit: i64, offset: i64, after: i64) -> Result<Vec<SignupSummary>>);

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
    reviewer_user_id: impl Into<super::transitions::AdministratorActor>,
) -> Result<User>);

native_operation!(crate::backend::db::postgres::users::signup_requests::reject_signup_request,
    crate::backend::db::sqlite::users::signup_requests::reject_signup_request,
    /// Returns the username and the email address (if any) the rejected request gave.
    fn reject_signup_request(
    pool: &DbPool,
    request_id: i64,
    reviewer_user_id: impl Into<super::transitions::AdministratorActor>,
) -> Result<(String, Option<String>)>);
