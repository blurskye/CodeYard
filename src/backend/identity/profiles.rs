//! Shared profile updates; account records never double as public response models.

use crate::backend::db::{
    models::{Repository, User},
    repositories,
    users::accounts,
    DbPool,
};

pub struct ProfileUpdate<'a> {
    pub display_name: Option<&'a str>,
    pub bio: Option<&'a str>,
    pub location: Option<&'a str>,
    pub website: Option<&'a str>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("One or more profile fields exceed the supported length")]
    Invalid,
    #[error("Profile fields must not contain NUL characters")]
    Nul,
    #[error("Website must be a web address starting with http:// or https://")]
    Website,
    #[error("Profile update requires an unrestricted account and a live session")]
    Denied,
    #[error("Profile storage is temporarily unavailable")]
    Unavailable,
}

pub fn validate(input: &ProfileUpdate<'_>) -> Result<(), ProfileError> {
    for (value, bound) in [
        (input.display_name, 100),
        (input.bio, 4000),
        (input.location, 200),
        (input.website, 255),
    ] {
        if value.is_some_and(|value| value.contains('\0')) {
            return Err(ProfileError::Nul);
        }
        if value.is_some_and(|value| value.len() > bound) {
            return Err(ProfileError::Invalid);
        }
    }
    if let Some(website) = input.website {
        if crate::backend::db::repositories::topics::parse_website(website).is_err() {
            return Err(ProfileError::Website);
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProfileReadError {
    Missing,
    UserUnavailable,
    RepositoriesUnavailable,
}

/// Public profiles intentionally hide private repositories from everyone except
/// their personal owner, even when another viewer has collaborator access.
pub async fn repositories_on_profile(
    pool: &DbPool,
    username: &str,
    viewer: Option<i64>,
    text: &str,
    page: usize,
) -> Result<(User, Vec<Repository>, usize), ProfileReadError> {
    let user = accounts::get_user_by_username(pool, username)
        .await
        .map_err(|_| ProfileReadError::UserUnavailable)?
        .ok_or(ProfileReadError::Missing)?;
    let own = viewer == Some(user.id);
    let offset = i64::try_from(page.saturating_sub(1).saturating_mul(30)).unwrap_or(i64::MAX);
    let (repositories, count) =
        repositories::records::list_user_repository_page(pool, user.id, own, text, 30, offset)
            .await
            .map_err(|_| ProfileReadError::RepositoriesUnavailable)?;
    Ok((
        user,
        repositories,
        usize::try_from(count).unwrap_or(usize::MAX),
    ))
}

/// The profile's user, and how many repositories the profile lists to `viewer`. The Stars tab shows
/// that count without the list.
pub async fn repository_count_on_profile(
    pool: &DbPool,
    username: &str,
    viewer: Option<i64>,
) -> Result<(User, usize), ProfileReadError> {
    let user = accounts::get_user_by_username(pool, username)
        .await
        .map_err(|_| ProfileReadError::UserUnavailable)?
        .ok_or(ProfileReadError::Missing)?;
    let own = viewer == Some(user.id);
    let count = repositories::count_user_repositories(pool, user.id, own)
        .await
        .map_err(|_| ProfileReadError::RepositoriesUnavailable)?;
    Ok((user, usize::try_from(count).unwrap_or(usize::MAX)))
}

pub async fn update(
    pool: &DbPool,
    actor: i64,
    session: &str,
    input: ProfileUpdate<'_>,
) -> Result<User, ProfileError> {
    validate(&input)?;
    accounts::update_user_profile(
        pool,
        actor,
        session,
        input.display_name,
        input.bio,
        input.location,
        input.website,
    )
    .await
    .map_err(|_| ProfileError::Unavailable)?
    .ok_or(ProfileError::Denied)
}
