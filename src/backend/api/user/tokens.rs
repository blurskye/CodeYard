use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::{ApiError, ApiFailure};
use crate::backend::api::http::session::{extract_session_id, get_current_user};
use crate::backend::api::user::profile::{load_settings, settings_response, settings_result};
use crate::backend::api::user::responses::{field, mutation_guard, Fields};
use crate::backend::app::AppState;
use crate::backend::db::models::PersonalAccessToken;
use crate::backend::db::users::tokens as stored;
use crate::backend::identity::tokens;
use crate::backend::identity::tokens::{TokenError, TokenInput};
use crate::webui::pages::account;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTokenRequest {
    pub name: String,
    pub scope: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct CreatedToken {
    #[serde(flatten)]
    pub token: PersonalAccessToken,
    pub value: String,
}

pub(crate) fn token_failure(error: TokenError) -> ApiFailure {
    let (status, code) = match error {
        TokenError::Invalid => (StatusCode::BAD_REQUEST, "validation_error"),
        TokenError::Denied => (StatusCode::FORBIDDEN, "forbidden"),
        TokenError::Missing => (StatusCode::NOT_FOUND, "not_found"),
        TokenError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    (status, Json(ApiError::new(code, &error.to_string())))
}

pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<PersonalAccessToken>>, ApiFailure> {
    let user = get_current_user(&state.pool, &headers).await?;
    Ok(Json(
        stored::list_tokens(&state.pool, user.id)
            .await
            .map_err(|_| token_failure(TokenError::Unavailable))?,
    ))
}

pub async fn create_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<CreateTokenRequest>,
) -> Result<(StatusCode, Json<CreatedToken>), ApiFailure> {
    let user = get_current_user(&state.pool, &headers).await?;
    let session_id = extract_session_id(&headers);
    let created = tokens::create(
        &state.pool,
        user.id,
        session_id.as_deref(),
        TokenInput {
            name: &request.name,
            scope: &request.scope,
            expires_at: request.expires_at,
        },
    )
    .await
    .map_err(token_failure)?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token: created.token,
            value: created.value,
        }),
    ))
}

pub async fn delete_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiFailure> {
    let user = get_current_user(&state.pool, &headers).await?;
    tokens::delete(&state.pool, user.id, id)
        .await
        .map_err(token_failure)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The token form posts `expiry` as `never`, or as `date` with `expires_at` set to a calendar
/// day. A day expires at its last second in UTC. `None` means the choice is invalid.
fn form_expiry(form: &Fields) -> Option<Option<DateTime<Utc>>> {
    match field(form, "expiry") {
        "never" => Some(None),
        "date" => {
            let day = NaiveDate::parse_from_str(field(form, "expires_at"), "%Y-%m-%d").ok()?;
            Some(Some(day.and_hms_opt(23, 59, 59)?.and_utc()))
        }
        _ => None,
    }
}

pub(crate) async fn token_add_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let expires_at = match form_expiry(&form) {
        Some(expires_at) => expires_at,
        None => {
            return settings_response(
                &ctx,
                "tokens",
                Some(&form),
                StatusCode::BAD_REQUEST,
                Some("Choose a valid expiry date, or choose Never expires."),
            )
            .await
        }
    };
    let actor = ctx
        .user
        .as_ref()
        .expect("mutation guard checked account")
        .id;
    let session_id = extract_session_id(&ctx.headers);
    let result = tokens::create(
        &ctx.state.pool,
        actor,
        session_id.as_deref(),
        TokenInput {
            name: field(&form, "name"),
            scope: field(&form, "scope"),
            expires_at,
        },
    )
    .await
    .map_err(token_failure);
    match result {
        Ok(created) => {
            let mut model = match load_settings(&ctx, "tokens", None).await {
                Ok(model) => model,
                Err(response) => return ctx.error(response.status(), "Your token was created, but settings could not be loaded. If its value was not shown, delete it and create another when settings are available."),
            };
            model.new_token = Some(created.value);
            ctx.page_model(
                "Settings",
                crate::webui::app::Page::Identity(account::Page::Settings {
                    model: Box::new(model),
                    csrf_token: ctx.csrf_token.clone(),
                    message: None,
                    is_error: false,
                }),
            )
        }
        Err(error) => settings_result(&ctx, "tokens", &form, Err::<(), _>(error), "token").await,
    }
}

pub(crate) async fn token_delete_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let actor = ctx
        .user
        .as_ref()
        .expect("mutation guard checked account")
        .id;
    // A revoked token stops working at once, so the server refuses a delete that did not confirm.
    let result = if field(&form, "confirm") == "yes" {
        tokens::delete(&ctx.state.pool, actor, id)
            .await
            .map_err(token_failure)
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "confirmation_required",
                "Confirm the deletion before the token is revoked.",
            )),
        ))
    };
    settings_result(&ctx, "tokens", &form, result, "token-deleted").await
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn form(expiry: &str, expires_at: &str) -> Fields {
        [("expiry", expiry), ("expires_at", expires_at)]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    #[test]
    fn never_expires_ignores_a_date_left_in_the_field() {
        assert_eq!(form_expiry(&form("never", "2027-01-15")), Some(None));
    }

    #[test]
    fn a_chosen_day_expires_at_its_last_second_in_utc() {
        let expected = Utc.with_ymd_and_hms(2027, 1, 15, 23, 59, 59).unwrap();
        assert_eq!(
            form_expiry(&form("date", "2027-01-15")),
            Some(Some(expected))
        );
    }

    #[test]
    fn missing_or_malformed_choices_are_refused() {
        assert_eq!(form_expiry(&form("date", "")), None);
        assert_eq!(form_expiry(&form("date", "2027-02-30")), None);
        assert_eq!(form_expiry(&form("date", "2027-01-15T00:00:00Z")), None);
        assert_eq!(form_expiry(&form("forever", "")), None);
        assert_eq!(form_expiry(&Fields::new()), None);
    }
}
