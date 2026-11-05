//! Signed-in account settings and native forms.

use codeyard_api_http::cell_routes::CellRoutes;
mod appearance;
pub mod avatar;
mod blocks;
pub mod email;
pub mod fine_grained;
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
) -> axum::routing::MethodRouter<std::sync::Arc<codeyard_app::app::AppState>> {
    axum::routing::get(move || async move {
        codeyard_api_http::render::redirect_response(&format!("/settings?tab={tab}"))
    })
}

pub fn router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
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
                .cell_route(
                    "/settings/gpg-keys",
                    back_to_settings("keys").post(crate::user::gpg::add_post),
                )
                .layer(DefaultBodyLimit::max(3 * 64 * 1024)),
        )
        .merge(
            Router::new()
                .cell_route("/_forms/theme", post(crate::user::appearance::set_theme))
                .cell_route("/settings/appearance", back_to_settings("appearance"))
                .cell_route("/settings", get(crate::user::profile::settings_get))
                .cell_route(
                    "/settings/profile",
                    back_to_settings("profile").post(crate::user::profile::profile_post),
                )
                .cell_route(
                    "/settings/social-accounts",
                    back_to_settings("profile").post(crate::user::profile::social_accounts_post),
                )
                .cell_route(
                    "/settings/password",
                    back_to_settings("password").post(crate::auth::change_password::password_post),
                )
                .cell_route(
                    "/settings/gpg-keys/:id/delete",
                    back_to_settings("keys").post(crate::user::gpg::delete_post),
                )
                .cell_route(
                    "/settings/pull-request-cap",
                    back_to_settings("blocked").post(crate::user::interactions::post_cap),
                )
                .cell_route(
                    "/settings/pull-request-bypass",
                    back_to_settings("blocked").post(crate::user::interactions::post_bypass),
                )
                .cell_route(
                    "/settings/interaction-limits",
                    back_to_settings("blocked").post(crate::user::interactions::post),
                )
                .cell_route(
                    "/settings/blocks",
                    back_to_settings("blocked").post(crate::user::blocks::blocks_post),
                )
                .cell_route(
                    "/settings/email",
                    back_to_settings("email").post(crate::user::email::email_post),
                )
                .cell_route(
                    "/settings/email-verification",
                    back_to_settings("email").post(crate::user::email::verification_post),
                )
                .cell_route(
                    "/settings/email-verification/code",
                    back_to_settings("email").post(crate::user::email::verification_code_post),
                )
                .cell_route(
                    "/settings/secondary-emails",
                    back_to_settings("email").post(crate::user::email::secondary_post),
                )
                .cell_route(
                    "/settings/secondary-emails/verify",
                    back_to_settings("email").post(crate::user::email::secondary_verification_post),
                )
                .cell_route(
                    "/settings/email-visibility",
                    back_to_settings("email").post(crate::user::email::visibility_post),
                )
                .cell_route(
                    "/settings/email-notifications",
                    back_to_settings("email").post(crate::user::email::notifications_post),
                )
                .cell_route(
                    "/settings/sessions/revoke",
                    back_to_settings("sessions").post(crate::user::sessions::revoke_post),
                )
                .cell_route(
                    "/settings/sessions/revoke-others",
                    back_to_settings("sessions").post(crate::user::sessions::revoke_others_post),
                )
                .cell_route(
                    "/settings/two-factor/setup",
                    back_to_settings("two-factor").post(crate::user::two_factor::setup_post),
                )
                .cell_route(
                    "/settings/two-factor/enable",
                    back_to_settings("two-factor").post(crate::user::two_factor::enable_post),
                )
                .cell_route(
                    "/settings/two-factor/recovery-codes",
                    back_to_settings("two-factor")
                        .post(crate::user::two_factor::recovery_codes_post),
                )
                .cell_route(
                    "/settings/two-factor/disable",
                    back_to_settings("two-factor").post(crate::user::two_factor::disable_post),
                )
                .cell_route(
                    "/settings/keys",
                    back_to_settings("keys").post(crate::user::keys::key_add_post),
                )
                .cell_route(
                    "/settings/signing-keys",
                    back_to_settings("keys").post(crate::user::keys::signing_key_add_post),
                )
                .cell_route(
                    "/settings/signing-keys/:key_id/delete",
                    back_to_settings("keys").post(crate::user::keys::signing_key_delete_post),
                )
                .cell_route(
                    "/settings/keys/:key_id/delete",
                    back_to_settings("keys").post(crate::user::keys::key_delete_post),
                )
                .cell_route(
                    "/settings/tokens",
                    back_to_settings("tokens").post(crate::user::tokens::token_add_post),
                )
                .cell_route(
                    "/settings/tokens/:token_id/delete",
                    back_to_settings("tokens").post(crate::user::tokens::token_delete_post),
                )
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .merge(crate::user::fine_grained::router().layer(DefaultBodyLimit::max(64 * 1024)))
        .merge(crate::user::avatar::router())
}
