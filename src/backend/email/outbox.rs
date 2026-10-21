//! Mail waits in the `email_outbox` table until a node sends it. Several nodes may run the
//! sender at once: each claims its own rows. A failed send is tried again later, a few times.
use super::{config, messages::Mail, send};
use crate::backend::crypto::SecretEncryptor;
use crate::backend::db::{self, DbPool};
use std::time::Duration;
use tracing::{info, warn};

/// Attempts per message, the first one included.
pub const MAX_ATTEMPTS: i32 = 5;
/// How long a claimed message is held. A sender that dies mid-send delays the retry by this much.
const CLAIM_SECONDS: i64 = 600;
/// Sent and given-up messages are kept this long, then deleted. Their bodies are emptied at once.
const KEEP_DAYS: i64 = 14;
/// The most messages one pass sends, so a large backlog cannot starve the stop signal.
const PER_PASS: usize = 50;
const PASS_INTERVAL: Duration = Duration::from_secs(10);
/// One pass in this many also deletes old messages: about once an hour.
const PRUNE_EVERY: u32 = 360;

/// Seconds to wait after attempt number `attempts` failed: 1, 4, 16 and 64 minutes.
pub fn retry_delay(attempts: i32) -> i64 {
    60 * 4_i64.pow(u32::try_from(attempts - 1).unwrap_or(0).min(6))
}

pub async fn enqueue(pool: &DbPool, to: &str, mail: Mail) {
    if let Err(error) = db::email::enqueue_email(pool, to, &mail.subject, &mail.body).await {
        warn!(%error, "could not queue an email");
    }
}

/// Sends the messages that are due, one at a time. Returns how many were sent.
pub async fn send_due(pool: &DbPool, secrets: Option<&SecretEncryptor>) -> usize {
    let config = match config::load(pool, secrets).await {
        Ok(Some((config, _))) => config,
        Ok(None) => return 0,
        Err(message) => {
            warn!(%message, "email is not sent");
            return 0;
        }
    };
    let mut sent = 0;
    for _ in 0..PER_PASS {
        let message = match db::email::claim_due_email(pool, MAX_ATTEMPTS, CLAIM_SECONDS).await {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                warn!(%error, "could not read the email outbox");
                break;
            }
        };
        let outcome = match send::send(
            &config,
            &message.to_address,
            &message.subject,
            &message.body_text,
        )
        .await
        {
            Ok(()) => {
                sent += 1;
                db::email::mark_email_sent(pool, message.id).await
            }
            Err(error) => {
                warn!(%error, id = message.id, attempts = message.attempts, "email send failed");
                db::email::mark_email_failed(
                    pool,
                    message.id,
                    &error,
                    retry_delay(message.attempts),
                    MAX_ATTEMPTS,
                )
                .await
            }
        };
        if let Err(error) = outcome {
            warn!(%error, id = message.id, "could not record the email result");
        }
    }
    sent
}

/// Runs on every web node until `stop` changes.
pub async fn run(
    pool: DbPool,
    secrets: Option<std::sync::Arc<SecretEncryptor>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut pass: u32 = 0;
    loop {
        let sent = send_due(&pool, secrets.as_deref()).await;
        if sent > 0 {
            info!(sent, "sent queued email");
        }
        if pass.is_multiple_of(PRUNE_EVERY) {
            match db::email::remove_old_email(&pool, MAX_ATTEMPTS, KEEP_DAYS).await {
                Ok(0) => {}
                Ok(removed) => info!(removed, "removed old queued email"),
                Err(error) => warn!(%error, "could not remove old queued email"),
            }
        }
        pass = pass.wrapping_add(1);
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(PASS_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_grows_with_each_failure() {
        let waits: Vec<i64> = (1..MAX_ATTEMPTS).map(retry_delay).collect();
        assert_eq!(waits, [60, 240, 960, 3840]);
        assert_eq!(retry_delay(0), 60);
        assert!(retry_delay(1000) <= 60 * 4096);
    }
}
