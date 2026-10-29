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

pub(in crate::backend::db) fn primary_link_condition(pg: bool) -> String {
    let now = if pg { "clock_timestamp()" } else { "strftime('%Y-%m-%dT%H:%M:%f000Z','now')" };
    format!("u.email=t.email AND NOT u.is_banned AND t.expires_at>{now}")
}

pub(in crate::backend::db) fn secondary_link_condition(pg: bool) -> String {
    let now = if pg { "clock_timestamp()" } else { "strftime('%Y-%m-%dT%H:%M:%f000Z','now')" };
    format!("NOT u.is_banned AND NOT e.verified AND e.verification_expires_at>{now}")
}

pub(in crate::backend::db) fn delivery_allowed_sql(pg: bool) -> String {
    let id = if pg { "$1" } else { "?1" };
    let primary = primary_link_condition(pg);
    let secondary = secondary_link_condition(pg);
    format!("SELECT EXISTS(SELECT 1 FROM email_outbox o WHERE o.id={id} AND o.sent_at IS NULL AND (o.link_user_id IS NULL OR EXISTS(SELECT 1 FROM users u WHERE u.id=o.link_user_id AND (EXISTS(SELECT 1 FROM email_tokens t WHERE t.user_id=u.id AND t.purpose=o.link_purpose AND t.token_hash=o.link_token_hash AND t.email=o.to_address AND {primary}) OR (o.link_purpose='secondary' AND EXISTS(SELECT 1 FROM user_emails e WHERE e.user_id=u.id AND lower(e.email)=lower(o.to_address) AND e.verification_hash=o.link_token_hash AND {secondary}))))))")
}

/// A link mail: the hash of its token and the message that carries the token. Both are saved
/// in one transaction.
pub struct EmailLink<'a> {
    pub user_id: i64,
    pub purpose: &'a str,
    pub email: &'a str,
    pub token_hash: &'a str,
    pub ttl_seconds: i64,
    pub subject: &'a str,
    pub body_text: &'a str,
}

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

native_operation!(crate::backend::db::postgres::email::enqueue_notification_email,
    crate::backend::db::sqlite::email::enqueue_notification_email,
    /// Queues the email for a notification. It goes out only while the recipient can still read
    /// the notification.
    fn enqueue_notification_email(pool: &DbPool, notification: i64, to_address: &str, subject: &str, body_text: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::email::withdraw_email,
    crate::backend::db::sqlite::email::withdraw_email,
    /// Drops an unsent message that must no longer be sent.
    fn withdraw_email(pool: &DbPool, id: i64) -> Result<()>);

native_operation!(crate::backend::db::postgres::email::enqueue_invitation_email,
    crate::backend::db::sqlite::email::enqueue_invitation_email,
    fn enqueue_invitation_email(pool: &DbPool, invitation: i64, to_address: &str, subject: &str, body_text: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::email::claim_due_email,
    crate::backend::db::sqlite::email::claim_due_email,
    /// Takes the oldest due message that has attempts left and holds it for `lease_seconds`.
    /// `attempts` is the number of this attempt; only a recorded failure counts it, so a send
    /// that dies mid-way is tried again once the hold ends. Nodes that run this at once get
    /// different messages.
    fn claim_due_email(pool: &DbPool, max_attempts: i32, lease_seconds: i64) -> Result<Option<OutboxMessage>>);

native_operation!(crate::backend::db::postgres::email::delivery_allowed,
    crate::backend::db::sqlite::email::delivery_allowed,
    /// Rechecks a claimed message before SMTP. Link mail must still match its live issuance;
    /// notification readability remains owned by notifications.
    fn delivery_allowed(pool: &DbPool, id: i64) -> Result<bool>);

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
    /// Stores the hash of a link mailed to the user, for `purpose` 'verify' or 'reset', and queues
    /// the mail in the same transaction. It replaces the user's earlier link of that kind, unless
    /// that one went to the same address less than `RESEND_SECONDS` ago. Superseded unsent mail
    /// is withdrawn atomically. False when nothing is queued.
    fn save_email_token(pool: &DbPool, link: &EmailLink<'_>) -> Result<bool>);

native_operation!(crate::backend::db::postgres::email::email_token_is_valid,
    crate::backend::db::sqlite::email::email_token_is_valid,
    /// Whether the link exists, has not expired, and was sent to the address the user has now.
    fn email_token_is_valid(pool: &DbPool, purpose: &str, token_hash: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::email::verify_email_token,
    crate::backend::db::sqlite::email::verify_email_token,
    /// Uses up a valid primary-address link and marks its address verified, in one transaction.
    /// None when no such link has this hash. Some(false) when the user changed the address since.
    fn verify_email_token(pool: &DbPool, token_hash: &str) -> Result<Option<bool>>);
