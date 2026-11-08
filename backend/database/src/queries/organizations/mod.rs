use crate::models::OrgRole;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrganizationOutcome<T> {
    Missing,
    Denied,
    Conflict,
    Suspended,
    Applied(T),
}

impl<T> OrganizationOutcome<T> {
    pub fn map<U>(self, project: impl FnOnce(T) -> U) -> OrganizationOutcome<U> {
        match self {
            Self::Missing => OrganizationOutcome::Missing,
            Self::Denied => OrganizationOutcome::Denied,
            Self::Conflict => OrganizationOutcome::Conflict,
            Self::Suspended => OrganizationOutcome::Suspended,
            Self::Applied(value) => OrganizationOutcome::Applied(project(value)),
        }
    }
}

pub fn administers(role: OrgRole) -> bool {
    matches!(role, OrgRole::Owner | OrgRole::Admin)
}

pub mod properties;

pub mod roles;
