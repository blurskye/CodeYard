//! Organization pages: the organization list and settings, the public profile, and team pages.
mod avatar;
mod fields;
mod governance;
mod membership;
mod page;
mod profile;
mod team_page;
mod teams;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;

use page::privacy_label;
pub use page::{
    render, Member, Organization, Page, RepositoryChoice, RepositoryGrant, Team, TeamDetail,
    REPOSITORY_CHOICE_LIMIT,
};
pub use profile::{profile_page, OrganizationProfile};
pub use team_page::{team_page, TeamPage};
