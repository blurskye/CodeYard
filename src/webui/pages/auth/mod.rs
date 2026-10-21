//! Sign-in, registration, setup and sign-out pages: `/login`, `/register`, `/setup`, `/enter`, `/logout`.
mod page;
#[cfg(test)]
mod tests;

pub use page::{auth_page, logout_page, password_help_page, two_factor_page, AuthMode};
pub(in crate::webui::pages) use page::{password_field, password_field_with_id};
