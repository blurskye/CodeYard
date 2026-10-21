//! Browser-bound single-use OAuth admission. Provider exchange stays outside transactions.

pub mod providers;

use crate::backend::{
    crypto::is_hex_token,
    db::{
        users::{
            sessions as states,
            transitions::{self, AccountAdmission},
        },
        DbPool,
    },
    identity::{
        sessions::{self, SessionContext, SessionIssueError},
        setup, AuthenticatedAccount,
    },
};

#[derive(Debug, PartialEq, Eq)]
pub enum OAuthError {
    UnknownProvider,
    Disabled,
    InvalidCode,
    InvalidState,
    InvalidPkce,
    SetupRequired,
    StateUnavailable,
    AccountUnavailable,
    ClientUnavailable,
    ProviderFailed,
    ApprovalRequired,
    Conflict,
    Denied,
    // The account admission has already committed; session failure cannot undo it.
    AccountAdmittedSessionFailed(SessionIssueError),
}

pub fn validate_callback(code: &str, token: &str) -> Result<(), OAuthError> {
    if code.is_empty() || code.len() > 4096 || !is_hex_token(token, 64) {
        return Err(OAuthError::InvalidCode);
    }
    Ok(())
}

pub async fn require_setup(pool: &DbPool) -> Result<(), OAuthError> {
    let status = setup::status(pool)
        .await
        .map_err(|_| OAuthError::AccountUnavailable)?;
    if !status.setup_completed {
        return Err(OAuthError::SetupRequired);
    }
    Ok(())
}

pub async fn begin(
    pool: &DbPool,
    provider: &str,
    redirect: &str,
    browser_nonce: &str,
    state_token: &str,
    verifier: &str,
    return_to: &str,
) -> Result<String, OAuthError> {
    let meta = providers::provider_meta(provider).ok_or(OAuthError::UnknownProvider)?;
    let (client, _) = providers::provider_credentials(provider).ok_or(OAuthError::Disabled)?;
    require_setup(pool).await?;
    states::save_bound_oauth_state(
        pool,
        state_token,
        verifier,
        provider,
        redirect,
        &providers::browser_binding(browser_nonce),
        return_to,
    )
    .await
    .map_err(|_| OAuthError::StateUnavailable)?;
    Ok(providers::authorization_with_pkce(
        &meta,
        &client,
        redirect,
        state_token,
        verifier,
    ))
}

pub struct Callback<'a> {
    pub provider: &'a str,
    pub code: &'a str,
    pub token: &'a str,
    pub browser_nonce: &'a str,
    pub redirect: &'a str,
    pub session: SessionContext<'a>,
}

pub async fn complete(
    pool: &DbPool,
    callback: Callback<'_>,
) -> Result<(AuthenticatedAccount, String), OAuthError> {
    let Callback {
        provider,
        code,
        token,
        browser_nonce,
        redirect,
        session,
    } = callback;
    let meta = providers::provider_meta(provider).ok_or(OAuthError::UnknownProvider)?;
    let (client_id, secret) =
        providers::provider_credentials(provider).ok_or(OAuthError::Disabled)?;
    validate_callback(code, token)?;
    require_setup(pool).await?;
    let stored = states::consume_bound_oauth_state(
        pool,
        token,
        provider,
        &providers::browser_binding(browser_nonce),
        redirect,
    )
    .await
    .map_err(|_| OAuthError::StateUnavailable)?
    .ok_or(OAuthError::InvalidState)?;
    if providers::supports_pkce(provider) && stored.pkce_verifier.len() != 64 {
        return Err(OAuthError::InvalidPkce);
    }
    let client = providers::provider_client().map_err(|_| OAuthError::ClientUnavailable)?;
    let body = providers::token_body(
        provider,
        &client_id,
        &secret,
        code,
        redirect,
        &stored.pkce_verifier,
    );
    let basic = (provider == "bitbucket").then_some((client_id.as_str(), secret.as_str()));
    let profile = providers::exchange_profile(
        &client,
        provider,
        meta.token_url,
        meta.user_info_url,
        body,
        basic,
    )
    .await
    .map_err(|_| OAuthError::ProviderFailed)?;
    let base = providers::username_from_profile(profile.name.as_deref(), &profile.provider_user_id);
    let name = profile.name.as_deref().filter(|name| name.len() <= 255);
    let user = match transitions::admit_oauth_account(
        pool,
        provider,
        &profile.provider_user_id,
        &base,
        name,
    )
    .await
    .map_err(|_| OAuthError::AccountUnavailable)?
    {
        AccountAdmission::User(user) => *user,
        AccountAdmission::SetupRequired => return Err(OAuthError::SetupRequired),
        AccountAdmission::ApprovalRequired => return Err(OAuthError::ApprovalRequired),
        AccountAdmission::Conflict => return Err(OAuthError::Conflict),
        _ => return Err(OAuthError::Denied),
    };
    let session_id = sessions::issue_authenticated_session(pool, session, user.id, None)
        .await
        .map_err(OAuthError::AccountAdmittedSessionFailed)?;
    Ok((AuthenticatedAccount { user, session_id }, stored.return_to))
}
