//! Shared profile updates; account records never double as public response models.

use crate::backend::db::{
    models::{Repository, User},
    repositories,
    users::accounts,
    DbPool,
};

pub fn social_account(
    input: &str,
) -> Result<crate::backend::db::users::social_accounts::SocialAccount, ProfileError> {
    let url = url::Url::parse(input.trim()).map_err(|_| ProfileError::Website)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.as_str().len() > 255
        || input.contains(['\0', '\r', '\n'])
    {
        return Err(ProfileError::Website);
    }
    let host = url.host_str().unwrap().trim_start_matches("www.");
    let provider = match host {
        "twitter.com" | "x.com" => "twitter",
        "facebook.com" => "facebook",
        "linkedin.com" => "linkedin",
        "instagram.com" => "instagram",
        "youtube.com" => "youtube",
        "twitch.tv" => "twitch",
        _ => "generic",
    };
    Ok(crate::backend::db::users::social_accounts::SocialAccount {
        provider: provider.into(),
        url: url.into(),
    })
}

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
    #[error("Only a verified account email can be published")]
    Email,
    #[error("Profile update requires an unrestricted account and a live session")]
    Denied,
    #[error("Profile storage is temporarily unavailable")]
    Unavailable,
}

fn website(input: &str) -> Result<Option<String>, ProfileError> {
    let input = input.trim();
    let text = if matches!(url::Url::parse(input), Err(url::ParseError::RelativeUrlWithoutBase)) && !input.is_empty() {
        format!("https://{input}")
    } else {
        input.to_owned()
    };
    crate::backend::db::repositories::topics::parse_website(&text).map_err(|_| ProfileError::Website)
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
        if value.is_some_and(|value| value.chars().count() > bound) {
            return Err(ProfileError::Invalid);
        }
    }
    if let Some(value) = input.website {
        website(value)?;
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
    owner_session: Option<&str>,
    text: &str,
    page: usize,
) -> Result<(User, Vec<Repository>, usize), ProfileReadError> {
    let user = accounts::get_user_by_username(pool, username)
        .await
        .map_err(|_| ProfileReadError::UserUnavailable)?
        .ok_or(ProfileReadError::Missing)?;
    let offset = i64::try_from(page.saturating_sub(1).saturating_mul(30)).unwrap_or(i64::MAX);
    let (repositories, count) =
        repositories::records::list_user_repository_page(pool, user.id, owner_session, text, 30, offset)
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
    owner_session: Option<&str>,
) -> Result<(User, usize), ProfileReadError> {
    let user = accounts::get_user_by_username(pool, username)
        .await
        .map_err(|_| ProfileReadError::UserUnavailable)?
        .ok_or(ProfileReadError::Missing)?;
    let count = repositories::count_user_repositories(pool, user.id, owner_session)
        .await
        .map_err(|_| ProfileReadError::RepositoriesUnavailable)?;
    Ok((user, usize::try_from(count).unwrap_or(usize::MAX)))
}

pub async fn save(
    pool: &DbPool,
    actor: i64,
    credential: &accounts::AccountCredential<'_>,
    input: &crate::backend::db::users::profiles::ProfilePatch<'_>,
) -> Result<User, ProfileError> {
    validate(&ProfileUpdate {
        display_name: input.display_name,
        bio: input.bio,
        location: input.location,
        website: input.website,
    })?;
    for value in [input.company, input.twitter_username, input.email]
        .into_iter()
        .flatten()
    {
        if value.len() > 255 {
            return Err(ProfileError::Invalid);
        }
        if value.contains('\0') {
            return Err(ProfileError::Nul);
        }
    }
    if input
        .primary_visibility
        .is_some_and(|value| !matches!(value, "private" | "public"))
    {
        return Err(ProfileError::Invalid);
    }
    let normalized = input.website.map(website).transpose()?.flatten();
    let patch = crate::backend::db::users::profiles::ProfilePatch {
        website: input.website.map(|_| normalized.as_deref().unwrap_or("")),
        ..*input
    };
    crate::backend::db::users::profiles::save_profile(pool, actor, credential, &patch)
        .await
        .map_err(|error| {
            if error.is::<accounts::AccountRestricted>() {
                ProfileError::Denied
            } else if error.is::<crate::backend::db::users::profiles::InvalidPublicEmail>() {
                ProfileError::Email
            } else {
                ProfileError::Unavailable
            }
        })
}

pub async fn update(
    pool: &DbPool,
    actor: i64,
    session: &str,
    input: ProfileUpdate<'_>,
) -> Result<User, ProfileError> {
    validate(&input)?;
    let normalized = input.website.map(website).transpose()?.flatten();
    accounts::update_user_profile(
        pool,
        actor,
        session,
        input.display_name,
        input.bio,
        input.location,
        input.website.map(|_| normalized.as_deref().unwrap_or("")),
    )
    .await
    .map_err(|_| ProfileError::Unavailable)?
    .ok_or(ProfileError::Denied)
}
