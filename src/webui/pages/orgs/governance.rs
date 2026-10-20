use super::{
    fields::{choice, controls_as, field_value, hidden_controls, input, select},
    Organization, Page,
};
use crate::webui::components::confirm_dialog;
use dioxus::prelude::*;

pub(super) fn suspension(org: &Organization) -> Element {
    rsx! { if org.suspended {
        div { role: "status", class: "org-alert",
            strong { "Organization changes are paused" }
            p { "This organization needs an active owner before members, teams, or profile settings can be changed." }
        }
    } }
}

/// The owner's delete card. It names the repositories deleting the organization also deletes. Its
/// dialog opens by its fragment, since organization pages have no disclosure widgets.
pub(super) fn deletion(page: &Page, target: &str) -> Element {
    let count = page.owned_repositories;
    let noun = if count == 1 {
        "repository"
    } else {
        "repositories"
    };
    let refused = page
        .draft
        .get("action")
        .is_some_and(|v| v == "delete-organization");
    rsx! {
        section { class: "org-danger", aria_labelledby: "organization-delete-title",
            div { class: "org-danger-header",
                h3 { id: "organization-delete-title", "Delete this organization" }
                p { "Deleting the organization removes its teams and memberships." }
                if count > 0 {
                    p { strong { "It also permanently deletes its {count} {noun} and everything in them." } }
                }
            }
            div { class: "org-danger-action",
                if refused {
                    if let Some(message) = &page.message {
                        div { role: "alert", class: "repo-alert repo-alert-error", p { "{message}" } }
                    }
                }
                a { class: "btn btn-destructive", href: "#organization-delete-dialog", "Delete organization" }
                {confirm_dialog(Some("organization-delete-dialog"), "Delete this organization", target, target, rsx! {
                    div { class: "org-form",
                        strong { "Delete this organization?" }
                        if count > 0 {
                            p { "It also permanently deletes its {count} {noun} and everything in them." }
                        }
                        {input(page, "delete-organization", "confirm", "Type the organization name to confirm")}
                        {hidden_controls(page, "delete-organization")}
                    }
                }, "Delete organization")}
            }
        }
    }
}

/// Site administrators' recovery, a visible danger card. It appears only when an organization has no owner.
pub(super) fn recovery(page: &Page) -> Element {
    let options = std::iter::once(choice("", "Choose an organization"))
        .chain(
            page.ownerless_organizations
                .iter()
                .map(|org| choice(&org.id.to_string(), &org.name)),
        )
        .collect();
    rsx! { if page.can_recover && !page.ownerless_organizations.is_empty() {
        section { class: "org-danger", aria_labelledby: "organization-recovery-title",
            div { class: "org-danger-header",
                h3 { id: "organization-recovery-title", "Recover an organization without an owner" }
                p { "Site administrators can appoint an existing member when an organization has no active owner." }
            }
            if let Some(href) = &page.ownerless_next {
                a { href: href.clone(), class: "org-pagination", "Next ownerless organizations" }
            }
            form { method: "post", action: "/admin/organizations/recovery", class: "org-form org-danger-form",
                {select("Organization", "org_id", field_value(page, "recover-owner", "org_id"), options)}
                {input(page, "recover-owner", "username", "Username of the new owner")}
                {input(page, "recover-owner", "confirm", "Type the organization name to confirm")}
                {controls_as(page, "recover-owner", "Restore owner", "btn btn-destructive")}
            }
        }
    } }
}
