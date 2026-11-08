//! Mention notifications. A new issue, pull request, comment or review text, and an edited
//! issue or pull request body, notifies each user it newly @mentions. The names are the ones the
//! Markdown renderer links, so a mention in code or in a link notifies nobody.
use crate::broadcast::NotificationBroadcaster;
use crate::create::create_and_broadcast_notification;
use crate::create::NotificationRequest;

use codeyard_identity::repository_access::can_read;
use codeyard_webui::components::references::mentioned_usernames;
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

#[derive(Clone, Copy)]
pub enum MentionComment {
    Issue(i64),
    PullRequest(i64),
    Commit(i64),
}

/// One text that may mention users, and the event it belongs to.
pub struct MentionSource<'a> {
    pub actor: &'a codeyard_database::models::User,
    pub repository: &'a codeyard_database::models::Repository,
    pub owner: &'a str,
    pub repo: &'a str,
    /// The issue or pull request number.
    pub number: i32,
    /// The title of the issue or pull request, which the notification is headed by.
    pub subject: &'a str,
    /// Where the notification leads: the item, or the comment on it.
    pub link: &'a str,
    /// The comment the text belongs to, for deletion of its mention notices.
    pub comment_id: Option<MentionComment>,
    /// The text before an edit. `None` for new text, so every name counts.
    pub old_text: Option<&'a str>,
    pub text: &'a str,
    /// Users this event already notifies for another reason. Each one is notified once.
    pub notified: &'a [i64],
}

/// Notifies the users that `source.text` newly mentions, and returns the ones it notified. The
/// write has already been saved, so a failure here never fails the request. A failed lookup is
/// logged; a failed notification is dropped, as the other notification kinds drop theirs.
pub async fn notify_mentions(
    pool: &codeyard_database::DbPool,
    broadcaster: &NotificationBroadcaster,
    source: MentionSource<'_>,
) -> Vec<i64> {
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
        return Vec::new();
    }
    let mut users =
        match codeyard_db_identity::users::accounts::get_users_by_usernames(pool, &names).await {
            Ok(users) => users,
            Err(error) => {
                tracing::warn!(%error, "Mention notifications were not sent: user lookup failed");
                return Vec::new();
            }
        };
    users.extend(remote_users(pool, source.repository, &names, &users).await);
    let title = match source.comment_id {
        Some(MentionComment::Commit(_)) => format!(
            "{} mentioned you in {}/{}@{}",
            source.actor.username, source.owner, source.repo, source.subject
        ),
        _ => format!(
            "{} mentioned you in {}/{}#{}",
            source.actor.username, source.owner, source.repo, source.number
        ),
    };
    let mut sent = Vec::new();
    for name in &names {
        if sent.len() == MAX_NOTIFIED {
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
        if user.is_stub {
            // An account homed in another cell gets the mention in its home inbox. Commit
            // mentions have no number there, so they stay local.
            if matches!(source.comment_id, Some(MentionComment::Commit(_))) {
                continue;
            }
            match codeyard_identity::cells::queue_notice(
                pool,
                user.id,
                source.repository,
                source.actor,
                source.number,
                source.subject,
                &title,
                source.text,
                source.link,
            )
            .await
            {
                Ok(true) => sent.push(user.id),
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "Cross-cell mention notice was not queued"),
            }
            continue;
        }
        let mut metadata = json!({
            "repo_owner": source.owner, "repo_name": source.repo, "number": source.number, "subject": source.subject,
            "comment_id": match source.comment_id { Some(MentionComment::Issue(id)) => Some(id), _ => None },
            "pull_request_comment_id": match source.comment_id { Some(MentionComment::PullRequest(id)) => Some(id), _ => None },
        });
        if let Some(MentionComment::Commit(id)) = source.comment_id {
            metadata["number"] = serde_json::Value::Null;
            metadata["commit_comment_id"] = json!(id);
            metadata["commit_sha"] = json!(source.subject);
        }
        let created = create_and_broadcast_notification(
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
                metadata,
            },
        )
        .await;
        // Only a notification that was created counts toward the budget.
        if created.is_ok() {
            sent.push(user.id);
        }
    }
    sent
}

/// Most accounts one event looks up in other cells, and the time all lookups may take.
const MAX_REMOTE_LOOKUP: usize = MAX_NOTIFIED;
const REMOTE_LOOKUP_SECONDS: u64 = 10;

/// Accounts this cell does not know yet, looked up in their home cells. Only a public
/// repository's mention reaches another cell, so no other lookup is made.
async fn remote_users(
    pool: &codeyard_database::DbPool,
    repository: &codeyard_database::models::Repository,
    names: &[String],
    known: &[codeyard_database::models::User],
) -> Vec<codeyard_database::models::User> {
    if pool.foreign_authorization().is_none()
        || repository.is_private
        || repository.is_internal
        || repository.is_gist
        || repository.is_stub
    {
        return Vec::new();
    }
    let mut found = Vec::new();
    let lookups = async {
        for name in names
            .iter()
            .filter(|name| !known.iter().any(|user| user.username == **name))
            .take(MAX_REMOTE_LOOKUP)
        {
            match codeyard_identity::cells::resolve_user(pool, name).await {
                Ok(Some(user)) => found.push(user),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "Mentioned account lookup failed"),
            }
        }
    };
    if tokio::time::timeout(
        std::time::Duration::from_secs(REMOTE_LOOKUP_SECONDS),
        lookups,
    )
    .await
    .is_err()
    {
        tracing::warn!("Mentioned account lookups timed out");
    }
    found
}

/// Stores a mention from another cell's public repository in this home inbox, once per event.
/// A recipient who is gone, blocked the actor or ignores the repository gets nothing, and the
/// event still counts as delivered.
pub async fn receive(
    pool: &codeyard_database::DbPool,
    broadcaster: &NotificationBroadcaster,
    event: &codeyard_database::cells::notices::Event,
    admission: &dyn codeyard_database::cells::notices::Admission,
) -> anyhow::Result<()> {
    let source = admission.check(event).await?;
    event.validate(source)?;
    let notice = &event.notice;
    if event.withdrawn {
        return codeyard_database::cells::notices::withdraw(pool, source, notice.repository.id)
            .await;
    }
    let Some(recipient) =
        codeyard_db_identity::users::accounts::get_user_by_id(pool, notice.user_id).await?
    else {
        return Ok(());
    };
    anyhow::ensure!(
        !recipient.is_stub && recipient.home_cell == i32::from(pool.cell_id().get()),
        "Notice recipient has another home"
    );
    let cell = i32::from(source.get());
    if codeyard_db_repos::notifications::records::received(pool, recipient.id, cell, event.id)
        .await?
    {
        return Ok(());
    }
    let actor = codeyard_database::cells::notices::retain(pool, source, notice).await?;
    anyhow::ensure!(
        admission.check(event).await? == source,
        "Notice source changed"
    );
    let request = NotificationRequest {
        user_id: recipient.id,
        actor_id: actor,
        repo_id: Some(notice.repository.id),
        kind: "mention",
        title: &notice.title,
        body: Some(&notice.body),
        link: Some(&notice.link),
        metadata: json!({
            "repo_owner": notice.owner.name(), "repo_name": notice.repository.name,
            "number": notice.number, "subject": notice.subject,
            "source_cell": cell, "source_event": event.id,
        }),
    };
    match crate::create::store(pool, broadcaster, request).await {
        Ok(_) | Err(crate::create::CreateError::Suppressed) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
