use super::{fields, Page};
use crate::webui::components::{DraftCheckbox, DraftInput, DraftSelect, SelectOption};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsPanel {
    pub company: String,
    pub email: String,
    pub twitter_username: String,
    pub location: String,
    pub blog: String,
    pub billing_email: String,
    pub default_repository_permission: String,
    pub has_organization_projects: bool,
    pub members_can_create_public_repositories: bool,
    pub members_can_create_private_repositories: bool,
    pub members_can_fork_private_repositories: bool,
    pub web_commit_signoff_required: bool,
    pub deploy_keys_enabled_for_repositories: bool,
}
fn value(page: &Page, action: &str, key: &str, current: &str) -> String {
    if page
        .draft
        .get("action")
        .is_some_and(|value| value == action)
    {
        page.draft.get(key).cloned().unwrap_or_default()
    } else {
        current.into()
    }
}
fn checked(page: &Page, action: &str, key: &str, current: bool) -> bool {
    if page
        .draft
        .get("action")
        .is_some_and(|value| value == action)
    {
        page.draft.get(key).is_some_and(|value| value == "true")
    } else {
        current
    }
}
fn contact(page: &Page, key: &str, label: &str, current: &str) -> Element {
    let id = format!("organization-setting-{key}");
    rsx! {label {r#for:id.clone(),"{label}"}
        DraftInput {id:Some(id.clone()),name:key.to_owned(),initial_value:value(page,"save-organization-contacts",key,current),
            input_type:if matches!(key,"email"|"billing_email") {"email".to_owned()} else {"text".to_owned()},
            maxlength:Some(match key {"email"|"billing_email"=>254,"twitter_username"=>15,_=>1024})}
    }
}
fn checkbox(page: &Page, action: &str, key: &str, label: &str, current: bool) -> Element {
    rsx! {label {class:"org-confirm",DraftCheckbox {name:key.to_owned(),submission_value:"true",initial_checked:checked(page,action,key,current)} span {"{label}"}}}
}
pub(super) fn panel(page: &Page) -> Element {
    if !page.is_owner {
        return rsx! {};
    }
    let (Some(org), Some(settings)) = (&page.selected, &page.settings) else {
        return rsx! {};
    };
    let target = format!("/settings/organizations/{}/settings", org.id);
    rsx! {section {class:"org-section",aria_labelledby:"organization-settings-title",
        h2 {id:"organization-settings-title","Organization settings"}
        p {class:"org-note","Only organization owners can change these policies. Repository roles do not grant organization ownership."}
        if org.suspended {p {class:"org-note","Changes are suspended until an active owner is available."}}
        section {class:"org-panel",header {class:"org-panel-header",h3 {"Contact details"}}
            div {class:"org-panel-body",
                if org.suspended {dl {
                    dt {"Company"} dd {"{settings.company}"} dt {"Public email"} dd {"{settings.email}"}
                    dt {"Location"} dd {"{settings.location}"} dt {"Website"} dd {"{settings.blog}"}
                    dt {"Twitter username"} dd {"{settings.twitter_username}"} dt {"Private billing contact"} dd {"{settings.billing_email}"}
                }} else {
                    form {key:"{org.id}-contacts",class:"org-form",method:"post",action:target.clone(),
                        {fields::hidden_controls(page,"save-organization-contacts")}
                        {contact(page,"company","Company",&settings.company)}
                        {contact(page,"email","Public email",&settings.email)}
                        {contact(page,"location","Location",&settings.location)}
                        {contact(page,"blog","Website (HTTP or HTTPS)",&settings.blog)}
                        {contact(page,"twitter_username","Twitter username (without @)",&settings.twitter_username)}
                        {contact(page,"billing_email","Private billing contact",&settings.billing_email)}
                        p {class:"org-field-help","Only the billing contact is private. It is visible to organization owners, never on the public profile."}
                        button {class:"btn btn-primary",r#type:"submit","Save contact details"}
                    }
                }
            }
        }
        section {class:"org-panel",header {class:"org-panel-header",h3 {"Repository and project policies"}}
            div {class:"org-panel-body",
                p {class:"org-note","Default access applies to every current and future repository. Explicit repository and team grants remain independent. No repository permission grants organization governance."}
                if org.suspended {
                    p {"Default repository permission: {settings.default_repository_permission}"}
                } else {form {key:"{org.id}-repository-policy",class:"org-form",method:"post",action:target.clone(),
                    {fields::hidden_controls(page,"save-organization-repository-policy")}
                    label {r#for:"organization-default-repository-permission","Default member repository permission"}
                    DraftSelect {id:Some("organization-default-repository-permission".into()),name:"default_repository_permission",
                        initial_value:value(page,"save-organization-repository-policy","default_repository_permission",&settings.default_repository_permission),
                        options:[("none","None"),("read","Read"),("write","Write"),("admin","Repository administrator")].into_iter().map(|(value,label)|SelectOption::new(value,label)).collect::<Vec<_>>()}
                    {checkbox(page,"save-organization-repository-policy","members_can_create_public_repositories","Allow members to create public repositories",settings.members_can_create_public_repositories)}
                    {checkbox(page,"save-organization-repository-policy","members_can_create_private_repositories","Allow members to create private repositories",settings.members_can_create_private_repositories)}
                    {checkbox(page,"save-organization-repository-policy","members_can_fork_private_repositories","Allow private repositories to be forked",settings.members_can_fork_private_repositories)}
                    {checkbox(page,"save-organization-repository-policy","has_organization_projects","Enable organization projects",settings.has_organization_projects)}
                    p {class:"org-field-help","Member-created repositories grant their creator repository administrator access. Disabling projects retains their data. Owners and delegated organization administrators can still create repositories and fork private repositories."}
                    button {class:"btn btn-primary",r#type:"submit","Save repository policies"}
                }}
            }
        }
        section {class:"org-panel",header {class:"org-panel-header",h3 {"Web commits and deploy keys"}}
            div {class:"org-panel-body",
                if !org.suspended {form {key:"{org.id}-security-policy",class:"org-form",method:"post",action:target.clone(),
                    {fields::hidden_controls(page,"save-organization-security-policy")}
                    {checkbox(page,"save-organization-security-policy","web_commit_signoff_required","Require sign-off for native web file commits",settings.web_commit_signoff_required)}
                    p {class:"org-field-help","Contributors must add a final Signed-off-by: username <commit email> trailer to the editor's extended description, upload description, or deletion message. Sign-offs are explicit declarations, never added automatically. This policy does not change Git or REST API commits."}
                    {checkbox(page,"save-organization-security-policy","deploy_keys_enabled_for_repositories","Allow deploy keys to be added and used",settings.deploy_keys_enabled_for_repositories)}
                    p {class:"org-field-help","Disabling deploy keys prevents new enrollments and authentication for every organization repository. Existing keys remain available to list or remove, and become usable again if reenabled."}
                    button {class:"btn btn-primary",r#type:"submit","Save commit and key policies"}
                }} else {p {"Web sign-off required: {settings.web_commit_signoff_required}. Deploy keys enabled: {settings.deploy_keys_enabled_for_repositories}."}}
            }
        }
    }}
}
