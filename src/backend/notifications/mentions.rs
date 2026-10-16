//! Mention notifications. A new issue, pull request, comment or review text, and an edited
//! issue or pull request body, notifies each user it newly @mentions. The names are the ones the
//! Markdown renderer links, so a mention in code or in a link notifies nobody.
use super::create::{create_and_broadcast_notification, NotificationRequest};
use super::NotificationBroadcaster;
use crate::backend::db;
use crate::backend::repositories::access::can_read;
use crate::webui::components::references::mentioned_usernames;
use serde_json::json;
use std::collections::HashSet;

/// Most users one event notifies. Restricted accounts and users who cannot read the repository
/// do not count toward it.
const MAX_NOTIFIED: usize = 20;
/// Most new names one event looks up, in the order they first appear. Names past this are not
/// notified. The cap bounds the access checks that one write can cause.
const MAX_LOOKUP: usize = 100;

/// Whether `text` mentions anyone. A caller uses it to skip work that only a mention needs.
pub fn mentions_anyone(text: &str) -> bool {
    !mentioned_usernames(text).is_empty()
}

/// One text that may mention users, and the event it belongs to.
pub struct MentionSource<'a> {
    pub actor: &'a db::models::User,
    pub repository: &'a db::models::Repository,
    pub owner: &'a str,
    pub repo: &'a str,
    /// The issue or pull request number.
    pub number: i32,
    /// Where the notification leads: the item, or the comment on it.
    pub link: &'a str,
    /// The text before an edit. `None` for new text, so every name counts.
    pub old_text: Option<&'a str>,
    pub text: &'a str,
    /// Users this event already notifies for another reason. Each one is notified once.
    pub notified: &'a [i64],
}

/// Notifies the users that `source.text` newly mentions. The write has already been saved, so a
/// failure here never fails the request. A failed lookup is logged; a failed notification is
/// dropped, as the other notification kinds drop theirs.
pub async fn notify_mentions(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    source: MentionSource<'_>,
) {
    // Names the old text mentions are not new, however many names either text holds. They are
    // subtracted before the cap, so an edit that keeps a long list still notifies its new name.
    let before: HashSet<String> = source
        .old_text
        .map(mentioned_usernames)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let names: Vec<String> = mentioned_usernames(source.text)
        .into_iter()
        .filter(|name| *name != source.actor.username && !before.contains(name))
        .take(MAX_LOOKUP)
        .collect();
    if names.is_empty() {
        return;
    }
    let users = match db::users::accounts::get_users_by_usernames(pool, &names).await {
        Ok(users) => users,
        Err(error) => {
            tracing::warn!(%error, "Mention notifications were not sent: user lookup failed");
            return;
        }
    };
    let title = format!(
        "{} mentioned you in {}/{}#{}",
        source.actor.username, source.owner, source.repo, source.number
    );
    let mut sent = 0;
    for name in &names {
        if sent == MAX_NOTIFIED {
            break;
        }
        let Some(user) = users.iter().find(|user| user.username == *name) else {
            continue;
        };
        // Restricted accounts get nothing, and they are skipped before they use up the budget.
        if user.is_banned || user.must_change_password || source.notified.contains(&user.id) {
            continue;
        }
        // A user who cannot read the repository gets nothing, so the mention reveals nothing.
        match can_read(pool, Some(user), source.repository).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!(%error, "Mention notification skipped: repository access check failed");
                continue;
            }
        }
        let _ = create_and_broadcast_notification(
            pool,
            broadcaster,
            NotificationRequest {
                user_id: user.id,
                actor_id: Some(source.actor.id),
                repo_id: Some(source.repository.id),
                kind: "mention",
                title: &title,
                body: Some(source.text),
                link: Some(source.link),
                metadata: json!({
                    "repo_owner": source.owner,
                    "repo_name": source.repo,
                    "number": source.number,
                }),
            },
        )
        .await;
        sent += 1;
    }
}
