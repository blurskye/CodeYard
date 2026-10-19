//! Authoritative organization operations, dispatched while holding DB selection.
//! Site administrators have NO implicit organization membership/management here.
//! Ingress must supply a verified actor; IDs never imply user/org equivalence.
use crate::backend::db::models::OrgRole;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrganizationOutcome<T> {
    Missing,
    Denied,
    Conflict,
    Suspended,
    Applied(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationInputError {
    Slug,
    Name,
    Description,
    Role,
    Privacy,
    Permission,
    Page,
}

#[derive(Debug, Clone, Copy)]
pub struct OrganizationPage {
    pub(in crate::backend::db) after: i64,
    pub(in crate::backend::db) limit: i64,
}
impl OrganizationPage {
    pub fn new(after: i64, limit: i64) -> std::result::Result<Self, OrganizationInputError> {
        if after < 0 || !(1..=100).contains(&limit) {
            return Err(OrganizationInputError::Page);
        }
        Ok(Self { after, limit })
    }
}
pub(crate) fn administers(role: OrgRole) -> bool {
    matches!(role, OrgRole::Owner | OrgRole::Admin)
}
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
pub mod deletion;
pub mod invitations;
pub mod membership;
pub mod profiles;
pub mod recovery;
pub mod teams;

pub use access::*;
pub use deletion::*;
pub use invitations::*;
pub use membership::*;
pub use profiles::*;
pub use recovery::*;
pub use teams::*;
