//! Account settings: `/settings`, and the forms that post back to it.
mod page;
#[cfg(test)]
mod tests;

pub use page::{
    settings_nav, settings_page, settings_tab, AccountSession, Key, Settings, Token, TwoFactor,
    TwoFactorSetup,
};
