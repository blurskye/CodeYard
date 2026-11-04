//! Secondary addresses require their own confirmation and never authorize a password reset.
use crate::backend::{
    db::{
        self,
        users::{accounts::AccountCredential, emails::EmailRefusal},
        DbPool,
    },
    email::links,
};
use codeyard_support::credentials::AccountRestricted;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EmailError {
    #[error("Enter valid email addresses, at most 100 at a time")]
    Invalid,
    #[error("This email address is already registered")]
    Taken,
    #[error("The primary email address cannot be deleted")]
    Primary,
    #[error("At most 100 secondary email addresses may be added")]
    Limit,
    #[error("A live unrestricted account credential is required")]
    Denied,
    #[error("Email storage is temporarily unavailable")]
    Unavailable,
}

pub async fn change(
    pool: &DbPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    input: &[String],
    add: bool,
) -> Result<Vec<String>, EmailError> {
    if input.is_empty() || input.len() > 100 {
        return Err(EmailError::Invalid);
    }
    let mut emails = Vec::new();
    for address in input {
        let address = address.trim().to_ascii_lowercase();
        if super::credentials::validate_email(&address).is_err() {
            return Err(EmailError::Invalid);
        }
        emails.push(address);
    }
    emails.sort();
    emails.dedup();
    db::users::emails::change_emails(pool, actor, credential, &emails, add)
        .await
        .map_err(|error| {
            if error.is::<AccountRestricted>() {
                return EmailError::Denied;
            }
            match error.downcast_ref::<EmailRefusal>() {
                Some(EmailRefusal::Taken) => EmailError::Taken,
                Some(EmailRefusal::Primary) => EmailError::Primary,
                Some(EmailRefusal::Limit) => EmailError::Limit,
                None => EmailError::Unavailable,
            }
        })?;
    if add {
        let user = db::users::accounts::get_user_by_id(pool, actor)
            .await
            .map_err(|_| EmailError::Unavailable)?
            .ok_or(EmailError::Denied)?;
        if let Err(error @ (links::LinkError::Unavailable | links::LinkError::Limited)) =
            links::send_secondary_verifications(pool, &user, &emails).await
        {
            tracing::warn!(
                user_id = actor,
                ?error,
                "Secondary addresses saved but verification mail was not queued"
            );
        }
    }
    Ok(emails)
}
