/// Shared by the legacy security-team API and the organization-role API.
pub const SECURITY_MANAGER_ROLE_ID: i64 = 6;

/// Read projection shared by feature listings and locked security administration.
pub fn security_manager_condition(actor: &str, org: &str) -> String {
    security_role_sql(actor, org, true)
}

/// The role alone, without the live owner that writes need, so reads keep working while
/// organization writes are suspended.
pub(crate) fn security_role_condition(actor: &str, org: &str) -> String {
    security_role_sql(actor, org, false)
}

fn security_role_sql(actor: &str, org: &str, operational: bool) -> String {
    let owner = if operational {
        " AND EXISTS(SELECT 1 FROM org_memberships owner JOIN users active ON active.id=owner.user_id WHERE owner.org_id=security_org.id AND owner.role='owner' AND NOT active.is_banned AND NOT active.must_change_password)"
    } else {
        ""
    };
    format!("EXISTS(SELECT 1 FROM organizations security_org JOIN org_memberships security_member ON security_member.org_id=security_org.id JOIN users security_user ON security_user.id=security_member.user_id WHERE security_org.id={org} AND security_user.id={actor} AND security_member.role IN ('owner','admin','member') AND NOT security_user.is_banned AND NOT security_user.must_change_password AND NOT EXISTS(SELECT 1 FROM organization_blocks blocked WHERE blocked.org_id=security_org.id AND blocked.user_id=security_user.id){owner} AND (security_member.role='owner' OR EXISTS(SELECT 1 FROM organization_role_assignments assignment JOIN organization_roles role ON role.id=assignment.role_id AND role.base_level=1 WHERE assignment.org_id=security_org.id AND assignment.role_id={SECURITY_MANAGER_ROLE_ID} AND ((assignment.user_id=security_user.id AND assignment.team_id IS NULL) OR (assignment.user_id IS NULL AND EXISTS(SELECT 1 FROM teams security_team JOIN team_ancestors lineage ON lineage.ancestor_id=security_team.id JOIN team_memberships team_member ON team_member.team_id=lineage.team_id WHERE security_team.id=assignment.team_id AND security_team.org_id=assignment.org_id AND security_team.privacy IN ('visible','secret') AND team_member.user_id=security_user.id AND team_member.role IN ('member','maintainer')))))))")
}
