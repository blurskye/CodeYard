//! Shared named operation surface; native transactions stay inside db.
//! The mail server settings, the outbox of mail to send, and each account's email preference.
use crate::backend::db::models::{EmailSettings, OutboxMessage};
use crate::backend::db::DbPool;
use anyhow::Result;

/// Settings from the admin form. `password` is the encrypted password and its nonce; `None`
/// keeps the one already saved.
/// A new link to the same address replaces the last one only after this long, so repeated
/// requests cannot flood a mailbox.
pub(in crate::backend::db) const RESEND_SECONDS: i64 = 60;

pub struct NewEmailSettings<'a> {
    pub host: &'a str,
    pub port: i32,
    pub security: &'a str,
    pub username: &'a str,
    pub password: Option<(&'a [u8], &'a [u8])>,
    pub from_name: &'a str,
    pub from_address: &'a str,
    pub reply_to: &'a str,
}

native_operation!(crate::backend::db::postgres::email::get_email_settings,
    crate::backend::db::sqlite::email::get_email_settings,
    fn get_email_settings(pool: &DbPool) -> Result<Option<EmailSettings>>);

native_operation!(crate::backend::db::postgres::email::save_email_settings,
    crate::backend::db::sqlite::email::save_email_settings,
    /// Returns false when the actor is no longer a signed-in administrator.
    fn save_email_settings(
    pool: &DbPool,
    actor: i64,
    session: &str,
    settings: &NewEmailSettings<'_>,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::email::enqueue_email,
    crate::backend::db::sqlite::email::enqueue_email,
    fn enqueue_email(pool: &DbPool, to_address: &str, subject: &str, body_text: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::email::claim_due_email,
    crate::backend::db::sqlite::email::claim_due_email,
    /// Takes the oldest due message that has attempts left and holds it for `lease_seconds`. The
    /// attempt is counted now. Nodes that run this at once get different messages.
    fn claim_due_email(pool: &DbPool, max_attempts: i32, lease_seconds: i64) -> Result<Option<OutboxMessage>>);

native_operation!(crate::backend::db::postgres::email::mark_email_sent,
    crate::backend::db::sqlite::email::mark_email_sent,
    /// Marks the message sent and empties its body, which may hold a link that still works.
    fn mark_email_sent(pool: &DbPool, id: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::email::mark_email_failed,
    crate::backend::db::sqlite::email::mark_email_failed,
    /// Records a failed attempt. A message that has used `max_attempts` is given up, and its
    /// body is emptied like a sent one.
    fn mark_email_failed(pool: &DbPool, id: i64, error: &str, retry_seconds: i64, max_attempts: i32) -> Result<()>);

native_operation!(crate::backend::db::postgres::email::remove_old_email,
    crate::backend::db::sqlite::email::remove_old_email,
    /// Deletes sent messages, and failed ones that used all attempts, older than `days`.
    fn remove_old_email(pool: &DbPool, max_attempts: i32, days: i64) -> Result<u64>);

native_operation!(crate::backend::db::postgres::email::get_email_notifications,
    crate::backend::db::sqlite::email::get_email_notifications,
    fn get_email_notifications(pool: &DbPool, user_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::email::set_email_notifications,
    crate::backend::db::sqlite::email::set_email_notifications,
    fn set_email_notifications(pool: &DbPool, user_id: i64, enabled: bool) -> Result<()>);

native_operation!(crate::backend::db::postgres::email::notification_email_address,
    crate::backend::db::sqlite::email::notification_email_address,
    /// The address to mail a user's notifications to: set, verified, and wanted by the user.
    fn notification_email_address(pool: &DbPool, user_id: i64) -> Result<Option<String>>);

native_operation!(crate::backend::db::postgres::email::save_email_token,
    crate::backend::db::sqlite::email::save_email_token,
    /// Stores the hash of a link mailed to the user, for `purpose` 'verify' or 'reset'. It
    /// replaces the user's earlier link of that kind, unless that one went to the same address
    /// less than `RESEND_SECONDS` ago. False when the earlier link stays and nothing is to be sent.
    fn save_email_token(pool: &DbPool, user_id: i64, purpose: &str, email: &str, token_hash: &str, ttl_seconds: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::email::email_token_is_valid,
    crate::backend::db::sqlite::email::email_token_is_valid,
    /// Whether the link exists, has not expired, and was sent to the address the user has now.
    fn email_token_is_valid(pool: &DbPool, purpose: &str, token_hash: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::email::take_email_token,
    crate::backend::db::sqlite::email::take_email_token,
    /// Uses up a valid link and returns the user and the address it was sent to. A link works once.
    fn take_email_token(pool: &DbPool, purpose: &str, token_hash: &str) -> Result<Option<(i64, String)>>);

native_operation!(crate::backend::db::postgres::email::confirm_email,
    crate::backend::db::sqlite::email::confirm_email,
    /// Marks the user's address verified, if it is still `email`.
    fn confirm_email(pool: &DbPool, user_id: i64, email: &str) -> Result<bool>);
