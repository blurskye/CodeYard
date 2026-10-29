//! Recipient-facing repository invitations; pending invitations are not repository access.
use crate::webui::{
    components::compact_date,
    navigation::NavLink,
    pages::{fields::csrf, settings::settings_nav},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Invitation {
    pub id: i64,
    pub owner: String,
    pub repository: String,
    pub inviter: String,
    pub access: String,
    pub created: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub csrf_token: String,
    pub invitations: Vec<Invitation>,
    pub page: usize,
    pub has_more: bool,
}
pub fn render(page: &Page) -> Element {
    rsx! {
        div {class:"identity-settings page-shell",
            header {class:"identity-settings-header",h1 {"Account settings"} p {"Manage your repository invitations."}}
            div {class:"identity-settings-layout",
                {settings_nav("invitations")}
                div {class:"identity-settings-content",
                    section {class:"identity-settings-section",
                        header {class:"identity-section-header",h2 {"Repository invitations"} p {"Accept to receive the invited permission. Pending invitations grant no access and expire after seven days."}}
                        if page.invitations.is_empty() {p {class:"repo-list-empty","No pending invitations."}}
                        for invitation in &page.invitations {
                            div {class:"repo-settings-row",key:"{invitation.id}",
                                div {class:"repo-settings-row-main",
                                    strong {"{invitation.owner}/{invitation.repository}"}
                                    p {"{invitation.inviter} invited you with {invitation.access} access · " {compact_date(&invitation.created)}}
                                }
                                form {method:"post",action:format!("/settings/invitations/{}/accept",invitation.id),
                                    {csrf(&page.csrf_token)}
                                    button {r#type:"submit",class:"btn btn-primary","Accept"}
                                    button {r#type:"submit",class:"btn btn-secondary",formaction:format!("/settings/invitations/{}/decline",invitation.id),"Decline"}
                                }
                            }
                        }
                        nav {class:"repo-pagination",aria_label:"Invitation pages",
                            if page.page>1 {NavLink {href:format!("/settings/invitations?page={}",page.page-1),class:"btn btn-secondary","Previous"}}
                            if page.has_more {NavLink {href:format!("/settings/invitations?page={}",page.page+1),class:"btn btn-secondary","Next"}}
                        }
                    }
                }
            }
        }
    }
}
