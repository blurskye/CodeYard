//! Organization pages: the organization list and settings, the public profile, and team pages.
pub mod actions;
mod avatar;
mod blocks;
pub mod code_security;
mod collaborators;
pub mod dependabot;
mod fields;
mod governance;
mod membership;
mod page;
pub mod private_registries;
mod profile;
mod properties;
mod roles;
mod settings;
mod team_page;
mod teams;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
mod tokens;

use page::privacy_label;
pub use page::{
    render, BlockedUser, Invitation, Member, Organization, OutsideCollaborator, Page,
    PendingInvitation, RepositoryChoice, RepositoryGrant, RoleDefinition, RolePanel, RoleTeam,
    RoleUser, Team, TeamDetail, REPOSITORY_CHOICE_LIMIT,
};
pub use profile::{profile_page, OrganizationProfile, ProfileContacts};
pub use properties::{PropertyDefinition, PropertyPanel, PropertySummary};
pub use settings::SettingsPanel;
pub use team_page::{team_page, TeamPage};
pub use tokens::{TokenGrant, TokenPanel};
