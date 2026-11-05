//! Browser-bound single-use OAuth admission. Provider exchange stays outside transactions.

pub mod oidc;
pub mod providers;

pub fn known_provider(provider: &str) -> bool {
    providers::provider_meta(provider).is_some() || oidc::provider(provider).is_some()
}

use codeyard_db_identity::users::transitions::AccountAdmission;

use crate::sessions::SessionContext;
use crate::sessions::SessionIssueError;

use crate::AuthenticatedAccount;
use codeyard_db_core::DbPool;

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

pub fn validate_callback(
    code: &str,
    token: &str,
) -> Result<codeyard_support::cells::Prefix, OAuthError> {
    if code.is_empty() || code.len() > 4096 {
        return Err(OAuthError::InvalidCode);
    }
    codeyard_support::cells::Secret::cookie(token)
        .map(|token| token.cell)
        .map_err(|_| OAuthError::InvalidCode)
}

pub async fn require_setup(pool: &DbPool) -> Result<(), OAuthError> {
    let status = crate::setup::status(pool)
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
    if codeyard_support::cells::Secret::cookie(state_token)
        .ok()
        .is_none_or(|token| token.cell != pool.cell_prefix())
    {
        return Err(OAuthError::InvalidState);
    }
    if let Some(configured) = oidc::provider(provider) {
        require_setup(pool).await?;
        return configured
            .begin(
                pool,
                redirect,
                browser_nonce,
                state_token,
                verifier,
                return_to,
            )
            .await
            .map_err(|_| OAuthError::StateUnavailable);
    }
    let meta = providers::provider_meta(provider).ok_or(OAuthError::UnknownProvider)?;
    let (client, _) = providers::provider_credentials(provider).ok_or(OAuthError::Disabled)?;
    require_setup(pool).await?;
    codeyard_db_identity::users::sessions::save_bound_oauth_state(
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
    if let Some(configured) = oidc::provider(provider) {
        if validate_callback(code, token)? != pool.cell_prefix() {
            return Err(OAuthError::InvalidState);
        }
        require_setup(pool).await?;
        let (profile, return_to) = configured
            .complete(pool, code, token, browser_nonce, redirect)
            .await?;
        return admit_profile(
            pool,
            provider,
            profile,
            session,
            return_to,
            Some(&configured.organization),
        )
        .await;
    }
    let meta = providers::provider_meta(provider).ok_or(OAuthError::UnknownProvider)?;
    let (client_id, secret) =
        providers::provider_credentials(provider).ok_or(OAuthError::Disabled)?;
    if validate_callback(code, token)? != pool.cell_prefix() {
        return Err(OAuthError::InvalidState);
    }
    require_setup(pool).await?;
    let stored = codeyard_db_identity::users::sessions::consume_bound_oauth_state(
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
    admit_profile(pool, provider, profile, session, stored.return_to, None).await
}

async fn admit_profile(
    pool: &DbPool,
    provider: &str,
    profile: providers::OAuthProfile,
    session: SessionContext<'_>,
    return_to: String,
    organization: Option<&str>,
) -> Result<(AuthenticatedAccount, String), OAuthError> {
    let base = providers::username_from_profile(profile.name.as_deref(), &profile.provider_user_id);
    let name = profile.name.as_deref().filter(|name| name.len() <= 255);
    let user = match codeyard_db_identity::users::transitions::admit_oauth_account(
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
    let credential = match organization {
        Some(organization) => {
            codeyard_db_identity::users::sessions::SessionCredential::OrganizationOidc {
                organization,
                provider,
                subject: &profile.provider_user_id,
            }
        }
        None => codeyard_db_identity::users::sessions::SessionCredential::OAuth,
    };
    let session_id =
        crate::sessions::issue_authenticated_session(pool, session, user.id, credential)
            .await
            .map_err(|error| match (organization, error) {
                (Some(_), SessionIssueError::Denied) => OAuthError::Denied,
                (_, error) => OAuthError::AccountAdmittedSessionFailed(error),
            })?;
    Ok((AuthenticatedAccount { user, session_id }, return_to))
}
