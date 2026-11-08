use crate::organizations::profiles::OrganizationMetadata;
use crate::organizations::OrganizationPage;
use anyhow::Result;
use codeyard_database::models::OrgMembership;
use codeyard_database::native_operation;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::DbPool;

// Audited exceptional promotion of an existing active member only when no usable
// owner remains. Original restrictions persist; this is not implicit tenant access.
native_operation!(crate::postgres::organizations::recovery::recover_suspended_organization_owner, crate::sqlite::organizations::recovery::recover_suspended_organization_owner,
    fn recover_suspended_organization_owner(pool: &DbPool, actor: impl Into<codeyard_db_identity::users::transitions::AdministratorActor>, org: i64, target: i64) -> Result<OrganizationOutcome<OrgMembership>>);
// Organizations with no usable owner, for the site administrator who recovers one.
native_operation!(crate::postgres::organizations::recovery::list_ownerless_organizations, crate::sqlite::organizations::recovery::list_ownerless_organizations,
    fn list_ownerless_organizations(pool: &DbPool, actor: i64, page: OrganizationPage) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>>);
