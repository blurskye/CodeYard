use super::{fields, Page, RoleUser};
use crate::webui::{components::SelectOption, navigation::NavLink};
use dioxus::prelude::*;

fn revoke_user(page: &Page, target: &str, user: &RoleUser, role: i64, all: bool) -> Element {
    let action = if all {
        "revoke-all-user-roles"
    } else {
        "revoke-user-role"
    };
    rsx! { form { class:"org-form",method:"post",action:target,
        input {r#type:"hidden",name:"username",value:user.username.clone()}
        if !all {input {r#type:"hidden",name:"role_id",value:role.to_string()}}
        {fields::hidden_controls(page,action)}
        label {input {r#type:"checkbox",name:"confirm",value:"true",required:true} if all {" Remove all directly assigned roles"}else{" Remove this directly assigned role"}}
        button {class:"btn btn-danger",r#type:"submit",if all {"Remove all direct roles"}else{"Remove direct role"}}
    }}
}
pub(super) fn panel(page: &Page) -> Element {
    if !page.is_owner {
        return rsx! {};
    }
    let (Some(org), Some(panel)) = (&page.selected, &page.roles) else {
        return rsx! {};
    };
    let Some(selected) = panel
        .definitions
        .iter()
        .find(|role| role.id == panel.selected)
    else {
        return rsx! {};
    };
    let base = format!("/settings/organizations/{}", org.id);
    let target = format!("{base}?tab=roles&role={}", selected.id);
    rsx! { section {class:"org-section",aria_labelledby:"organization-roles-title",
        h2 {id:"organization-roles-title","Organization roles"}
        p {class:"org-note","These predefined roles apply to every current and future repository. Security managers additionally manage repository security; no role confers organization ownership."}
        form {class:"org-form",method:"get",action:base,
            input {r#type:"hidden",name:"tab",value:"roles"}
            {fields::select("Organization role","role",selected.id.to_string(),panel.definitions.iter().map(|role|SelectOption {value:role.id.to_string(),label:role.name.clone()}).collect())}
            button {class:"btn",r#type:"submit","View role"}
        }
        h3 {"{selected.name}"}
        p {"{selected.description}"}
        if !org.suspended {
            form {class:"org-form",method:"post",action:target.clone(),
                {fields::input(page,"assign-user-role","username","Organization member username")}
                input {r#type:"hidden",name:"role_id",value:selected.id.to_string()}
                {fields::controls(page,"assign-user-role","Assign to member")}
            }
            form {class:"org-form",method:"post",action:target.clone(),
                {fields::input(page,"assign-team-role","team_slug","Team handle")}
                input {r#type:"hidden",name:"role_id",value:selected.id.to_string()}
                {fields::controls(page,"assign-team-role","Assign to team")}
            }
        }
        p {class:"org-note","Removing a direct user role does not remove roles inherited through teams. Removing a team role affects every team member. Role revocation cancels affected organization jobs."}
        h3 {"Members with this role"}
        ul {class:"org-list",
            if panel.users.is_empty() {li {class:"org-empty","No members on this page."}}
            for user in &panel.users {
                li {class:"org-row",key:"{user.id}",
                    div {class:"org-row-main",
                        h3 {NavLink {class:"org-row-link",href:crate::webui::profile_path(&user.username),"{user.username}"}}
                        if user.direct {p {"Directly assigned"}}
                        for team in &user.teams {p {key:"{team.id}","Inherited from " NavLink {href:format!("/settings/organizations/{}?tab=teams&team={}",org.id,team.id),"{team.name}"}}}
                    }
                    if user.direct && !org.suspended {div {class:"org-row-actions",
                        {revoke_user(page,&target,user,selected.id,false)}
                        {revoke_user(page,&target,user,selected.id,true)}
                    }}
                }
            }
        }
        if let Some(next)=&panel.users_next {NavLink {class:"org-pagination",href:next.clone(),"Next role members"}}
        h3 {"Teams assigned this role"}
        ul {class:"org-list",
            if panel.teams.is_empty() {li {class:"org-empty","No teams on this page."}}
            for team in &panel.teams {
                li {class:"org-row",key:"{team.id}",
                    div {class:"org-row-main",
                        h3 {NavLink {class:"org-row-link",href:format!("/settings/organizations/{}?tab=teams&team={}",org.id,team.id),"{team.name}"}}
                        p {"@{team.slug}"}
                    }
                    if !org.suspended {div {class:"org-row-actions",
                        for all in [false,true] {form {class:"org-form",method:"post",action:target.clone(),key:"{all}",
                            input {r#type:"hidden",name:"team_id",value:team.id.to_string()}
                            if !all {input {r#type:"hidden",name:"role_id",value:selected.id.to_string()}}
                            {fields::hidden_controls(page,if all {"revoke-all-team-roles"}else{"revoke-team-role"})}
                            label {input {r#type:"checkbox",name:"confirm",value:"true",required:true} if all {" Remove all team role assignments"}else{" Remove this team role assignment"}}
                            button {class:"btn btn-danger",r#type:"submit",if all {"Remove all team roles"}else{"Remove team role"}}
                        }}
                    }}
                }
            }
        }
        if let Some(next)=&panel.teams_next {NavLink {class:"org-pagination",href:next.clone(),"Next role teams"}}
    }}
}
