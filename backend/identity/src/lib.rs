//! Account policy and atomic identity operations, independent of ingress transport.
//! HTTP owns decoding, cookies, public DTOs, statuses and redirects.

pub mod accounts;
pub mod activity;

pub mod blocks;
pub mod credentials;
pub mod emails;
pub mod fine_grained;

pub mod github_apps;
pub mod gpg;

pub mod keys;
pub mod login;
pub mod oauth;
pub mod oauth_apps;
pub mod passwords;
pub mod profiles;
pub mod registration;
pub mod sessions;
pub mod setup;
pub mod signatures;
pub mod tokens;
pub mod two_factor;

use codeyard_db_core::models::User;

/// Credentials were revalidated by the session transaction, not merely by a KDF.
/// This record stays server-side; adapters must project a safe account response.
pub struct AuthenticatedAccount {
    pub user: User,
    pub session_id: String,
}
pub mod email_links;

pub mod repository_access;

pub mod client_address;
pub mod database_context;
pub mod github_auth;

pub mod cells;
