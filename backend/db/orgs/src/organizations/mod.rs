use codeyard_db_core::models::OrgRole;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationInputError {
    Slug,
    Name,
    Description,
    Role,
    Privacy,
    Parent,
    Permission,
    Page,
}

#[derive(Debug, Clone, Copy)]
pub struct OrganizationPage {
    pub after: i64,
    pub limit: i64,
}
impl OrganizationPage {
    pub fn new(after: i64, limit: i64) -> std::result::Result<Self, OrganizationInputError> {
        if after < 0 || !(1..=100).contains(&limit) {
            return Err(OrganizationInputError::Page);
        }
        Ok(Self { after, limit })
    }
}
/// The most repositories one organization selection (secret visibility, immutable releases)
/// may name, so validating it stays one bounded query.
pub(crate) const MAX_SCOPED_REPOSITORIES: usize = 1000;

pub(crate) fn can_change_member(
    actor: OrgRole,
    old: Option<OrgRole>,
    new: Option<OrgRole>,
) -> bool {
    actor == OrgRole::Owner
        || (actor == OrgRole::Admin
            && old.is_none_or(|role| role == OrgRole::Member)
            && new.is_none_or(|role| role == OrgRole::Member))
}

pub mod access;
pub mod blocks;
pub mod deletion;
pub mod discovery;
pub mod external_properties;
pub mod immutable_releases;
pub mod invitations;
pub mod membership;
pub mod outside;
pub mod profiles;
pub mod properties;
pub mod recovery;
pub mod roles;
pub mod settings;
pub mod team_invitations;
pub mod teams;
