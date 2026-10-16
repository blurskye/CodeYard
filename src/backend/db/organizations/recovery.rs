use super::*;
use crate::backend::db::{models::OrgMembership, DbPool};
use anyhow::Result;

// Audited exceptional promotion of an existing active member only when no usable
// owner remains. Original restrictions persist; this is not implicit tenant access.
native_operation!(crate::backend::db::postgres::organizations::recovery::recover_suspended_organization_owner, crate::backend::db::sqlite::organizations::recovery::recover_suspended_organization_owner,
    fn recover_suspended_organization_owner(pool: &DbPool, actor: i64, org: i64, target: i64) -> Result<OrganizationOutcome<OrgMembership>>);
// Organizations with no usable owner, for the site administrator who recovers one.
native_operation!(crate::backend::db::postgres::organizations::recovery::list_ownerless_organizations, crate::backend::db::sqlite::organizations::recovery::list_ownerless_organizations,
    fn list_ownerless_organizations(pool: &DbPool, actor: i64) -> Result<OrganizationOutcome<Vec<OrganizationMetadata>>>);
