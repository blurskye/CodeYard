//! Shared SSR and hydrated identity views; presentation models never contain DB records.
use crate::webui::{
    components::{compact_date, DraftSelect, DraftTextarea, SelectOption},
    navigation::NavLink,
};
use dioxus::prelude::*;
use std::collections::HashMap;

const FIELD: &str = "w-full bg-background border border-border text-sm text-foreground focus:border-primary focus:outline-none";
const SUBMIT: &str = "btn btn-primary";
const PANEL: &str = "panel rounded-md border border-border bg-card p-6 space-y-5";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AuthMode {
    Login,
    Register,
    Setup,
}
impl AuthMode {
    pub fn title(self) -> &'static str {
        match self {
            Self::Login => "Sign in to Codeyard",
            Self::Register => "Create your account",
            Self::Setup => "Set up your node",
        }
    }
}

/// Explicit recovery fields prevent a generic form map from serializing secrets.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormValues {
    pub username: String,
    pub email: String,
    pub node_mode: String,
    pub display_name: String,
    pub bio: String,
    pub location: String,
    pub website: String,
    pub name: String,
    pub public_key: String,
    pub scope: String,
    pub expiry: String,
    pub expires_at: String,
    pub redirect: String,
    pub mode: String,
    pub days: String,
}
impl FormValues {
    pub fn get(&self, key: &str) -> String {
        match key {
            "username" => &self.username,
            "email" => &self.email,
            "node_mode" => &self.node_mode,
            "display_name" => &self.display_name,
            "bio" => &self.bio,
            "location" => &self.location,
            "website" => &self.website,
            "name" => &self.name,
            "public_key" => &self.public_key,
            "scope" => &self.scope,
            "expiry" => &self.expiry,
            "expires_at" => &self.expires_at,
            "redirect" => &self.redirect,
            "mode" => &self.mode,
            "days" => &self.days,
            _ => return String::new(),
        }
        .clone()
    }
}

/// Only explicitly non-secret values may survive a failed submission.
pub fn preserved_values(form: &HashMap<String, String>) -> FormValues {
    let get = |name: &str| form.get(name).cloned().unwrap_or_default();
    let public_key = get("public_key");
    FormValues {
        username: get("username"),
        email: get("email"),
        node_mode: get("node_mode"),
        display_name: get("display_name"),
        bio: get("bio"),
        location: get("location"),
        website: get("website"),
        name: get("name"),
        scope: get("scope"),
        expiry: get("expiry"),
        expires_at: get("expires_at"),
        public_key: if public_key.contains("PRIVATE KEY") {
            String::new()
        } else {
            public_key
        },
        redirect: get("redirect"),
        mode: get("mode"),
        days: get("days"),
    }
}

/// Authorized presentation state shared by initial SSR and client navigation.
/// Booleans only describe visible controls; backend POSTs recheck all authority.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Auth {
        mode: AuthMode,
        action: String,
        csrf_token: String,
        values: FormValues,
        private: bool,
        /// Ids of the configured OAuth providers, such as `github`, in display order.
        #[serde(default)]
        providers: Vec<String>,
        redirect: String,
        message: Option<String>,
        is_error: bool,
    },
    Logout {
        csrf_token: String,
    },
    Settings {
        model: Box<Settings>,
        csrf_token: String,
        message: Option<String>,
        is_error: bool,
    },
    Admin {
        model: Admin,
        csrf_token: String,
        message: Option<String>,
        is_error: bool,
    },
    BanConfirmation {
        username: String,
        user_id: i64,
        csrf_token: String,
        days: i32,
    },
    Profile {
        profile: Profile,
        repositories: Vec<Repository>,
        csrf_token: String,
        own: bool,
        authenticated: bool,
        following: bool,
        followers: i64,
        follows: i64,
        message: Option<String>,
        activity: ProfileActivity,
    },
}

pub fn render(page: Page) -> Element {
    match page {
        Page::Auth {
            mode,
            action,
            csrf_token,
            values,
            private,
            providers,
            redirect,
            message,
            is_error,
        } => auth_page(
            mode,
            &action,
            &csrf_token,
            values,
            private,
            providers,
            &redirect,
            message.as_deref(),
            is_error,
        ),
        Page::Logout { csrf_token } => logout_page(&csrf_token),
        Page::Settings {
            model,
            csrf_token,
            message,
            is_error,
        } => settings_page(*model, &csrf_token, message.as_deref(), is_error),
        Page::Admin {
            model,
            csrf_token,
            message,
            is_error,
        } => admin_page(model, &csrf_token, message.as_deref(), is_error),
        Page::BanConfirmation {
            username,
            user_id,
            csrf_token,
            days,
        } => ban_confirmation_page(&username, user_id, &csrf_token, days),
        Page::Profile {
            profile,
            repositories,
            csrf_token,
            own,
            authenticated,
            following,
            followers,
            follows,
            message,
            activity,
        } => profile_page(
            profile,
            repositories,
            activity,
            &csrf_token,
            own,
            authenticated,
            following,
            followers,
            follows,
            message.as_deref(),
        ),
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub username: String,
    pub display_name: String,
    pub bio: String,
    pub location: String,
    pub website: String,
    pub avatar_url: Option<String>,
    pub joined: String,
}
/// One item of a profile's recent activity. `url` leads to what it is about.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProfileActivityItem {
    pub kind: String,
    pub created_at: String,
    pub repository: String,
    pub url: String,
    pub number: Option<i32>,
    pub title: Option<String>,
    pub branch: Option<String>,
    pub commits: Option<i32>,
}

/// A page of recent activity. `next_page` is set when another page has items.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProfileActivity {
    pub items: Vec<ProfileActivityItem>,
    pub next_page: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Repository {
    pub name: String,
    pub description: String,
    pub private: bool,
    pub archived: bool,
    pub stars: i64,
    pub forks: i64,
    pub updated: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Key {
    pub id: i64,
    pub name: String,
    pub fingerprint: String,
    pub kind: String,
    pub created: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Signup {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub created: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Account {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub admin: bool,
    pub banned: bool,
    pub must_change_password: bool,
    pub files: i64,
    pub bytes: i64,
    pub commits: i64,
    pub pushes: i64,
    pub contributions: i64,
    pub created: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Activity {
    pub date: String,
    pub contributions: i64,
    pub cumulative_contributions: i64,
    pub bytes: i64,
    pub cumulative_bytes: i64,
}

pub fn settings_tab(raw: Option<&str>) -> &'static str {
    match raw {
        Some("appearance") => "appearance",
        Some("keys") => "keys",
        Some("tokens") => "tokens",
        Some("password") => "password",
        Some("access") => "access",
        _ => "profile",
    }
}
/// Whitelist public website link schemes. Unsafe stored values remain editable text.
pub fn safe_website(raw: &str) -> Option<&str> {
    let value = raw.trim();
    if !value.chars().any(|c| c.is_control() || c == '\\')
        && (value.starts_with("https://") || value.starts_with("http://"))
    {
        let authority = value.split_once("://")?.1.split(['/', '?', '#']).next()?;
        if !authority.is_empty() && !authority.contains('@') {
            return Some(value);
        }
    }
    None
}

mod administration;
mod auth;
mod fields;
mod profile;
mod settings;

pub use administration::{admin_page, ban_confirmation_page, normalize_days, Admin};
use auth::password_field;
pub use auth::{auth_page, logout_page};
use fields::{access_forms, csrf, notice, text_field};
pub(crate) use profile::{profile_icon, ProfileIcon};
pub use profile::{profile_page, ProfileRepositories};
pub use settings::{settings_nav, settings_page, Settings, Token};

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
