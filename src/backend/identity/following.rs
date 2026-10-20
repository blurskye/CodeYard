//! Follow edges and their best-effort notices, shared by JSON and native forms.

use crate::backend::{
    db::{following, users::accounts, DbPool},
    notifications::{
        create::{create_and_broadcast_notification, NotificationRequest},
        NotificationBroadcaster,
    },
};
use serde_json::json;

pub struct FollowStatus {
    pub following: bool,
    pub followers_count: i64,
    pub following_count: i64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FollowError {
    #[error("User not found")]
    Missing,
    #[error("Cannot follow yourself")]
    SelfFollow,
    #[error("Cannot unfollow yourself")]
    SelfUnfollow,
    #[error("Follow storage is temporarily unavailable")]
    Unavailable,
    #[error("Follow request completed, but updated counts are temporarily unavailable")]
    CountsUnavailableAfterSave,
}

pub async fn status(
    pool: &DbPool,
    target: i64,
    viewer: Option<i64>,
) -> Result<FollowStatus, FollowError> {
    let following = match viewer.filter(|viewer| *viewer != target) {
        Some(viewer) => following::is_user_following(pool, viewer, target)
            .await
            .map_err(|_| FollowError::Unavailable)?,
        None => false,
    };
    Ok(FollowStatus {
        following,
        followers_count: following::count_user_followers(pool, target)
            .await
            .map_err(|_| FollowError::Unavailable)?,
        following_count: following::count_user_following(pool, target)
            .await
            .map_err(|_| FollowError::Unavailable)?,
    })
}

pub async fn change(
    pool: &DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: i64,
    username: &str,
    follow: bool,
) -> Result<FollowStatus, FollowError> {
    let current = accounts::get_user_by_id(pool, actor)
        .await
        .map_err(|_| FollowError::Unavailable)?
        .ok_or(FollowError::Missing)?;
    let target = accounts::get_user_by_username(pool, username)
        .await
        .map_err(|_| FollowError::Unavailable)?
        .ok_or(FollowError::Missing)?;
    if actor == target.id {
        return Err(if follow {
            FollowError::SelfFollow
        } else {
            FollowError::SelfUnfollow
        });
    }
    if follow {
        let created = following::follow_user(pool, actor, target.id)
            .await
            .map_err(|_| FollowError::Unavailable)?;
        if created {
            let title = format!("{} started following you", current.username);
            let link = format!("/{}", current.username);
            if let Err(error) = create_and_broadcast_notification(
                pool,
                broadcaster,
                NotificationRequest {
                    user_id: target.id,
                    actor_id: Some(actor),
                    repo_id: None,
                    kind: "user_followed",
                    title: &title,
                    body: None,
                    link: Some(&link),
                    metadata: json!({ "follower_username": current.username }),
                },
            )
            .await
            {
                if matches!(
                    error,
                    crate::backend::notifications::create::CreateError::Suppressed
                ) {
                    return status(pool, target.id, Some(actor))
                        .await
                        .map_err(|_| FollowError::CountsUnavailableAfterSave);
                }
                tracing::warn!(
                    actor_id = actor,
                    target_id = target.id,
                    "Follow notice unavailable"
                );
            }
        }
    } else {
        following::unfollow_user(pool, actor, target.id)
            .await
            .map_err(|_| FollowError::Unavailable)?;
    }
    status(pool, target.id, Some(actor))
        .await
        .map_err(|_| FollowError::CountsUnavailableAfterSave)
}
