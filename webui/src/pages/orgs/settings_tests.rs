use super::*;
fn page(values: HashMap<String, String>) -> Page {
    let mut page = sample_page(values, vec![]);
    page.tab = "settings".into();
    page.settings = Some(Box::new(SettingsPanel {
        company: "Acme <tools>".into(),
        email: "public@example.test".into(),
        billing_email: "private@example.test".into(),
        twitter_username: "acme".into(),
        location: "Montréal".into(),
        blog: "https://example.test/".into(),
        default_repository_permission: "write".into(),
        has_organization_projects: true,
        members_can_create_public_repositories: true,
        members_can_create_private_repositories: false,
        members_can_fork_private_repositories: true,
        web_commit_signoff_required: false,
        deploy_keys_enabled_for_repositories: true,
    }));
    page
}
#[test]
fn organization_settings_render_selected_policies_and_private_owner_controls() {
    let rendered = html(page(HashMap::new()));
    assert!(rendered.contains("/settings/organizations/3/settings"));
    assert!(rendered.contains("csrf-fixture"));
    assert!(rendered.contains("private@example.test"));
    assert!(!rendered.contains("Acme <tools>"));
    assert!(selected(
        selects(&rendered, "default_repository_permission")[0],
        "write"
    ));
    assert!(rendered.contains("save-organization-contacts"));
    assert!(rendered.contains("save-organization-security-policy"));
}
#[test]
fn settings_forms_retain_drafts_and_unchecked_values_without_changing_other_forms() {
    let rendered = html(page(draft(&[
        ("action", "save-organization-repository-policy"),
        ("default_repository_permission", "read"),
        ("members_can_create_private_repositories", "true"),
    ])));
    assert!(selected(
        selects(&rendered, "default_repository_permission")[0],
        "read"
    ));
    let public = rendered
        .split("name=\"members_can_create_public_repositories\"")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    assert!(!public.contains("checked"));
    let private = rendered
        .split("name=\"members_can_create_private_repositories\"")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    assert!(private.contains("checked"));
    assert!(
        rendered.contains("private@example.test"),
        "an unrelated contact form keeps its stored values"
    );
    let rendered = html(page(draft(&[
        ("action", "save-organization-contacts"),
        ("company", "Rejected <b>draft</b>"),
        ("email", "invalid"),
    ])));
    assert!(rendered.contains("Rejected &#60;b&#62;draft&#60;/b&#62;"));
    assert!(selected(
        selects(&rendered, "default_repository_permission")[0],
        "write"
    ));
}
#[test]
fn organization_settings_do_not_render_owner_controls_for_members_or_suspension() {
    let mut member = page(HashMap::new());
    member.is_owner = false;
    member.can_manage = false;
    let rendered = html(member);
    assert!(!rendered.contains("private@example.test"));
    assert!(!rendered.contains("save-organization-contacts"));
    let mut suspended = page(HashMap::new());
    suspended.selected.as_mut().unwrap().suspended = true;
    let rendered = html(suspended);
    assert!(rendered.contains("private@example.test"));
    assert!(!rendered.contains("save-organization-contacts"));
    assert!(!rendered.contains("save-organization-security-policy"));
}
#[test]
fn public_contacts_and_member_creation_do_not_confer_governance_or_unsafe_links() {
    let mut page = public_profile(false, Some(1), vec![]);
    page.can_create = true;
    page.contacts = ProfileContacts {
        company: "Company <script>alert(1)</script>".into(),
        email: "public@example.test".into(),
        location: "Montréal".into(),
        blog: "javascript:alert(1)".into(),
        twitter_username: "acme".into(),
    };
    let mut dom = VirtualDom::new_with_props(profile_page, page);
    dom.rebuild_in_place();
    let rendered = dioxus_ssr::render(&dom);
    assert!(rendered.contains("New repository"));
    assert!(!rendered.contains("Manage organization"));
    assert!(!rendered.contains("<script>alert(1)</script>"));
    assert!(!rendered.contains("href=\"javascript:"));
    assert!(rendered.contains("public@example.test"));
}
