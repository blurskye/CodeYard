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
) -> Result<(User, Vec<Repository>), ProfileReadError> {
    let user = accounts::get_user_by_username(pool, username)
        .await
        .map_err(|_| ProfileReadError::UserUnavailable)?
        .ok_or(ProfileReadError::Missing)?;
    let own = viewer == Some(user.id);
    let repositories = repositories::list_user_repositories(pool, user.id)
        .await
        .map_err(|_| ProfileReadError::RepositoriesUnavailable)?
        .into_iter()
        .filter(|repository| visible_on_profile(repository.is_private, own))
        .collect();
    Ok((user, repositories))
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
    input: ProfileUpdate<'_>,
) -> Result<User, ProfileError> {
    validate(&input)?;
    accounts::update_user_profile(
        pool,
        actor,
        input.display_name,
        input.bio,
        input.location,
        input.website,
    )
    .await
    .map_err(|_| ProfileError::Unavailable)
}

pub fn visible_on_profile(private: bool, own: bool) -> bool {
    !private || own
}
