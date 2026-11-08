use anyhow::Result;
use codeyard_database::models::User;
use codeyard_database::native_operation;
use codeyard_database::queries::users::accounts::AccountCredential;
use codeyard_database::DbPool;

#[derive(Default)]
pub struct ProfilePatch<'a> {
    pub display_name: Option<&'a str>,
    pub bio: Option<&'a str>,
    pub location: Option<&'a str>,
    pub website: Option<&'a str>,
    pub company: Option<&'a str>,
    pub twitter_username: Option<&'a str>,
    pub hireable: Option<bool>,
    pub email: Option<&'a str>,
    pub primary_visibility: Option<&'a str>,
}

#[derive(Clone, Debug, Default, sqlx::FromRow)]
pub struct ProfileSettings {
    pub company: Option<String>,
    pub twitter_username: Option<String>,
    pub hireable: Option<bool>,
    pub public_email: Option<String>,
    pub primary_visibility: String,
    pub secondary_email_verified: bool,
    pub primary_email: Option<String>,
    pub primary_email_verified: bool,
}

impl ProfileSettings {
    pub fn public_email(&self) -> Option<&str> {
        if let Some(selected) = self
            .public_email
            .as_deref()
            .filter(|email| !email.is_empty())
        {
            // The primary address follows the primary privacy setting even when it was once
            // published as a secondary address.
            let primary = self
                .primary_email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(selected));
            let shown = if primary {
                self.primary_visibility == "public" && self.primary_email_verified
            } else {
                self.secondary_email_verified
            };
            return shown.then_some(selected);
        }
        (self.primary_visibility == "public" && self.primary_email_verified)
            .then_some(self.primary_email.as_deref())
            .flatten()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Only a verified account email can be published")]
pub struct InvalidPublicEmail;

#[derive(sqlx::FromRow)]
pub struct AccountStatistics {
    pub public_gists: i64,
    pub private_gists: i64,
    pub owned_private_repos: i64,
    pub total_private_repos: i64,
    pub disk_usage: i64,
    pub collaborators: i64,
}

native_operation!(crate::postgres::users::profiles::account_statistics,
    crate::sqlite::users::profiles::account_statistics,
    fn account_statistics(pool: &DbPool, user: i64, token: i64) -> Result<Option<AccountStatistics>>);

native_operation!(crate::postgres::users::profiles::public_email,
    crate::sqlite::users::profiles::public_email,
    fn public_email(pool: &DbPool, user: i64) -> Result<Option<String>>);

native_operation!(crate::postgres::users::profiles::get_profile_settings,
    crate::sqlite::users::profiles::get_profile_settings,
    fn get_profile_settings(pool: &DbPool, user: i64) -> Result<ProfileSettings>);

native_operation!(crate::postgres::users::profiles::save_profile,
    crate::sqlite::users::profiles::save_profile,
    fn save_profile(pool: &DbPool, user: i64, credential: &AccountCredential<'_>, input: &ProfilePatch<'_>) -> Result<User>);
