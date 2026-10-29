//! Signed-in account settings and native forms.

mod appearance;
pub mod avatar;
mod blocks;
pub mod email;
mod gpg;
pub(crate) mod interactions;
pub mod keys;
pub mod profile;
pub mod responses;
pub mod sessions;
pub mod tokens;
pub mod two_factor;

/// A rejected settings form is shown at its POST-only URL. A GET there, from a reload, a
/// copied address or a typed one, goes to the settings tab the form belongs to.
fn back_to_settings(
    tab: &'static str,
) -> axum::routing::MethodRouter<std::sync::Arc<crate::backend::app::AppState>> {
    axum::routing::get(move || async move {
        crate::backend::api::http::render::redirect_response(&format!("/settings?tab={tab}"))
    })
}

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use crate::backend::api::auth::change_password;
    use axum::{
        extract::DefaultBodyLimit,
        routing::{get, post},
        Router,
    };
    Router::new()
        // A 64 KiB armored key grows when its form is percent-encoded, so this form has room
        // for it at three times its size; the key itself is still checked against 64 KiB.
        .merge(
            Router::new()
                .route(
                    "/settings/gpg-keys",
                    back_to_settings("keys").post(gpg::add_post),
                )
                .layer(DefaultBodyLimit::max(3 * 64 * 1024)),
        )
        .merge(
            Router::new()
                .route("/_forms/theme", post(appearance::set_theme))
                .route("/settings/appearance", back_to_settings("appearance"))
                .route("/settings", get(profile::settings_get))
                .route(
                    "/settings/profile",
                    back_to_settings("profile").post(profile::profile_post),
                )
                .route(
                    "/settings/social-accounts",
                    back_to_settings("profile").post(profile::social_accounts_post),
                )
                .route(
                    "/settings/password",
                    back_to_settings("password").post(change_password::password_post),
                )
                .route(
                    "/settings/gpg-keys/:id/delete",
                    back_to_settings("keys").post(gpg::delete_post),
                )
                .route(
                    "/settings/pull-request-cap",
                    back_to_settings("blocked").post(interactions::post_cap),
                )
                .route(
                    "/settings/pull-request-bypass",
                    back_to_settings("blocked").post(interactions::post_bypass),
                )
                .route(
                    "/settings/interaction-limits",
                    back_to_settings("blocked").post(interactions::post),
                )
                .route(
                    "/settings/blocks",
                    back_to_settings("blocked").post(blocks::blocks_post),
                )
                .route(
                    "/settings/email",
                    back_to_settings("email").post(email::email_post),
                )
                .route(
                    "/settings/email-verification",
                    back_to_settings("email").post(email::verification_post),
                )
                .route(
                    "/settings/email-verification/code",
                    back_to_settings("email").post(email::verification_code_post),
                )
                .route(
                    "/settings/secondary-emails",
                    back_to_settings("email").post(email::secondary_post),
                )
                .route(
                    "/settings/secondary-emails/verify",
                    back_to_settings("email").post(email::secondary_verification_post),
                )
                .route(
                    "/settings/email-visibility",
                    back_to_settings("email").post(email::visibility_post),
                )
                .route(
                    "/settings/email-notifications",
                    back_to_settings("email").post(email::notifications_post),
                )
                .route(
                    "/settings/sessions/revoke",
                    back_to_settings("sessions").post(sessions::revoke_post),
                )
                .route(
                    "/settings/sessions/revoke-others",
                    back_to_settings("sessions").post(sessions::revoke_others_post),
                )
                .route(
                    "/settings/two-factor/setup",
                    back_to_settings("two-factor").post(two_factor::setup_post),
                )
                .route(
                    "/settings/two-factor/enable",
                    back_to_settings("two-factor").post(two_factor::enable_post),
                )
                .route(
                    "/settings/two-factor/recovery-codes",
                    back_to_settings("two-factor").post(two_factor::recovery_codes_post),
                )
                .route(
                    "/settings/two-factor/disable",
                    back_to_settings("two-factor").post(two_factor::disable_post),
                )
                .route(
                    "/settings/keys",
                    back_to_settings("keys").post(keys::key_add_post),
                )
                .route(
                    "/settings/signing-keys",
                    back_to_settings("keys").post(keys::signing_key_add_post),
                )
                .route(
                    "/settings/signing-keys/:key_id/delete",
                    back_to_settings("keys").post(keys::signing_key_delete_post),
                )
                .route(
                    "/settings/keys/:key_id/delete",
                    back_to_settings("keys").post(keys::key_delete_post),
                )
                .route(
                    "/settings/tokens",
                    back_to_settings("tokens").post(tokens::token_add_post),
                )
                .route(
                    "/settings/tokens/:token_id/delete",
                    back_to_settings("tokens").post(tokens::token_delete_post),
                )
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .merge(avatar::router())
}
