//! Account settings: `/settings`, and the forms that post back to it.
pub mod fine_grained;
mod page;
#[cfg(test)]
mod tests;

pub(crate) use page::{section_nav, NavGroup};
pub use page::{
    settings_nav, settings_page, settings_tab, AccountSession, BlockedAccounts, EmailVerification,
    Key, SecondaryEmail, SessionPagination, Settings, Token, TwoFactor, TwoFactorSetup,
};
