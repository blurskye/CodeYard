use crate::organizations::profiles::validate_description;
use crate::organizations::profiles::validate_name;
use crate::organizations::profiles::validate_slug;
use crate::organizations::profiles::NewOrganization;
use crate::organizations::profiles::OrganizationMetadata;
use crate::organizations::profiles::OrganizationProfile;
use crate::organizations::OrganizationInputError;
use crate::organizations::OrganizationPage;
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::DbPool;
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

native_operation!(crate::postgres::organizations::profiles::create_organization, crate::sqlite::organizations::profiles::create_organization,
    fn create_organization(pool: &DbPool, actor: i64, input: &NewOrganization) -> Result<OrganizationOutcome<OrganizationMetadata>>);
native_operation!(crate::postgres::organizations::profiles::update_organization_profile, crate::sqlite::organizations::profiles::update_organization_profile,
    fn update_organization_profile(pool: &DbPool, actor: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, org: i64, input: &OrganizationProfile) -> Result<OrganizationOutcome<OrganizationMetadata>>);
// The picture is a file id. Setting one needs an owner or admin, and the file has to
// be the actor's own confirmed `org_avatar` upload (`Conflict` otherwise); `None`
// removes the picture. The answer is the picture it replaced, if any.
native_operation!(crate::postgres::organizations::profiles::set_organization_avatar, crate::sqlite::organizations::profiles::set_organization_avatar,
    fn set_organization_avatar(pool: &DbPool, actor: i64, org: i64, file: Option<Uuid>) -> Result<OrganizationOutcome<Option<Uuid>>>);
native_operation!(crate::postgres::organizations::profiles::get_organization_avatar, crate::sqlite::organizations::profiles::get_organization_avatar,
    fn get_organization_avatar(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<Option<Uuid>>>);
// Public pages need no actor. A missing or governance-suspended organization is `None`.
native_operation!(crate::postgres::organizations::profiles::get_public_organization, crate::sqlite::organizations::profiles::get_public_organization,
    fn get_public_organization(pool: &DbPool, name: &str) -> Result<Option<codeyard_db_core::models::Organization>>);
native_operation!(crate::postgres::organizations::profiles::get_public_organization_by_id, crate::sqlite::organizations::profiles::get_public_organization_by_id,
    fn get_public_organization_by_id(pool: &DbPool, id: i64) -> Result<Option<codeyard_db_core::models::Organization>>);
native_operation!(crate::postgres::organizations::profiles::get_organization_metadata, crate::sqlite::organizations::profiles::get_organization_metadata,
    fn get_organization_metadata(pool: &DbPool, actor: i64, org: i64) -> Result<OrganizationOutcome<OrganizationMetadata>>);
native_operation!(crate::postgres::organizations::profiles::list_actor_organizations, crate::sqlite::organizations::profiles::list_actor_organizations,
    fn list_actor_organizations(pool: &DbPool, actor: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>>);
// Presentation choices only: native creation must recheck actor, role and governance.
native_operation!(crate::postgres::organizations::profiles::list_repository_creation_organizations, crate::sqlite::organizations::profiles::list_repository_creation_organizations,
    fn list_repository_creation_organizations(pool: &DbPool, actor: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>>);

// Exact, case-sensitive namespace, matching the equality and UNIQUE rules of users.username.
// Do not lowercase identities or conflate routing namespaces.
pub(super) fn validate_slug(value: &str) -> std::result::Result<(), OrganizationInputError> {
    codeyard_model::account_input::validate_namespace(value, 3)
        .map_err(|_| OrganizationInputError::Slug)
}
pub(super) fn validate_name(value: &str) -> std::result::Result<(), OrganizationInputError> {
    if value.trim().is_empty()
        || value.len() > 1024
        || value.chars().count() > 255
        || value.chars().any(char::is_control)
    {
        return Err(OrganizationInputError::Name);
    }
    Ok(())
}
pub(super) fn validate_description(
    value: Option<&str>,
) -> std::result::Result<(), OrganizationInputError> {
    if value.is_some_and(|s| {
        s.len() > 4096
            || s.chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
    }) {
        return Err(OrganizationInputError::Description);
    }
    Ok(())
}

/// Each field is `None` to leave the stored value alone, `Some(None)` to clear it, or
/// `Some(Some(value))` to set it.
#[derive(Debug, Clone)]
pub struct OrganizationProfile {
    pub display_name: Option<Option<String>>,
    pub description: Option<Option<String>>,
}
impl OrganizationProfile {
    /// Sets both fields. `None` stores no value, as creating an organization needs.
    pub fn new(
        display_name: Option<&str>,
        description: Option<&str>,
    ) -> std::result::Result<Self, OrganizationInputError> {
        Self::partial(Some(display_name), Some(description))
    }

    /// An update that may leave either field as it is stored.
    pub fn partial(
        display_name: Option<Option<&str>>,
        description: Option<Option<&str>>,
    ) -> std::result::Result<Self, OrganizationInputError> {
        if let Some(Some(name)) = display_name {
            validate_name(name)?;
        }
        if let Some(description) = description {
            validate_description(description)?;
        }
        Ok(Self {
            display_name: display_name.map(|value| value.map(str::to_owned)),
            description: description.map(|value| value.map(str::to_owned)),
        })
    }
}
#[derive(Debug, Clone)]
pub struct NewOrganization {
    pub slug: String,
    pub profile: OrganizationProfile,
}
impl NewOrganization {
    pub fn new(
        slug: &str,
        profile: OrganizationProfile,
    ) -> std::result::Result<Self, OrganizationInputError> {
        validate_slug(slug)?;
        // Organization slugs are top-level routes. Team slugs are scoped to their organization.
        if codeyard_db_identity::users::is_reserved_name(slug) {
            return Err(OrganizationInputError::Slug);
        }
        Ok(Self {
            slug: slug.to_owned(),
            profile,
        })
    }
}
/// Deliberately excludes billing_email and private storage/credential fields.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct OrganizationMetadata {
    pub id: i64,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub is_verified: bool,
    /// Derived from usable persisted owners; bans/reset restrictions are not exempted.
    pub governance_suspended: bool,
    pub avatar_file_id: Option<Uuid>,
}
